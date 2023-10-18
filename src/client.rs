use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::{select, task};
use tracing::{debug, error, info, Instrument};

use crate::instance::{self, Instance, InstanceKey, InstanceMap};
use crate::lsp::jsonrpc::{Message, RequestId, ResponseSuccess, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::InitializeParams;

/// Finds or spawns a language server instance and connects the client
pub async fn process(
    socket: TcpStream,
    port: u16,
    instance_map: Arc<Mutex<InstanceMap>>,
) -> Result<()> {
    let (socket_read, socket_write) = socket.into_split();
    let mut reader = LspReader::new(BufReader::new(socket_read), "client");
    let mut writer = LspWriter::new(socket_write, "client");

    // Read the first client message, this must be `initialize` request.
    let req = match reader
        .read_message()
        .await
        .context("receive `initialize` request")?
        .context("channel closed")?
    {
        Message::Request(req) if req.method == "initialize" => req,
        _ => bail!("first client message was not `initialize` request"),
    };
    let mut init_params = serde_json::from_value::<InitializeParams>(req.params)
        .context("parse `initialize` request params")?;

    // Remove `lspMux` from `initializationOptions`, it's ra-multiplex extension
    // and we don't want to forward it to the real language server.
    let options = init_params
        .initialization_options
        .as_mut()
        .context("missing `initializationOptions` in `initialize` request")?
        .lsp_mux
        .take()
        .context("missing `lspMux` in `initializationOptions` in `initialize` request")?;
    // TODO verify protocol version

    // Select the workspace root directory.
    //
    // Ideally we'd be looking up any server which has a superset of workspace
    // folders active possibly adding transforming the `initialize` request into
    // a few requests for adding workspace folders if the server supports it.
    // Buuut let's just run with supporting single-folder workspaces only at
    // first, it's probably the most common use-case anyway.
    let folders = &init_params.workspace_folders;
    ensure!(
        folders.len() == 1,
        "workspace must have exactly 1 folder, has {n}.\n{folders:#?}",
        n = folders.len(),
    );
    let folder = init_params.workspace_folders[0].clone();

    // Get an language server instance for this client.
    let key = InstanceKey {
        server: options.server,
        args: options.args,
        workspace_root: folder.uri,
    };
    let instance = instance::get_or_spawn(instance_map, key, init_params).await?;

    // Respond to client's `initialize` request using a response result from
    // the first time this server instance was initialized, it might not be a
    // response directly to our previous request but it should be similar since
    let res = ResponseSuccess {
        jsonrpc: Version,
        result: serde_json::to_value(instance.initialize_result()).unwrap(),
        id: req.id,
    };
    writer
        .write_message(&res.into())
        .await
        .context("send `initialize` request response")?;

    // Wait for the client to send `initialized` notification. We don't want to
    // forward it since the server only expects one and we already sent a fake
    // one during the server handshake.
    match reader
        .read_message()
        .await
        .context("receive `initialized` notification")?
        .context("channel closed")?
    {
        Message::Notification(notif) if notif.method == "initialized" => {
            // Discard the notification.
        }
        _ => bail!("second client message was not `initialized` notification"),
    }
    info!("initialized client");

    let (client_tx, client_rx) = mpsc::channel(64);
    let (close_tx, close_rx) = mpsc::channel(1);
    task::spawn(input_task(client_rx, close_rx, writer).in_current_span());
    instance.add_client(port, client_tx.clone()).await;

    task::spawn(output_task(reader, port, instance, close_tx).in_current_span());

    Ok(())
}

/// Receives messages from a channel and writes tem to the client input socket
async fn input_task(
    mut rx: mpsc::Receiver<Message>,
    mut close_rx: mpsc::Receiver<Message>,
    mut writer: LspWriter<OwnedWriteHalf>,
) {
    // Unlike the output task, here we first wait on the channel which is going to
    // block until the language server sends a notification, however if we're the last
    // client and have just closed the server is unlikely to send any. This results in the
    // last client often falsely hanging while the gc task depends on the input channels being
    // closed to detect a disconnected client.
    //
    // When a client sends a shutdown request we receive a message on the `close_rx`, send
    // the reply and close the connection. If no shutdown request was received but the
    // client closed `close_rx` channel will be dropped (unlike the normal rx channel which
    // is shared) and the connection will close without sending any response.
    while let Some(message) = select! {
        message = close_rx.recv() => message,
        message = rx.recv() => message,
    } {
        if let Err(err) = writer.write_message(&message).await {
            match err.kind() {
                // ignore benign errors, treat as socket close
                ErrorKind::BrokenPipe => {}
                // report fatal errors
                _ => error!(?err, "error writing client input: {err}"),
            }
            break; // break on any error
        }
    }
    debug!("client input closed");
    info!("client disconnected");
}

fn tag_id(port: u16, id: &RequestId) -> RequestId {
    RequestId::String(match id {
        RequestId::Number(number) => format!("{port}:n:{number}"),
        RequestId::String(string) => format!("{port}:s:{string}"),
    })
}

/// Reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn output_task(
    mut reader: LspReader<BufReader<OwnedReadHalf>>,
    port: u16,
    instance: Arc<Instance>,
    close_tx: mpsc::Sender<Message>,
) {
    loop {
        let message = match reader.read_message().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("client output closed");
                break;
            }
            Err(err) => {
                error!(?err, "error reading client output");
                continue;
            }
        };

        match message {
            Message::Request(req) if req.method == "shutdown" => {
                // Client requested the server to shut down but other clients might still be connected.
                // Instead we disconnect this client to prevent the editor hanging
                // see <https://github.com/pr2502/ra-multiplex/issues/5>.
                info!("client sent shutdown request, sending a response and closing connection");
                // <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#shutdown>
                let message = Message::ResponseSuccess(ResponseSuccess {
                    jsonrpc: Version,
                    result: Value::Null,
                    id: req.id,
                });
                // Ignoring error because we would've closed the connection regardless
                let _ = close_tx.send(message).await;
                break;
            }

            Message::Request(mut req) => {
                req.id = tag_id(port, &req.id);
                if instance.send_message(req.into()).await.is_err() {
                    break;
                }
                instance.keep_alive();
            }

            Message::ResponseSuccess(_) | Message::ResponseError(_) => {
                debug!(?message, "client response");
            }

            Message::Notification(notif) => {
                if instance.send_message(notif.into()).await.is_err() {
                    break;
                }
                instance.keep_alive();
            }
        }
    }
}

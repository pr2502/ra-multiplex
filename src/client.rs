use std::io::ErrorKind;
use std::mem;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::{select, task};
use tracing::{debug, error, info, trace, Instrument};

use crate::instance::{self, Instance, InstanceKey, InstanceMap, INIT_REQUEST_ID};
use crate::lsp::jsonrpc::{Message, ResponseSuccess, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::proto;

/// Finds or spawns a language server instance and connects the client
pub async fn process(
    socket: TcpStream,
    port: u16,
    instance_map: Arc<Mutex<InstanceMap>>,
) -> Result<()> {
    let (socket_read, socket_write) = socket.into_split();
    let mut socket_read = BufReader::new(socket_read);

    let mut buffer = Vec::new();
    let proto_init = proto::Init::from_reader(&mut buffer, &mut socket_read).await?;

    let key = InstanceKey::from_proto_init(&proto_init).await;
    debug!(
        path = ?key.workspace_root(),
        server = ?key.server(),
        args = ?key.args(),
        "client configured",
    );

    let instance = instance::get(instance_map, &key).await?;

    let initialize_request_id = wait_for_initialize_request(&instance, &mut socket_read).await?;

    let (client_tx, client_rx) = register_client_with_instance(port, &instance).await;
    let (close_tx, close_rx) = mpsc::channel(1);
    task::spawn(input_task(client_rx, close_rx, socket_write).in_current_span());
    task::spawn(output_task(port, instance.clone(), socket_read, close_tx).in_current_span());

    wait_for_initialize_response(initialize_request_id, &instance, client_tx).await?;
    Ok(())
}

async fn wait_for_initialize_request(
    instance: &Instance,
    socket_read: &mut BufReader<OwnedReadHalf>,
) -> Result<Value> {
    let mut reader = LspReader::new(socket_read);

    let message = reader.read_message().await?.context("channel closed")?;
    trace!(?message, "<- client");

    let mut req = match message {
        Message::Request(req) if req.method == "initialize" => {
            debug!(message = ?Message::from(req.clone()), "recv InitializeRequest");
            req
        }
        _ => bail!("first client message was not InitializeRequest"),
    };

    // this is an initialize request, it's special because it cannot be sent twice or
    // rust-analyzer will crash.

    // We save the request id so we can later use it for the response and we replace it with a known value.
    let init_request_id = mem::replace(&mut req.id, Value::String(INIT_REQUEST_ID.to_owned()));

    if instance.init_cache.attempt_send_request() {
        instance
            .message_writer
            .send(req.into())
            .await
            .context("forward client request")?;
    }

    Ok(init_request_id)
}

async fn wait_for_initialize_response(
    init_request_id: Value,
    instance: &Instance,
    tx: mpsc::Sender<Message>,
) -> Result<()> {
    // Parse the cached message and restore the `id` to the value this client expects
    let mut res = instance.init_cache.response.wait().await.clone();
    res.id = init_request_id;
    debug!(message = ?Message::from(res.clone()), "send response to InitializeRequest");
    tx.send(res.into())
        .await
        .context("send initialize response")?;
    Ok(())
}

async fn register_client_with_instance(
    port: u16,
    instance: &Instance,
) -> (mpsc::Sender<Message>, mpsc::Receiver<Message>) {
    let (client_tx, client_rx) = mpsc::channel(64);
    instance
        .message_readers
        .lock()
        .await
        .insert(port, client_tx.clone());
    (client_tx, client_rx)
}

/// Receives messages from a channel and writes tem to the client input socket
async fn input_task(
    mut rx: mpsc::Receiver<Message>,
    mut close_rx: mpsc::Receiver<Message>,
    socket_write: OwnedWriteHalf,
) {
    let mut writer = LspWriter::new(socket_write);

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
        trace!(?message, "-> client");
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

fn tag_id(port: u16, id: &Value) -> Value {
    Value::String(match id {
        Value::Number(number) => format!("{port}:n:{number}"),
        Value::String(string) => format!("{port}:s:{string}"),
        _ => unreachable!("unexpected message id type {id:?}"),
    })
}

/// Reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn output_task(
    port: u16,
    instance: Arc<Instance>,
    socket_read: BufReader<OwnedReadHalf>,
    close_tx: mpsc::Sender<Message>,
) {
    let mut reader = LspReader::new(socket_read);
    let tx = instance.message_writer.clone();

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
        trace!(?message, "<- client");

        match message {
            Message::Notification(notif) if notif.method == "initialized" => {
                // Initialized notification can only be sent once per server
                if instance.init_cache.attempt_send_notif() {
                    debug!("send InitializedNotification");

                    if tx.send(notif.into()).await.is_err() {
                        break;
                    }
                    instance.keep_alive();
                } else {
                    // We're not the first, skip processing the message further
                    debug!("skip InitializedNotification");
                    continue;
                }
            }

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
                if tx.send(req.into()).await.is_err() {
                    break;
                }
                instance.keep_alive();
            }

            Message::ResponseSuccess(_) | Message::ResponseError(_) => {
                debug!(?message, "client response");
            }

            Message::Notification(notif) => {
                if tx.send(notif.into()).await.is_err() {
                    break;
                }
                instance.keep_alive();
            }
        }
    }
}

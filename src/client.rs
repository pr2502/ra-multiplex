use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{mpsc, Mutex};
use tokio::task;
use tracing::{debug, error, info, warn, Instrument};
use uriparse::URI;

use crate::instance::{self, Instance, InstanceKey, InstanceMap};
use crate::lsp::ext::{self, LspMuxOptions, Tag};
use crate::lsp::jsonrpc::{
    self, Message, Request, RequestId, ResponseError, ResponseSuccess, Version,
};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::InitializeParams;

/// Read first client message and dispatch lsp mux commands
pub async fn process(
    socket: TcpStream,
    port: u16,
    instance_map: Arc<Mutex<InstanceMap>>,
) -> Result<()> {
    let (socket_read, socket_write) = socket.into_split();
    let mut reader = LspReader::new(BufReader::new(socket_read), "client");
    let writer = LspWriter::new(socket_write, "client");

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
    let mut init_params = serde_json::from_value::<InitializeParams>(req.params.clone())
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
    ensure!(
        options.version == LspMuxOptions::PROTOCOL_VERSION,
        "unsupported protocol version {:?}, expected {:?}",
        &options.version,
        LspMuxOptions::PROTOCOL_VERSION,
    );

    debug!(?options, "lspmux initialization");
    match options.method {
        ext::Request::Connect { server, args, cwd } => {
            connect(
                port,
                instance_map,
                (server, args, cwd),
                req,
                init_params,
                reader,
                writer,
            )
            .await
        }
        ext::Request::Status {} => status(instance_map, writer).await,
        ext::Request::Reload { cwd } => reload(cwd, instance_map, writer).await,
    }
}

#[derive(Clone)]
pub struct Client {
    port: u16,
    sender: mpsc::Sender<Message>,
}

impl Client {
    fn new(port: u16) -> (Client, mpsc::Receiver<Message>) {
        let (sender, receiver) = mpsc::channel(16);
        (Client { port, sender }, receiver)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Send a message to the client channel
    pub async fn send_message(&self, message: Message) -> Result<(), SendError<Message>> {
        self.sender.send(message).await
    }
}

async fn status(
    instance_map: Arc<Mutex<InstanceMap>>,
    mut writer: LspWriter<OwnedWriteHalf>,
) -> Result<()> {
    let status = task::spawn_blocking(move || instance_map.blocking_lock().get_status())
        .await
        .unwrap();
    writer
        .write_message(&Message::ResponseSuccess(ResponseSuccess {
            jsonrpc: Version,
            result: serde_json::to_value(status).unwrap(),
            id: RequestId::Number(0),
        }))
        .await
        .context("writing response")
}

async fn reload(
    cwd: String,
    instance_map: Arc<Mutex<InstanceMap>>,
    mut writer: LspWriter<OwnedWriteHalf>,
) -> Result<()> {
    if let Some(instance) = instance_map.lock().await.get_by_cwd(&cwd) {
        instance
            .send_message(Message::Request(Request {
                jsonrpc: Version,
                method: "rust-analyzer/reloadWorkspace".into(),
                params: Value::Null,
                id: RequestId::Number(0).tag(Tag::Drop),
            }))
            .await
            .ok()
            .context("instance closed")?;

        writer
            .write_message(&Message::ResponseSuccess(ResponseSuccess::null(
                RequestId::Number(0),
            )))
            .await
            .context("writing response")?;
    } else {
        writer
            .write_message(&Message::ResponseError(ResponseError {
                jsonrpc: Version,
                error: jsonrpc::Error {
                    code: 0,
                    message: "no instance found".into(),
                    data: None,
                },
                id: RequestId::Number(0),
            }))
            .await
            .context("writing response")?;
        debug!(?cwd, "no instance found for path");
    }

    Ok(())
}

/// Find or spawn a language server instance and connect the client to it
async fn connect(
    port: u16,
    instance_map: Arc<Mutex<InstanceMap>>,
    (server, args, cwd): (String, Vec<String>, Option<String>),
    req: Request,
    init_params: InitializeParams,
    mut reader: LspReader<BufReader<OwnedReadHalf>>,
    mut writer: LspWriter<OwnedWriteHalf>,
) -> Result<()> {
    // Select the workspace root directory.
    let workspace_root = {
        let (scheme, _, mut path, _, _) = select_workspace_root(&init_params, cwd.as_deref())
            .context("could not get any workspace_root")?
            .into_parts();
        if scheme != uriparse::Scheme::File {
            bail!("only `file://` URIs are supported");
        }
        path.normalize(false);
        path.to_string()
    };

    // Get an language server instance for this client.
    let key = InstanceKey {
        server,
        args,
        workspace_root,
    };
    let instance = instance::get_or_spawn(instance_map, key, init_params).await?;

    // Respond to client's `initialize` request using a response result from
    // the first time this server instance was initialized, it might not be
    // a response directly to our previous request but it should be hopefully
    // similar if it comes from another instance of the same client.
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

    let (client, client_rx) = Client::new(port);
    task::spawn(input_task(client_rx, writer).in_current_span());
    instance.add_client(client.clone()).await;

    task::spawn(output_task(reader, client, instance).in_current_span());

    Ok(())
}

fn select_workspace_root<'a>(
    init_params: &'a InitializeParams,
    proxy_cwd: Option<&'a str>,
) -> Result<URI<'a>> {
    if init_params.workspace_folders.len() > 1 {
        // TODO Ideally we'd be looking up any server which has a superset of
        // workspace folders active possibly adding transforming the `initialize`
        // request into a few requests for adding workspace folders if the
        // server supports it. Buuut let's just run with supporting single-folder
        // workspaces only at first, it's probably the most common use-case anyway.
        warn!("initialize request with multiple workspace folders isn't supported");
        debug!(workspace_folders = ?init_params.workspace_folders);
    }

    if init_params.workspace_folders.len() == 1 {
        match URI::try_from(init_params.workspace_folders[0].uri.as_str())
            .context("parse initParams.workspaceFolders[0].uri")
        {
            Ok(root) => return Ok(root),
            Err(err) => warn!(?err, "failed to parse URI"),
        }
    }

    assert!(init_params.workspace_folders.is_empty());

    // Using the deprecated fields `rootPath` or `rootUri` as fallback
    if let Some(root_uri) = &init_params.root_uri {
        match URI::try_from(root_uri.as_str()).context("parse initParams.rootUri") {
            Ok(root) => return Ok(root),
            Err(err) => warn!(?err, "failed to parse URI"),
        }
    }
    if let Some(root_path) = &init_params.root_path {
        // `rootPath` doesn't have a schema but `Url` requires it to parse
        match uriparse::Path::try_from(root_path.as_str())
            .map_err(uriparse::URIError::from)
            .and_then(|path| {
                URI::builder()
                    .with_scheme(uriparse::Scheme::File)
                    .with_path(path)
                    .build()
            })
            .context("parse initParams.rootPath")
        {
            Ok(root) => return Ok(root),
            Err(err) => warn!(?err, "failed to parse URI"),
        }
    }

    // Using the proxy `cwd` as fallback
    if let Some(proxy_cwd) = proxy_cwd {
        match uriparse::Path::try_from(proxy_cwd)
            .map_err(uriparse::URIError::from)
            .and_then(|path| {
                URI::builder()
                    .with_scheme(uriparse::Scheme::File)
                    .with_path(path)
                    .build()
            })
            .context("parse initParams.initializationOptions.lspMux.cwd")
        {
            Ok(root) => return Ok(root),
            Err(err) => warn!(?err, "failed to parse URI"),
        }
    }

    bail!("could not determine a suitable workspace_root");
}

/// Receive messages from channel and write them to the client input socket
async fn input_task(mut rx: mpsc::Receiver<Message>, mut writer: LspWriter<OwnedWriteHalf>) {
    // The other end of this channel is held by the `output_task` _and_ in the
    // `Instance` itself, this task depends on the `output_task` to detect a
    // client disconnect and call `Instance::cleanup_client`, otherwise we're
    // going to hang forever here.
    while let Some(message) = rx.recv().await {
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

/// Read messages from client output socket and send them to the server channel
async fn output_task(
    mut reader: LspReader<BufReader<OwnedReadHalf>>,
    client: Client,
    instance: Arc<Instance>,
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
        instance.keep_alive();

        match message {
            Message::Request(req) if req.method == "shutdown" => {
                // Client requested the server to shut down but other clients might still be connected.
                // Instead we disconnect this client to prevent the editor hanging
                // see <https://github.com/pr2502/ra-multiplex/issues/5>.
                info!("client sent shutdown request, sending a response and closing connection");

                // <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#shutdown>
                let res = ResponseSuccess::null(req.id);
                // Ignoring error because we would've closed the connection regardless
                let _ = client.send_message(res.into()).await;
                break;
            }

            Message::Request(mut req) => {
                req.id = req.id.tag(Tag::Port(client.port));
                if instance.send_message(req.into()).await.is_err() {
                    break;
                }
            }

            Message::ResponseSuccess(mut res) => match res.id.untag() {
                (Some(Tag::Forward), id) => {
                    res.id = id;
                    if instance.send_message(res.into()).await.is_err() {
                        break;
                    }
                }
                (Some(Tag::Drop), _) => {
                    // Drop the message
                }
                _ => {
                    debug!(?res, "unexpected client response");
                }
            },

            Message::ResponseError(res) => {
                warn!(?res, "client responded with error");
            }

            Message::Notification(notif) if notif.method == "textDocument/didOpen" => {
                if let Err(err) = instance.open_file(client.port, notif.params).await {
                    warn!(?err, "error opening file");
                }
            }

            Message::Notification(notif) if notif.method == "textDocument/didClose" => {
                if let Err(err) = instance.close_file(client.port, notif.params).await {
                    warn!(?err, "error closing file");
                }
            }

            Message::Notification(notif) => {
                if instance.send_message(notif.into()).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Err(err) = instance.cleanup_client(client).await {
        warn!(?err, "error cleaning up after a client");
    }
}

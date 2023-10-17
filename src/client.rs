use std::io::ErrorKind;
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

pub struct Client {
    port: u16,
    initialize_request_id: Option<Value>,
    instance: Arc<Instance>,
}

impl Client {
    /// finds or spawns a rust-analyzer instance and connects the client
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

        let mut client = Client {
            port,
            initialize_request_id: None,
            instance: instance::get(instance_map, &key).await?,
        };

        client.wait_for_initialize_request(&mut socket_read).await?;

        let (client_tx, client_rx) = client.register_client_with_instance().await;
        let (close_tx, close_rx) = mpsc::channel(1);
        client.spawn_input_task(client_rx, close_rx, socket_write);
        client.spawn_output_task(socket_read, close_tx);

        client.wait_for_initialize_response(client_tx).await?;
        Ok(())
    }

    async fn wait_for_initialize_request(
        &mut self,
        socket_read: &mut BufReader<OwnedReadHalf>,
    ) -> Result<()> {
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

        // we save the request id so we can later use it for the response
        self.initialize_request_id = Some(req.id.clone());

        if self.instance.init_cache.attempt_send_request() {
            // it haven't been sent yet, we can send it.
            //
            // instead of tagging the original id we replace it with a custom id that only
            // the `initialize` uses
            req.id = Value::String(INIT_REQUEST_ID.to_owned());

            self.instance
                .message_writer
                .send(req.into())
                .await
                .context("forward client request")?;
            self.instance.keep_alive();
        } else {
            // initialize request was already sent for this instance, no need to send it again
        }
        Ok(())
    }

    async fn wait_for_initialize_response(&self, tx: mpsc::Sender<Message>) -> Result<()> {
        // parse the cached message and restore the `id` to the value this client expects
        let mut res = self.instance.init_cache.response.get().await;
        res.id = self
            .initialize_request_id
            .clone()
            .expect("BUG: need to wait_for_initialize_request first");
        debug!(message = ?Message::from(res.clone()), "send response to InitializeRequest");
        tx.send(res.into())
            .await
            .context("send initialize response")?;
        Ok(())
    }

    async fn register_client_with_instance(
        &self,
    ) -> (mpsc::Sender<Message>, mpsc::Receiver<Message>) {
        let (client_tx, client_rx) = mpsc::channel(64);
        self.instance
            .message_readers
            .write()
            .await
            .insert(self.port, client_tx.clone());
        (client_tx, client_rx)
    }

    fn spawn_input_task(
        &self,
        mut rx: mpsc::Receiver<Message>,
        mut close_rx: mpsc::Receiver<Message>,
        socket_write: OwnedWriteHalf,
    ) {
        let mut writer = LspWriter::new(socket_write);
        task::spawn(
            async move {
                // unlike the output task, here we first wait on the channel which is going to
                // block until the rust-analyzer server sends a notification, however if we're the last
                // client and have just closed the server is unlikely to send any. this results in the
                // last client often falsely hanging while the gc task depends on the input channels being
                // closed to detect a disconnected client.
                //
                // when a client sends a shutdown request we receive a message on the close_rx, send
                // the reply and close the connection. if no shutdown request was received but the
                // client closed close_rx channel will be dropped (unlike the normal rx channel which
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
            .in_current_span(),
        );
    }

    fn spawn_output_task(
        &self,
        socket_read: BufReader<OwnedReadHalf>,
        close_tx: mpsc::Sender<Message>,
    ) {
        let port = self.port;
        let instance = Arc::clone(&self.instance);
        let instance_tx = self.instance.message_writer.clone();
        task::spawn(
            async move {
                match read_client_socket(socket_read, instance_tx, close_tx, port, instance).await {
                    Ok(_) => debug!("client output closed"),
                    Err(err) => error!(?err, "error reading client output"),
                }
            }
            .in_current_span(),
        );
    }
}

fn tag_id(port: u16, id: &Value) -> Result<String> {
    match id {
        Value::Number(number) => Ok(format!("{port}:n:{number}")),
        Value::String(string) => Ok(format!("{port}:s:{string}")),
        _ => bail!("unexpected message id type {id:?}"),
    }
}

/// reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn read_client_socket(
    socket_read: BufReader<OwnedReadHalf>,
    tx: mpsc::Sender<Message>,
    close_tx: mpsc::Sender<Message>,
    port: u16,
    instance: Arc<Instance>,
) -> Result<()> {
    let mut reader = LspReader::new(socket_read);

    while let Some(message) = reader.read_message().await? {
        trace!(?message, "<- client");

        match message {
            Message::Request(mut req) if req.method == "initialized" => {
                // initialized notification can only be sent once per server
                if instance.init_cache.attempt_send_notif() {
                    debug!("send InitializedNotification");

                    req.id = tag_id(port, &req.id)?.into();
                    if tx.send(req.into()).await.is_err() {
                        break;
                    }
                    instance.keep_alive();
                } else {
                    // we're not the first, skip processing the message further
                    debug!("skip InitializedNotification");
                    continue;
                }
            }

            Message::Request(req) if req.method == "shutdown" => {
                // client requested the server to shut down but other clients might still be connected.
                // instead we disconnect this client to prevent the editor hanging
                // see <https://github.com/pr2502/ra-multiplex/issues/5>
                info!("client sent shutdown request, sending a response and closing connection");
                // <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#shutdown>
                let message = Message::ResponseSuccess(ResponseSuccess {
                    jsonrpc: Version,
                    result: Value::Null,
                    id: req.id,
                });
                // ignoring error because we would've closed the connection regardless
                let _ = close_tx.send(message).await;
                break;
            }

            Message::Request(mut req) => {
                req.id = tag_id(port, &req.id)?.into();
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
    Ok(())
}

use super::instance::{
    InitializeCache, InstanceKey, InstanceRegistry, RaInstance, INIT_REQUEST_ID,
};
use anyhow::{bail, Context, Result};
use ra_multiplex::{lsp::{Message, self}, proto};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::{select, task};

pub type ResponseSet = Arc<Mutex<HashSet<String>>>;
pub struct Client {
    port: u16,
    initialize_request_id: Option<Value>,
    instance: Arc<RaInstance>,
    responses: ResponseSet,
}

impl Client {
    /// finds or spawns a rust-analyzer instance and connects the client
    pub async fn process(
        socket: TcpStream,
        port: u16,
        registry: InstanceRegistry,
        responses: ResponseSet,
    ) -> Result<()> {
        let (socket_read, socket_write) = socket.into_split();
        let mut socket_read = BufReader::new(socket_read);

        let mut buffer = Vec::new();
        let proto_init = proto::Init::from_reader(&mut buffer, &mut socket_read).await?;

        let key = InstanceKey::from_proto_init(&proto_init).await;
        log::debug!("client configured {key:?}");

        let mut client = Client {
            port,
            initialize_request_id: None,
            instance: registry.get(&key).await?,
            responses,
        };

        client.wait_for_initialize_request(&mut socket_read).await?;

        let (client_tx, client_rx) = client.register_client_with_instance().await;
        let (close_tx, close_rx) = mpsc::channel(1);
        client.spawn_input_task(client_rx, close_rx, socket_write);
        client.spawn_output_task(socket_read, close_tx);

        client
            .wait_for_initialize_response(client_tx, &mut buffer)
            .await?;
        Ok(())
    }

    async fn wait_for_initialize_request(
        &mut self,
        socket_read: &mut BufReader<OwnedReadHalf>,
    ) -> Result<()> {
        let mut buffer = Vec::new();
        let port = self.port;
        let (mut json, _bytes) = lsp::read_message(&mut *socket_read, &mut buffer)
            .await?
            .context("channel closed")?;
        if !matches!(json.get("method"), Some(Value::String(method)) if method == "initialize") {
            bail!("first client message was not InitializeRequest");
        }
        log::debug!("[{port}] recv InitializeRequest");
        // this is an initialize request, it's special because it cannot be sent twice or
        // rust-analyzer will crash.

        // we save the request id so we can later use it for the response
        self.initialize_request_id = Some(
            json.remove("id")
                .context("InitializeRequest is missing an `id`")?,
        );
        if self.instance.init_cache.attempt_send_request() {
            // it haven't been sent yet, we can send it.
            //
            // instead of tagging the original id we replace it with a custom id that only
            // the `initialize` uses
            json.insert("id".to_owned(), Value::String(INIT_REQUEST_ID.to_owned()));

            self.instance
                .message_writer
                .send(Message::from_json(&json, &mut buffer))
                .await
                .context("forward client request")?;
        } else {
            // initialize request was already sent for this instance, no need to send it again
        }
        Ok(())
    }

    async fn wait_for_initialize_response(
        &self,
        tx: mpsc::Sender<Message>,
        buffer: &mut Vec<u8>,
    ) -> Result<()> {
        let port = self.port;
        // parse the cached message and restore the `id` to the value this client expects
        let response = self.instance.init_cache.response.get().await;
        let mut json: Map<String, Value> = serde_json::from_slice(response.as_bytes())
            .expect("BUG: cached initialize response was invalid");
        json.insert(
            "id".to_owned(),
            self.initialize_request_id
                .clone()
                .expect("BUG: need to wait_for_initialize_request first"),
        );
        let message = Message::from_json(&json, buffer);
        log::debug!("[{port}] send response to InitializeRequest");
        tx.send(message).await.context("send initialize response")?;
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
        mut socket_write: OwnedWriteHalf,
    ) {
        let port = self.port;
        task::spawn(async move {
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
                log::trace!(
                    "rpc.send[client] {}",
                    std::str::from_utf8(message.as_bytes()).unwrap_or("not utf-8")
                );
                if let Err(err) = message.to_writer(&mut socket_write).await {
                    match err.kind() {
                        // ignore benign errors, treat as socket close
                        ErrorKind::BrokenPipe => {}
                        // report fatal errors
                        _ => log::error!("[{port}] error writing client input: {err}"),
                    }
                    break; // break on any error
                }
            }
            log::debug!("[{port}] client input closed");
            log::info!("[{port}] client disconnected");
        });
    }

    fn spawn_output_task(
        &self,
        socket_read: BufReader<OwnedReadHalf>,
        close_tx: mpsc::Sender<Message>,
    ) {
        let port = self.port;
        let instance = Arc::clone(&self.instance);
        let instance_tx = self.instance.message_writer.clone();
        let responses = self.responses.clone();
        task::spawn(async move {
            match read_client_socket(
                socket_read,
                instance_tx,
                close_tx,
                port,
                &instance.init_cache,
                responses,
            )
            .await
            {
                Ok(_) => log::debug!("[{port}] client output closed"),
                Err(err) => log::error!("[{port}] error reading client output: {err:?}"),
            }
        });
    }
}

fn tag_id(port: u16, id: &Value) -> Result<String> {
    match id {
        Value::Number(number) => Ok(format!("{port:04x}:n:{number}")),
        Value::String(string) => Ok(format!("{port:04x}:s:{string}")),
        _ => bail!("unexpected message id type {id:?}"),
    }
}

/// reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn read_client_socket(
    mut socket_read: BufReader<OwnedReadHalf>,
    tx: mpsc::Sender<Message>,
    close_tx: mpsc::Sender<Message>,
    port: u16,
    init_cache: &InitializeCache,
    responses: ResponseSet,
) -> Result<()> {
    let mut buffer = Vec::new();

    while let Some((mut json, bytes)) = lsp::read_message(&mut socket_read, &mut buffer).await? {
        log::trace!(
            "rpc.receive[client] {}",
            std::str::from_utf8(bytes).unwrap_or("not utf-8")
        );

        if matches!(json.get("method"), Some(Value::String(method)) if method == "initialized") {
            // initialized notification can only be sent once per server
            if init_cache.attempt_send_notif() {
                log::debug!("[{port}] send InitializedNotification");
            } else {
                // we're not the first, skip processing the message further
                log::debug!("[{port}] skip InitializedNotification");
                continue;
            }
        }
        let method = json.get("method");
        if matches!(method, Some(Value::String(method)) if method == "shutdown") {
            // client requested the server to shut down but other clients might still be connected.
            // instead we disconnect this client to prevent the editor hanging
            // see <https://github.com/pr2502/ra-multiplex/issues/5>
            if let Some(shutdown_request_id) = json.get("id") {
                log::info!("[{port}] client sent shutdown request, sending a response and closing connection");
                // <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#shutdown>
                let message = Message::from_json(
                    &json!({
                        "id": shutdown_request_id,
                        "jsonrpc": "2.0",
                        "result": null,
                    }),
                    &mut buffer,
                );
                // ignoring error because we would've closed the connection regardless
                let _ = close_tx.send(message).await;
            }
            break;
        }
        if let Some(id) = json.get("id") {
            if matches!(method, Some(_)) {
                // methods containing an id need the id modified so we can discern which client to send
                // the response to
                let tagged_id = tag_id(port, id)?;
                json.insert("id".to_owned(), Value::String(tagged_id));

                let message = Message::from_json(&json, &mut buffer);
                if tx.send(message).await.is_err() {
                    break;
                }
            } else if matches!(json.get("result"), Some(_))
                && responses.lock().await.insert(
                    id.as_str()
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| id.to_string()),
                )
            {
                // Don't modify the id of a response.
                // Only send the response of the first client, suboptimal
                // May want to special case workspace/configuration
                let message = Message::from_bytes(bytes);
                if tx.send(message).await.is_err() {
                    break;
                }
            }
        } else {
            // notification messages without an id don't need any modification and can be forwarded
            // to rust-analyzer as is
            let message = Message::from_bytes(bytes);
            if tx.send(message).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

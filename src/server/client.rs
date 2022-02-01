use crate::server::instance::{InitializeCache, InstanceRegistry, RaInstance};
use crate::server::lsp::{self, Message};
use anyhow::{bail, Context, Result};
use ra_multiplex::common::proto;
use serde_json::Value;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::{select, task};

pub struct Client {
    port: u16,
    instance: Arc<RaInstance>,
}

impl Client {
    /// finds or spawns a rust-analyzer instance and connects the client
    pub async fn process(socket: TcpStream, port: u16, registry: InstanceRegistry) -> Result<()> {
        let (socket_read, socket_write) = socket.into_split();
        let mut socket_read = BufReader::new(socket_read);

        let mut buffer = Vec::new();
        let proto_init = proto::Init::from_reader(&mut buffer, &mut socket_read).await?;

        let project_root = find_project_root(&proto_init.cwd)
            .with_context(|| format!("couldn't find project root for {}", &proto_init.cwd))?;

        let instance = registry.get(&project_root).await?;

        let client = Client { port, instance };

        client.wait_for_initialize_request(&mut socket_read).await?;

        let (client_tx, client_rx) = client.register_client_with_instance().await;
        let client_close = Arc::new(Notify::new());
        client.spawn_input_task(client_rx, socket_write, Arc::clone(&client_close));
        client.spawn_output_task(socket_read, client_close);

        client.wait_for_initialize_response(client_tx).await;
        Ok(())
    }

    async fn wait_for_initialize_request(
        &self,
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
        // this is an initialize request, it's special because it cannot
        if self.instance.init_cache.attempt_send_request() {
            // it haven't been sent yet, we can send it.

            // instead of taggin the original id we replace it with a custom id that only
            // the `initialize` uses
            json.insert(
                "id".to_owned(),
                Value::String("initialize_request".to_owned()),
            );

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

    async fn wait_for_initialize_response(&self, tx: mpsc::Sender<Message>) {
        let port = self.port;
        let message = self.instance.init_cache.response.get().await;
        log::debug!("[{port}] send response to InitializeRequest");
        tx.send(message).await.unwrap();
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
        mut socket_write: OwnedWriteHalf,
        close: Arc<Notify>,
    ) {
        let port = self.port;
        task::spawn(async move {
            // unlike the output task, here we first wait on the channel which is going to
            // block until the rust-analyzer server sends a notification, however if we're the last
            // client and have just closed the server is unlikely to send any. this results in the
            // last client often falsely hanging while the gc task depends on the input channels being
            // closed to detect a disconnected client.
            //
            // to solve this we use a notification from the output task which closes as soon as the
            // tcp connection from client is closed.
            while let Some(message) = select! {
                _ignore = close.notified() => None,
                message = rx.recv() => message,
            } {
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

    fn spawn_output_task(&self, socket_read: BufReader<OwnedReadHalf>, close: Arc<Notify>) {
        let port = self.port;
        let instance = Arc::clone(&self.instance);
        let instance_tx = self.instance.message_writer.clone();
        task::spawn(async move {
            match read_client_socket(socket_read, instance_tx, port, &instance.init_cache).await {
                Ok(_) => {
                    log::debug!("[{port}] client output closed");
                    close.notify_one(); // close the input channel as well
                }
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

/// we assume that the top-most directory from the client_cwd containing a file named `Cargo.toml`
/// is the project root
fn find_project_root(client_cwd: &str) -> Option<PathBuf> {
    let client_cwd = Path::new(&client_cwd);
    let mut project_root = None;
    for ancestor in client_cwd.ancestors() {
        let cargo_toml = ancestor.join("Cargo.toml");
        if cargo_toml.exists() {
            project_root = Some(ancestor.to_owned());
        }
    }
    project_root
}

/// reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn read_client_socket(
    mut reader: BufReader<OwnedReadHalf>,
    sender: mpsc::Sender<Message>,
    port: u16,
    init_cache: &InitializeCache,
) -> Result<()> {
    let mut buffer = Vec::new();

    while let Some((mut json, bytes)) = lsp::read_message(&mut reader, &mut buffer).await? {
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

        if let Some(id) = json.get("id") {
            // messages containing an id need the id modified so we can discern which client to send
            // the response to
            let tagged_id = tag_id(port, id)?;
            json.insert("id".to_owned(), Value::String(tagged_id));

            let message = Message::from_json(&json, &mut buffer);
            if sender.send(message).await.is_err() {
                break;
            }
        } else {
            // notification messages without an id don't need any modification and can be forwarded
            // to rust-analyzer as is
            let message = Message::from_bytes(bytes);
            if sender.send(message).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

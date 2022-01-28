#![feature(let_else)]

//! # LSP Multiplexer
//! Some LSP clients are not very smart about spawning the servers, for example coc-rust-analyzer
//! in neovim will spawn a new rust-analyzer instance per neovim instance, unfortunately this
//! wastes a _lot_ of resources.
//!
//! LSP Multiplexer attempts to solve this problem by spawning a single rust-analyzer instance per
//! cargo workspace and routing the messages through TCP to multiple clients.

use anyhow::{bail, ensure, Context, Result};
use ra_multiplex::proto;
use serde_json::{Map, Number, Value};
use server::lsp;
use server::message::Message;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::{self, FromStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio::task;

mod server {
    pub mod lsp;
    pub mod message;
}

struct InitializeCache {
    request_sent: AtomicBool,
    response_send: watch::Sender<Option<Message>>,
    response_recv: watch::Receiver<Option<Message>>,
    notif_send: watch::Sender<Option<Message>>,
    notif_recv: watch::Receiver<Option<Message>>,
}

impl Default for InitializeCache {
    fn default() -> Self {
        let (response_send, response_recv) = watch::channel(None);
        let (notif_send, notif_recv) = watch::channel(None);
        InitializeCache {
            request_sent: AtomicBool::new(false),
            response_send,
            response_recv,
            notif_send,
            notif_recv,
        }
    }
}

impl InitializeCache {
    async fn wait_for_response(&self) -> Message {
        let mut receiver = self.response_recv.clone();
        loop {
            if let Some(notif) = receiver.borrow().as_ref() {
                return notif.clone();
            }
            receiver.changed().await.unwrap();
        }
    }

    fn send_response(&self, message: Message) {
        self.response_send.send(Some(message)).unwrap();
    }

    async fn wait_for_notif(&self) -> Message {
        let mut receiver = self.notif_recv.clone();
        loop {
            if let Some(notif) = receiver.borrow().as_ref() {
                return notif.clone();
            }
            receiver.changed().await.unwrap();
        }
    }

    fn send_notif(&self, message: Message) {
        self.notif_send.send(Some(message)).unwrap();
    }

    /// returns true if this is the first thread attempting to send a request
    fn attempt_send_request(&self) -> bool {
        let already_sent = self.request_sent.swap(true, Ordering::SeqCst);
        !already_sent
    }
}

type MessageReaders = RwLock<HashMap<u16, mpsc::Sender<Message>>>;

struct RaInstance {
    message_readers: Arc<MessageReaders>,
    message_writer: mpsc::Sender<Message>,
    init_cache: Arc<InitializeCache>,
    child: Child,
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

impl RaInstance {
    fn spawn(project_root: &Path) -> Result<RaInstance> {
        let mut child = Command::new("rust-analyzer")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't spawn rust-analyzer")?;

        // read errors from server stderr and log them
        let path = project_root.display().to_string();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        task::spawn(async move {
            if let Err(err) = read_server_errors(stderr, &path).await {
                log::error!("[{path}] read_server_errors {err:?}");
            }
            log::info!("[{path}] read_server_errors closed");
        });

        let init_cache = Arc::new(InitializeCache::default());
        let message_readers = Arc::new(MessageReaders::default());

        // read messages from server stdout and distribute them to client channels
        let path = project_root.display().to_string();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let message_readers2 = Arc::clone(&message_readers);
        let init_cache2 = Arc::clone(&init_cache);
        task::spawn(async move {
            if let Err(err) = read_server_socket(stdout, message_readers2, init_cache2).await {
                log::error!("[{path}] read_server_socket {err:?}");
            }
            log::info!("[{path}] read_server_socket closed");
        });

        // read messages sent by clients from a channel and write them into server stdin
        let path = project_root.display().to_string();
        let stdin = child.stdin.take().unwrap();
        let (message_writer, rx) = mpsc::channel(64);
        task::spawn(async move {
            if let Err(err) = write_socket(rx, stdin).await {
                log::error!("[{path}] write_socket(server) {err:?}");
            }
            log::info!("[{path}] write_socket(server) closed");
        });

        Ok(RaInstance {
            child,
            message_readers,
            message_writer,
            init_cache,
        })
    }
}

/// finds or spawns a rust-analyzer instance and connects the client
async fn process_client(
    socket: TcpStream,
    port: u16,
    registry: Arc<Mutex<HashMap<PathBuf, RaInstance>>>,
) -> Result<()> {
    log::info!("accepted {port}");

    let (socket_read, socket_write) = socket.into_split();
    let mut socket_read = BufReader::new(socket_read);

    let mut buffer = Vec::new();
    socket_read
        .read_until(b'\0', &mut buffer)
        .await
        .context("read proto init")?;
    buffer.pop();

    let proto_init: proto::Init = serde_json::from_slice(&buffer).context("invalid proto init")?;
    ensure!(proto_init.check_version(), "invalid protocol version");
    ensure!(
        proto_init.args.is_empty(),
        "unexpected args passed to client"
    );

    let project_root = find_project_root(&proto_init.cwd)
        .with_context(|| format!("couldn't find project root for {}", &proto_init.cwd))?;

    let mut registry = registry.lock().await;
    let instance = match registry.entry(project_root.clone()) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(e) => {
            e.insert(RaInstance::spawn(&project_root).context("spawn ra instance")?)
        }
    };

    let tx = instance.message_writer.clone();
    let init_cache = Arc::clone(&instance.init_cache);
    task::spawn(async move {
        if let Err(err) = read_client_socket(socket_read, tx, port, init_cache).await {
            log::error!("read_client_socket {err:?}");
        }
        log::info!("read_client_socket closed");
    });

    let (tx, rx) = mpsc::channel(64);
    let mut message_readers = instance.message_readers.write().await;
    message_readers.insert(port, tx.clone());
    drop(message_readers);
    task::spawn(async move {
        if let Err(err) = write_socket(rx, socket_write).await {
            log::error!("write_socket(client) {err:?}");
        }
        log::info!("write_socket(client) closed");
    });

    let init_cache = Arc::clone(&instance.init_cache);
    drop(registry); // unlock the registry before the next await point

    let message = init_cache.wait_for_response().await;
    log::info!("send response to InitializeRequest for {port}");
    tx.send(message).await.unwrap();

    let message = init_cache.wait_for_notif().await;
    log::info!("send InitializedNotification for {port}");
    tx.send(message).await.unwrap();
    Ok(())
}

/// reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn read_client_socket<R>(
    mut reader: R,
    sender: mpsc::Sender<Message>,
    port: u16,
    init_cache: Arc<InitializeCache>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();

    loop {
        let header = lsp::Header::from_reader(&mut buffer, &mut reader)
            .await
            .context("parsing header")?;
        let Some(header) = header else {
            return Ok(());
        };

        buffer.clear();
        buffer.resize(header.content_length, 0);
        reader.read_exact(&mut buffer).await.context("read body")?;

        let mut json: Map<String, Value> =
            serde_json::from_slice(&buffer).context("invalid body")?;

        if let Some(Value::String(method)) = json.get("method") {
            if method == "initialize" {
                log::info!("recv InitializeRequest from {port}");
                // this is an initialize request, it's special because it cannot
                if init_cache.attempt_send_request() {
                    // it haven't been sent yet, we can send it.

                    // instead of taggin the original id we replace it with a custom id that only
                    // the `initialize` uses
                    json.insert("id".to_owned(), Value::String("initialize_request".to_owned()));
                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &json).context("serialize body")?;

                    sender
                        .send(Message::new(&buffer))
                        .await
                        .context("forward client request")?;
                }

                // we skip further processing regardless of whether we forwarded the initialize
                // request or not
                continue;
            }
        }

        if let Some(id) = json.get("id") {
            log::debug!("recv request port={port}, message_id={id:?}");

            // messages containing an id need the id modified so we can discern the client to send
            // the response to
            let tagged_id = tag_id(port, id)?;
            json.insert("id".to_owned(), Value::String(tagged_id));

            buffer.clear();
            serde_json::to_writer(&mut buffer, &json).context("serialize body")?;

            sender
                .send(Message::new(&buffer))
                .await
                .context("forward client request")?;
        } else {
            log::debug!("recv notif port={port}");

            // notification messages without an id don't need any modification and can be forwarded
            // to rust-analyzer as is
            sender
                .send(Message::new(&buffer))
                .await
                .context("forward client notification")?;
        }
    }
}

fn tag_id(port: u16, id: &Value) -> Result<String> {
    match id {
        Value::Number(number) => Ok(format!("{port:04x}:n:{number}")),
        Value::String(string) => Ok(format!("{port:04x}:s:{string}")),
        _ => bail!("unexpected message id type {id:?}"),
    }
}

fn parse_tagged_id(tagged: &str) -> Result<(u16, Value)> {
    let (port, rest) = tagged.split_once(':').context("missing first `:`")?;
    let port = u16::from_str_radix(port, 16)?;
    let (value_type, old_id) = rest.split_once(':').context("missing second `:`")?;
    let old_id = match value_type {
        "n" => Value::Number(Number::from_str(old_id)?),
        "s" => Value::String(old_id.to_owned()),
        _ => bail!("invalid tag type `{value_type}`"),
    };
    Ok((port, old_id))
}

async fn read_server_socket<R>(
    mut reader: R,
    senders: Arc<MessageReaders>,
    init_cache: Arc<InitializeCache>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();

    loop {
        let header = lsp::Header::from_reader(&mut buffer, &mut reader)
            .await
            .context("parsing header")?;
        let header = if let Some(header) = header {
            header
        } else {
            return Ok(());
        };

        buffer.clear();
        buffer.resize(header.content_length, 0);
        reader.read_exact(&mut buffer).await.context("read body")?;

        let mut json: Map<String, Value> =
            serde_json::from_slice(&buffer).context("invalid body")?;

        if let Some(Value::String(method)) = json.get("method") {
            if method == "initialized" {
                // initialized notification needs to be cached so it can be sent to every new
                // client
                log::debug!("recv InitializedNotification");
                init_cache.send_notif(Message::new(&buffer));
                continue;
            }
        }

        if let Some(id) = json.get("id") {
            // we tagged the request id so we expect to only receive tagged responses
            let tagged_id = match id {
                Value::String(string) if string == "initialize_request" => {
                    // this is a response to the InitializeRequest, we need to process it
                    // separately
                    log::debug!("recv InitializeRequest response");
                    // NOTE we just guess the first request had originally the id Number(0)
                    json.insert("id".to_owned(), Value::Number(0.into()));
                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &json).context("serialize body")?;
                    init_cache.send_response(Message::new(&buffer));
                    continue;
                }
                Value::String(string) => string,
                _ => {
                    log::warn!("unexpected response message id type {id:?}");
                    log::debug!("response to no request {}", Value::Object(json));
                    // FIXME uncommenting this crashes rust-analyzer, presumably because the client
                    // then sends a confusing response or something? i'm guessing that the response
                    // id gets tagged with port but rust-analyzer expects to know the id because
                    // it's actually a response and not a request. i'm not sure if we can handle
                    // these at all with multiple clients attached
                    //
                    // ideally we could send these to all clients, but what if there is a matching
                    // response from each client? rust-analyzer only expects one (this might
                    // actually be why it's crashing)
                    //
                    // ignoring these might end up being the safest option, they don't seem to
                    // matter to neovim anyway
                    // ```rust
                    // let message = Message::new(&buffer);
                    // let senders = senders.read().await;
                    // for sender in senders.values() {
                    //     sender
                    //         .send(message.clone())
                    //         .await
                    //         .context("forward server notification")?;
                    // }
                    // ```
                    continue;
                }
            };

            let (port, old_id) = match parse_tagged_id(tagged_id) {
                Ok(ok) => ok,
                Err(err) => {
                    log::warn!("invalid tagged id {err:?}");
                    continue;
                }
            };

            log::debug!("send response port={port}, message_id={old_id:?}");

            json.insert("id".to_owned(), old_id);

            buffer.clear();
            serde_json::to_writer(&mut buffer, &json).context("serialize body")?;

            let senders = senders.read().await;
            if let Some(sender) = senders.get(&port) {
                sender
                    .send(Message::new(&buffer))
                    .await
                    .context("forward server response")?;
            } else {
                log::warn!("no client on port {port}");
            }
        } else {
            log::debug!("send notif port=? {}", Value::Object(json));

            // notification messages without an id are sent to all clients
            let message = Message::new(&buffer);
            let senders = senders.read().await;
            for sender in senders.values() {
                sender
                    .send(message.clone())
                    .await
                    .context("forward server notification")?;
            }
        }
    }
}

async fn read_server_errors<R>(mut reader: R, path: &str) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = String::new();
    loop {
        buffer.clear();
        let bytes_read = reader.read_line(&mut buffer).await.context("[{path}]")?;
        if bytes_read == 0 {
            // reached EOF
            return Ok(());
        }
        let line = buffer.trim_end();
        log::error!("[{path}] {line}");
    }
}

/// reads messages from a `mpsc::Receiver` and writes them into a socket
async fn write_socket<W>(mut receiver: mpsc::Receiver<Message>, mut writer: W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = receiver.recv().await {
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", message.len()).as_bytes())
            .await
            .context("write header")?;
        writer
            .write_all(message.as_bytes())
            .await
            .context("write body")?;
        writer.flush().await.context("flush socket")?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let registry = Arc::new(Mutex::new(HashMap::new()));

    let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), proto::PORT))
        .await
        .context("listen")?;

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let registry = Arc::clone(&registry);
                task::spawn(async move {
                    if let Err(err) = process_client(socket, addr.port(), registry).await {
                        log::error!("{err:?}");
                    }
                });
            }
            Err(err) => match err.kind() {
                // ignore benign errors
                std::io::ErrorKind::NotConnected => {
                    log::warn!("{err}");
                }
                _ => {
                    Err(err).context("accept connection")?;
                }
            },
        }
    }
}

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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::{self, FromStr};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task;

mod server {
    pub mod lsp;
    pub mod message;
}

struct RaInstance {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: ChildStderr,

    args: Vec<String>,
    project_root: PathBuf,
}

impl RaInstance {
    fn spawn(client_cwd: String, client_args: Vec<String>) -> Result<RaInstance> {
        // find the cargo project root:
        // we assume that the top-most directory from the client_cwd containing a file named `Cargo.toml` is
        // the project root
        let client_cwd = Path::new(&client_cwd);
        let mut project_root = None;
        for ancestor in client_cwd.ancestors() {
            let cargo_toml = ancestor.join("Cargo.toml");
            if cargo_toml.exists() {
                project_root = Some(ancestor.to_owned());
            }
        }
        let project_root = project_root
            .with_context(|| format!("couldn't find project root for {}", client_cwd.display()))?;

        let child = Command::new("rust-analyzer")
            .args(&client_args)
            .current_dir(&project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't spawn rust-analyzer")?;

        Ok(RaInstance {
            stdin: child.stdin.unwrap(),
            stdout: BufReader::new(child.stdout.unwrap()),
            stderr: child.stderr.unwrap(),
            args: client_args,
            project_root,
        })
    }
}

/// finds or spawns a rust-analyzer instance and connects the client
async fn process_client(socket: TcpStream, port: u16) -> Result<()> {
    log::info!("accepted {port}");

    let (socket_read, socket_write) = socket.into_split();
    let mut socket_read = BufReader::new(socket_read);

    let mut header = Vec::new();
    socket_read
        .read_until(b'\0', &mut header)
        .await
        .context("read proto init")?;
    header.pop();

    let proto_init: proto::Init = serde_json::from_slice(&header).context("invalid proto init")?;
    ensure!(proto_init.check_version(), "invalid protocol version");

    // TODO for now we're spawning a new instance unconditionally
    let instance =
        RaInstance::spawn(proto_init.cwd, proto_init.args).context("spawn ra instance")?;

    let (tx, rx) = mpsc::channel(64);
    task::spawn(async move {
        if let Err(err) = read_client_socket(socket_read, tx, port).await {
            log::error!("read_client_socket {err:?}");
        }
    });
    task::spawn(async move {
        if let Err(err) = write_socket(rx, instance.stdin).await {
            log::error!("write_socket(server) {err:?}");
        }
    });

    let (tx, rx) = mpsc::channel(64);
    task::spawn(async move {
        if let Err(err) = read_server_socket(instance.stdout, tx).await {
            log::error!("read_server_socket {err:?}");
        }
    });
    task::spawn(async move {
        if let Err(err) = write_socket(rx, socket_write).await {
            log::error!("write_socket(client) {err:?}");
        }
    });
    Ok(())
}

/// reads from client socket and tags the id for requests, forwards the messages into a mpsc queue
/// to the writer
async fn read_client_socket<R>(
    mut reader: R,
    sender: mpsc::Sender<Message>,
    port: u16,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();

    loop {
        let header = lsp::Header::from_reader(&mut buffer, &mut reader)
            .await
            .context("parsing header")?;

        buffer.clear();
        buffer.resize(header.content_length, 0);
        reader.read_exact(&mut buffer).await.context("read body")?;

        let mut json: Map<String, Value> =
            serde_json::from_slice(&buffer).context("invalid body")?;
        if let Some(id) = json.get("id") {
            log::debug!("recv request port={port}, message_id={id:?}");

            // messages containing an id need the id modified so we can discern the client to send
            // the response to
            let tagged_id = tag_id(port, id)?;
            json.insert("id".to_owned(), Value::String(tagged_id));

            buffer.clear();
            serde_json::to_writer(&mut buffer, &json).context("serialize body")?;

            sender
                .send(Message::new(&buffer).with_port(port))
                .await
                .context("forward client request")?;
        } else {
            log::debug!("recv notif port={port}");

            // notification messages without an id don't need any modification and can be forwarded
            // to rust-analyzer as is
            sender
                .send(Message::new(&buffer).with_port(port))
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

async fn read_server_socket<R>(mut reader: R, sender: mpsc::Sender<Message>) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();

    loop {
        let header = lsp::Header::from_reader(&mut buffer, &mut reader)
            .await
            .context("parsing header")?;

        buffer.clear();
        buffer.resize(header.content_length, 0);
        reader.read_exact(&mut buffer).await.context("read body")?;

        let mut json: Map<String, Value> =
            serde_json::from_slice(&buffer).context("invalid body")?;
        if let Some(id) = json.get("id") {
            // we tagged the request id so we expect to only receive tagged responses
            let tagged_id = match id {
                Value::String(string) => string,
                _ => {
                    log::warn!("unexpected response message id type {id:?}");
                    log::debug!("response to no request {}", Value::Object(json));
                    // BUG uncomment this and it crashes both socket readers, why??
                    // sender
                    //     .send(Message::new(&buffer))
                    //     .await
                    //     .context("forward server request")?;
                    continue;
                }
            };

            let (port, old_id) = match parse_tagged_id(&tagged_id) {
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

            sender
                .send(Message::new(&buffer).with_port(port))
                .await
                .context("forward server response")?;
        } else {
            log::debug!("send notif port=? {}", Value::Object(json));

            // notification messages without an id don't need any modification and can be forwarded
            // to rust-analyzer as is
            sender
                .send(Message::new(&buffer))
                .await
                .context("forward server notification")?;
        }
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

    let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), proto::PORT))
        .await
        .context("listen")?;

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                task::spawn(async move {
                    if let Err(err) = process_client(socket, addr.port()).await {
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

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{bail, Context as _, Result};
use pin_project_lite::pin_project;
use tokio::io::{self, AsyncRead, AsyncWrite, BufStream};
use tokio::net::TcpStream;

use crate::config::Config;
use crate::lsp::jsonrpc::Message;
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{InitializationOptions, InitializeParams, LspMuxOptions};

pin_project! {
    struct Stdio {
        #[pin]
        stdin: io::Stdin,
        #[pin]
        stdout: io::Stdout,
    }
}

fn stdio() -> Stdio {
    Stdio {
        stdin: io::stdin(),
        stdout: io::stdout(),
    }
}

impl AsyncRead for Stdio {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut io::ReadBuf,
    ) -> Poll<io::Result<()>> {
        self.project().stdin.poll_read(cx, buf)
    }
}

impl AsyncWrite for Stdio {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.project().stdout.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.project().stdout.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.project().stdout.poll_shutdown(cx)
    }
}

pub async fn run(server: String, args: Vec<String>) -> Result<()> {
    let config = Config::load_or_default().await;
    let version = String::from("test"); // TODO use a real protocol version number

    let mut stream = TcpStream::connect(config.connect)
        .await
        .context("connect")?;
    let mut stdio = BufStream::new(stdio());

    // Wait for the client to send `initialize` request.
    let mut reader = LspReader::new(&mut stdio, "client");
    let mut req = match reader.read_message().await?.context("stdin closed")? {
        Message::Request(req) if req.method == "initialize" => req,
        _ => bail!("first client message was not initialize request"),
    };

    // Patch `initializationOptions` with our own data.
    let mut params = serde_json::from_value::<InitializeParams>(req.params)
        .context("parse initialize request params")?;
    params
        .initialization_options
        .get_or_insert_with(InitializationOptions::default)
        .lsp_mux
        .get_or_insert_with(|| LspMuxOptions {
            version,
            server,
            args,
        });
    req.params = serde_json::to_value(params).expect("BUG: invalid data");

    // Forward the modified `initialize` request.
    let mut writer = LspWriter::new(&mut stream, "server");
    writer
        .write_message(&req.into())
        .await
        .context("forward initialize request")?;

    // Forward everything else unmodified.
    io::copy_bidirectional(&mut stream, &mut stdio)
        .await
        .context("io error")?;
    Ok(())
}

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use pin_project_lite::pin_project;
use ra_multiplex::config::Config;
use ra_multiplex::proto;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

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

pub async fn main(server_path: String, server_args: Vec<String>) -> Result<()> {
    let config = Config::load_or_default().await;

    let proto_init = proto::Init::new(server_path, server_args);
    let mut proto_init = serde_json::to_vec(&proto_init).context("sending proto init")?;
    proto_init.push(b'\0');

    let mut stream = TcpStream::connect(config.connect)
        .await
        .context("connect")?;

    stream
        .write_all(&proto_init)
        .await
        .context("sending proto init")?;
    drop(proto_init);

    io::copy_bidirectional(&mut stream, &mut stdio())
        .await
        .context("io error")?;
    Ok(())
}

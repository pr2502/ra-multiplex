use anyhow::{Context, Result};
use clap::Parser;
use ra_multiplex::config::Config;
use ra_multiplex::proto;
use std::env;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::{io, task};

#[derive(Parser)]
#[clap(name = "ra-multiplex")]
#[clap(version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// LSP server to use
    #[clap(long, default_value = "rust-analyzer")]
    server: String,

    /// Arguments passed to the server
    args: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::load_or_default().await;

    let args = if config.arg_parsing {
        Args::parse()
    } else {
        Args {
            server: "rust-analyzer".to_owned(),
            args: env::args().skip(1).collect(),
        }
    };

    let proto_init = proto::Init::new(args.server, args.args);
    let mut proto_init = serde_json::to_vec(&proto_init).context("sending proto init")?;
    proto_init.push(b'\0');

    let stream = TcpStream::connect(config.connect)
        .await
        .context("connect")?;
    let (mut read_stream, mut write_stream) = stream.into_split();

    write_stream
        .write_all(&proto_init)
        .await
        .context("sending proto init")?;
    drop(proto_init);

    let t1 = task::spawn(async move {
        io::copy(&mut read_stream, &mut io::stdout())
            .await
            .context("io error")
    });
    let t2 = task::spawn(async move {
        io::copy(&mut io::stdin(), &mut write_stream)
            .await
            .context("io error")
    });
    tokio::select! {
        res = t1 => res,
        res = t2 => res,
    }
    .context("join")??;
    Ok(())
}

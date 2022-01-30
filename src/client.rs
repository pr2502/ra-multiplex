use anyhow::{Context, Result};
use ra_multiplex::common::config::Config;
use ra_multiplex::common::proto;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::{io, task};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::load_or_default();

    let stream = TcpStream::connect((config.listen, config.port))
        .await
        .context("connect")?;
    let (mut read_stream, mut write_stream) = stream.into_split();

    let proto_init = proto::Init::from_env();
    let mut proto_init = serde_json::to_vec(&proto_init).context("sending proto init")?;
    proto_init.push(b'\0');
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

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use tokio::io::BufReader;
use tokio::net::TcpStream;

use crate::config::Config;
use crate::lsp::jsonrpc::{Message, Request, RequestId, Version};
use crate::lsp::lspmux::{self, LspMuxOptions, StatusResponse};
use crate::lsp::transport::{LspReader, LspWriter};

pub async fn ext_request<T>(method: lspmux::Request) -> Result<T>
where
    T: DeserializeOwned,
{
    let config = Config::load_or_default().await;
    // let cwd = env::current_dir()
    //     .context("unable to get current_dir")?
    //     .to_str()
    //     .context("current_dir is not valid utf-8")?
    //     .to_owned();

    let (reader, writer) = TcpStream::connect(config.connect)
        .await
        .context("connect")?
        .into_split();
    let mut writer = LspWriter::new(writer, "lspmux");
    let mut reader = LspReader::new(BufReader::new(reader), "lspmux");

    writer
        .write_message(
            &Request {
                jsonrpc: Version,
                method: "initialize".into(),
                params: serde_json::to_value(LspMuxOptions {
                    version: LspMuxOptions::PROTOCOL_VERSION.into(),
                    method,
                })
                .unwrap(),
                id: RequestId::Number(0),
            }
            .into(),
        )
        .await
        .context("send lspmux request")?;

    match reader
        .read_message()
        .await
        .context("read lspmux response")?
        .context("stream ended")?
        .into_response()
        .context("received message was not a response")?
    {
        Ok(success) => serde_json::from_value(success.result).context("parse response result"),
        Err(error) => bail!(
            "unexpected error response {msg:?}",
            msg = Message::ResponseError(error),
        ),
    }
}

pub async fn status() -> Result<()> {
    let res = ext_request::<StatusResponse>(lspmux::Request::Status {}).await?;
    dbg!(res);
    Ok(())
}

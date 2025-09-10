use std::env;

use anyhow::{bail, Context, Result};
use serde::de::{DeserializeOwned, IgnoredAny};
use tokio::io::BufReader;

use crate::config::Config;
use crate::lsp::ext::{self, LspMuxOptions, StatusResponse};
use crate::lsp::jsonrpc::{Message, Request, RequestId, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{InitializationOptions, InitializeParams};
use crate::socketwrapper::Stream;

pub async fn ext_request<T>(config: &Config, method: ext::Request) -> Result<T>
where
    T: DeserializeOwned,
{
    let (reader, writer) = Stream::connect(&config.connect)
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
                params: serde_json::to_value(InitializeParams {
                    initialization_options: Some(InitializationOptions {
                        lsp_mux: Some(LspMuxOptions {
                            version: LspMuxOptions::PROTOCOL_VERSION.into(),
                            method,
                        }),
                        other_options: serde_json::Map::default(),
                    }),
                    process_id: None,
                    client_info: None,
                    locale: None,
                    root_path: None,
                    root_uri: None,
                    capabilities: None,
                    trace: None,
                    workspace_folders: Vec::new(),
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
            "received error response: {msg:?}",
            msg = Message::ResponseError(error),
        ),
    }
}

pub fn config(config: &Config) -> Result<()> {
    println!("{config:#?}");
    Ok(())
}

pub async fn status(config: &Config, json: bool) -> Result<()> {
    let res = ext_request::<StatusResponse>(config, ext::Request::Status {}).await?;

    if json {
        let json = serde_json::to_string(&res).unwrap();
        println!("{json}");
        return Ok(());
    }

    for instance in res.instances {
        println!("- Instance");
        println!("  pid: {}", instance.pid);
        println!("  server: {:?} {:?}", instance.server, instance.args);
        if !instance.env.is_empty() {
            println!("  server env:");
            for (key, val) in instance.env {
                println!("    {key} = {val}");
            }
        }
        println!("  path: {:?}", instance.workspace_root);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        println!("  last used: {}s ago", now - instance.last_used);
        println!("  registered dynamic capabilities:");
        for cap in instance.registered_dyn_capabilities {
            println!("    - {}", cap);
        }
        println!("  clients:");
        for client in instance.clients {
            println!("    - Client");
            println!("      id: {}", client.id);
            println!("      files:");
            for file in client.files {
                println!("        - {}", file);
            }
        }
    }
    Ok(())
}

pub async fn reload(config: &Config) -> Result<()> {
    let cwd = env::current_dir()
        .context("unable to get current_dir")?
        .to_str()
        .context("current_dir is not valid utf-8")?
        .to_owned();
    ext_request::<IgnoredAny>(config, ext::Request::Reload { cwd }).await?;
    Ok(())
}

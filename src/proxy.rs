use std::collections::BTreeMap;
use std::env;

use anyhow::{bail, Context as _, Result};
use tokio::io::{self, BufStream};

use crate::config::Config;
use crate::lsp::ext::{LspMuxOptions, Request};
use crate::lsp::jsonrpc::Message;
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{InitializationOptions, InitializeParams};
use crate::socketwrapper::Stream;

pub async fn run(config: &Config, server: String, args: Vec<String>) -> Result<()> {
    let cwd = env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(String::from));

    let mut env = BTreeMap::new();
    for key in &config.pass_environment {
        if let Ok(val) = std::env::var(key) {
            env.insert(key.clone(), val);
        }
    }

    let mut stream = Stream::connect(&config.connect)
        .await
        .context("connecting to server")?;
    let mut stdio = BufStream::new(io::join(io::stdin(), io::stdout()));

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
            version: LspMuxOptions::PROTOCOL_VERSION.to_owned(),
            method: Request::Connect {
                server,
                args,
                env,
                cwd,
            },
        });
    req.params = serde_json::to_value(params).expect("BUG: invalid data");

    // Forward the modified `initialize` request.
    let mut writer = LspWriter::new(&mut stream, "lspmux");
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

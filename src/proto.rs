use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Serialize, Deserialize, Debug)]
pub struct Init {
    proto: String,
    version: String,
    pub cwd: String,
    pub server: String,
    pub args: Vec<String>,
}

impl Init {
    pub fn new(server: String, args: Vec<String>) -> Init {
        Init {
            proto: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            cwd: env::current_dir()
                .expect("cannot access current directory")
                .to_str()
                .expect("current directory path is not valid UTF-8")
                .to_owned(),
            server,
            args,
        }
    }

    pub fn from_json(json: &serde_json::Map<String, Value>) -> Result<Self> {
        let params = json
            .get("params")
            .context("expected params in initialization message")?;
        let init_options = params
            .get("initializationOptions")
            .context("expected initializationOptions in params")?;

        let mut server = None;
        let mut args = Vec::new();
        if let Some(config) = init_options.get("raMultiplex") {
            server = config.get("server").and_then(|s| s.as_str());
            if let Some(values) = config.get("args").and_then(|s| s.as_array()) {
                for value in values {
                    args.push(
                        value
                            .as_str()
                            .context("expected args to be an array of strings")?
                            .to_owned(),
                    )
                }
            }
        };

        // TODO: this is deprecated and may be absent; try newer fields too.
        let cwd = params
            .get("rootPath")
            .context("expected rootPath in init message")?
            .as_str()
            .context("expected rootPath to be a String")?
            .to_owned();

        Ok(Init {
            // FIXME: this values here don't really make sense, but need to figure out what to do
            // with these fields
            proto: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            cwd,
            server: server.unwrap_or("rust-analyzer").to_string(),
            args,
        })
    }

    // XXX: unused
    pub async fn from_reader<R: AsyncBufRead + Unpin>(
        buffer: &mut Vec<u8>,
        mut reader: R,
    ) -> Result<Self> {
        buffer.clear();
        reader
            .read_until(b'\0', &mut *buffer)
            .await
            .context("read proto init")?;
        buffer.pop(); // remove trailing '\0'

        let proto_init: Self =
            serde_json::from_slice(buffer).context("invalid ra-multiplex proto init")?;
        ensure!(
            proto_init.check_version(),
            "ra-multiplex client protocol version differs from server version"
        );

        Ok(proto_init)
    }

    /// returns true if the version matches
    pub fn check_version(&self) -> bool {
        self.proto == env!("CARGO_PKG_NAME") && self.version == env!("CARGO_PKG_VERSION")
    }
}

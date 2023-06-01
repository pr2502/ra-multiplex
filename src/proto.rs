use anyhow::{Context, Result};
use log::debug;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;

#[derive(Serialize, Deserialize, Debug)]
pub struct Init {
    pub cwd: String,
    pub server: String,
    pub args: Vec<String>,
}

impl Init {
    pub fn new(server: String, args: Vec<String>) -> Init {
        Init {
            cwd: env::current_dir()
                .expect("cannot access current directory")
                .to_str()
                .expect("current directory path is not valid UTF-8")
                .to_owned(),
            server,
            args,
        }
    }

    /// Create an `Init` instance from a raw initialization request.
    ///
    /// Removes the `raMultiplex` objects from `initializationOptions` if it is present.
    pub fn from_init_request(json: &mut serde_json::Map<String, Value>) -> Result<Self> {
        let mut server = Option::<String>::None;
        let mut args = Vec::new();

        let params = json
            .get_mut("params")
            .context("initialization message should contain params")?;

        let init_options = params
            .get_mut("initializationOptions")
            .context("params should contain initializationOptions")?;

        if let Some(options) = init_options.as_object_mut() {
            if let Some(config) = options.remove("raMultiplex") {
                server = config
                    .get("server")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                if let Some(values) = config.get("args").and_then(|s| s.as_array()) {
                    for value in values {
                        args.push(
                            value
                                .as_str()
                                .context("args should to be an array of strings")?
                                .to_owned(),
                        )
                    }
                }
            } else {
                debug!("no `raMultiplex` objects in initialization request");
            }
        }

        // TODO: this is deprecated and may be absent; try newer fields too.
        let cwd = params
            .get("rootPath")
            .context("expected rootPath in init message")?
            .as_str()
            .context("expected rootPath to be a String")?
            .to_owned();

        Ok(Init {
            cwd,
            server: server.unwrap_or("rust-analyzer".to_string()),
            args,
        })
    }
}

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Serialize, Deserialize)]
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
            "ra-multiplex client protocol different version from the server"
        );

        Ok(proto_init)
    }

    /// returns true if the version matches
    pub fn check_version(&self) -> bool {
        self.proto == env!("CARGO_PKG_NAME") && self.version == env!("CARGO_PKG_VERSION")
    }
}

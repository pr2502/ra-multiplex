use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Serialize, Deserialize)]
pub struct Init {
    pub proto: String,
    pub version: String,
    pub cwd: String,
    pub args: Vec<String>,
}

impl Init {
    pub fn from_env() -> Init {
        Init {
            proto: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            cwd: env::current_dir()
                .expect("cannot access current directory")
                .to_str()
                .expect("current directory path is not valid UTF-8")
                .to_owned(),
            args: env::args().skip(1).collect(),
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

        let proto_init: Self = serde_json::from_slice(buffer).context("invalid proto init")?;
        ensure!(proto_init.check_version(), "invalid protocol version");
        ensure!(
            proto_init.args.is_empty(),
            "unexpected args passed to client"
        );

        Ok(proto_init)
    }

    /// returns true if the version matches
    pub fn check_version(&self) -> bool {
        self.proto == env!("CARGO_PKG_NAME") && self.version == env!("CARGO_PKG_VERSION")
    }
}

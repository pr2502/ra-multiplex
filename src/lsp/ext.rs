//! LSP-mux (ra-multiplex) specific protocol extensions

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde_derive::{Deserialize, Serialize};
use tracing::warn;

use super::jsonrpc::RequestId;

/// Additional metadata inserted into LSP RequestId
pub enum Tag {
    /// Request is coming from a client connected on this port
    Port(u16),
    /// Response to this request should be ignored
    Drop,
    /// Response to this request should be forwarded
    Forward,
}

impl RequestId {
    /// Serializes the ID to a string and prepends Tag
    pub fn tag(&self, tag: Tag) -> RequestId {
        let tag = match tag {
            Tag::Port(port) => format!("port:{port}"),
            Tag::Drop => "drop".into(),
            Tag::Forward => "forward".into(),
        };
        let id = match self {
            RequestId::Number(number) => format!("n:{number}"),
            RequestId::String(string) => format!("s:{string}"),
        };
        RequestId::String(format!("{tag}:{id}"))
    }

    // Attempts to parse Tag out of the ID
    pub fn untag(&self) -> (Option<Tag>, RequestId) {
        fn parse_inner_id(input: &str) -> Result<RequestId> {
            let (value_type, serialized_id) = input.split_once(':').context("missing `:`")?;
            Ok(match value_type {
                "n" => RequestId::Number(serialized_id.parse().context("invalid numeric ID")?),
                "s" => RequestId::String(serialized_id.to_owned()),
                _ => bail!("invalid tag type `{value_type}`"),
            })
        }

        fn parse_port(input: &str) -> Result<(u16, &str)> {
            let (port, rest) = input.split_once(':').context("missing`:`")?;
            let port = u16::from_str(port).context("invalid port number")?;
            Ok((port, rest))
        }

        fn parse_tag(input: &RequestId) -> Result<(Tag, RequestId)> {
            let RequestId::String(input) = input else {
                bail!("tagged id must be a String found `{input:?}`");
            };

            if let Some(rest) = input.strip_prefix("port:") {
                let (port, rest) = parse_port(rest)?;
                let inner_id = parse_inner_id(rest).context("failed to parse inner ID")?;
                return Ok((Tag::Port(port), inner_id));
            }

            if let Some(rest) = input.strip_prefix("drop:") {
                let inner_id = parse_inner_id(rest).context("failed to parse inner ID")?;
                return Ok((Tag::Drop, inner_id));
            }

            if let Some(rest) = input.strip_prefix("forward:") {
                let inner_id = parse_inner_id(rest).context("failed to parse inner ID")?;
                return Ok((Tag::Forward, inner_id));
            }

            bail!("unrecognized prefix: {input:?}");
        }

        match parse_tag(self) {
            Ok((tag, inner_id)) => (Some(tag), inner_id),
            Err(err) => {
                warn!(?err, "invalid tagged ID");
                (None, self.clone())
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LspMuxOptions {
    /// Version number of the protocol
    ///
    /// Version is for now naively checked for equality with
    /// [`PROTOCOL_VERSION`](LspMuxOptions::PROTOCOL_VERSION), the server will
    /// refuse connections to mismatched clients.
    pub version: String,

    #[serde(flatten)]
    pub method: Request,
}

impl LspMuxOptions {
    /// Protocol version
    ///
    /// This doesn't match the crate version, it starts at `"1"` and will only
    /// increase if we make a backwards-incompatible change.
    pub const PROTOCOL_VERSION: &'static str = "1";
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "method")]
#[serde(rename_all = "camelCase")]
pub enum Request {
    /// Connect to a language server
    Connect {
        /// The language server to run
        ///
        /// Can be either an absolute path like `/usr/local/bin/rust-analyzer` or a
        /// plain name like `rust-analyzer` which will then be resolved according to
        /// the *server's* path.
        server: String,

        /// Arguments which will be passed to the language server, defaults to an
        /// empty list if omited.
        #[serde(default = "Vec::new")]
        args: Vec<String>,

        /// Current working directory of the proxy command. This is only used as
        /// fallback if the client doesn't provide any workspace root.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },

    /// List instances and connected clients
    Status {},

    /// Reload an instance
    ///
    /// For rust-analyzer send the `rust-analyzer/reloadWorkspace` extension request.
    /// Do nothing for other language servers.
    Reload {
        /// Selects instance with the longest path where
        /// `cwd.starts_with(workspace_root)` is true
        cwd: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub instances: Vec<Instance>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub pid: u32,
    pub server: String,
    pub args: Vec<String>,
    pub workspace_root: String,
    pub registered_dyn_capabilities: Vec<String>,
    pub last_used: i64,
    pub clients: Vec<Client>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub port: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StopResponse {
    pub instance: Instance,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde_json::{from_value, json, to_value, Value};

    use crate::lsp::InitializationOptions;

    fn test<T>(input: Value)
    where
        T: Serialize + DeserializeOwned,
    {
        let deserialized = from_value::<T>(input.clone()).expect("failed to deserialize");
        let serialized = to_value(&deserialized).expect("failed to serialize");
        assert_eq!(input, serialized);
    }

    #[test]
    fn lsp_mux_only() {
        test::<InitializationOptions>(json!({
            "lspMux": {
                "version": "1",
                "method": "connect",
                "server": "some-language-server",
                "args": ["a", "b", "c"],
                "cwd": "/home/user",
            }
        }))
    }

    #[test]
    fn lsp_mux_and_other_stuff() {
        test::<InitializationOptions>(json!({
            "lspMux": {
                "version": "1",
                "method": "connect",
                "server": "some-language-server",
                "args": ["a", "b", "c"],
            },
            "lsp_mux": "not the right key",
            "lspmux": "also not it",
            "lsp mux": "wrong one",
            "a": 1,
            "b": null,
            "c": {},
            "d": [],
        }))
    }

    #[test]
    #[should_panic = "missing field `version`"]
    fn missing_version() {
        test::<InitializationOptions>(json!({
            "lspMux": {
                "method": "connect",
                "server": "some-language-server",
                "args": ["a", "b", "c"],
            },
        }))
    }

    #[test]
    #[should_panic = "missing field `method`"]
    fn missing_method() {
        test::<InitializationOptions>(json!({
            "lspMux": {
                "version": "1",
                "server": "some-language-server",
                "args": ["a", "b", "c"],
            },
        }))
    }

    #[test]
    #[should_panic = "missing field `server`"]
    fn missing_server() {
        test::<InitializationOptions>(json!({
            "lspMux": {
                "version": "1",
                "method": "connect",
                "args": ["a", "b", "c"],
            },
        }))
    }
}

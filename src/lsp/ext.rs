//! LSP-mux (ra-multiplex) specific protocol extensions

use serde_derive::{Deserialize, Serialize};

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
    /// Returns instances and connected clients
    Status {},
    /// Stop an instance
    Stop {
        /// Stops an instance with the longest path where
        /// `workspace_root.starts_with(cwd)` is true
        cwd: String,

        /// Only returns which instance would be stopped
        dry_run: bool,
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

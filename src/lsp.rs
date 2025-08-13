//! ## Language server protocol
//!
//! Specification can be found at
//! <https://microsoft.github.io/language-server-protocol/specifications/specification-current/>.
//!
//! We're not interested in supporting or even parsing the whole protocol, we only want a subset
//! that will allow us to mupltiplex messages between multiple clients and a single server.
//!
//! LSP has several main message types:
//!
//! ### Request Message
//! Requests from client to server. Requests contain an `id` property which is either `integer` or
//! `string`.
//!
//! ### Response Message
//! Responses from server for client requests. Also contain an `id` property, but according to the
//! the specification it can also be null, it's unclear what we should do when it is null. We could
//! either send the response to all clients or drop it.
//!
//! ### Notification Message
//! Notifications must not receive a response, this doesn't really mean anything to us as we're
//! just relaying the messages. It sounds like it'd allow us to simply pass a notification from any
//! client to the server and to pass a server notification to all clients, however there are some
//! subtypes of notifications defined by the LSP where that could be confusing to the client or
//! server:
//! - Cancel notifications - contains an `id` property again, so we could multiplex this like any
//!   other request
//! - Progress notifications - contains a `token` property which could be used to identify the
//!   client but the specification also says it has nothing to do with the request IDs

use serde::{Deserialize, Serialize};

macro_rules! impl_json_debug {
    ( $($type:ty),* $(,)? ) => {
        $(
            impl ::std::fmt::Debug for $type {
                fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                    let json = ::serde_json::to_string(self).expect("BUG: invalid message");
                    f.write_str(&json)
                }
            }
        )*
    };
}

pub mod ext;
pub mod jsonrpc;
pub mod transport;

/// Params for the `initialize` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub process_id: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,

    pub root_uri: Option<String>,

    /// User provided initialization options
    ///
    /// This is where lspmux-proxy should be inserting it's setup. However we
    /// should remove them again before forwarding them to the language server.
    pub initialization_options: Option<InitializationOptions>,

    pub capabilities: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceValue>,

    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub workspace_folders: Vec<WorkspaceFolder>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializationOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp_mux: Option<ext::LspMuxOptions>,

    #[serde(flatten)]
    pub other_options: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub enum TraceValue {
    Off,
    Messages,
    Verbose,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    capabilities: serde_json::Value,

    #[serde(skip_serializing_if = "Option::is_none")]
    server_info: Option<ServerInfo>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerInfo {
    name: String,
    version: Option<String>,
}

/// Params for `client/registerCapability` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationParams {
    pub registrations: Vec<Registration>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub id: String,
    pub method: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_options: Option<serde_json::Value>,
}

/// Params for `client/unregisterCapability` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UnregistrationParams {
    /// This is a typo in the LSP 3.xx spec, we need to replicate it until they
    /// upgrade the protocol version.
    #[serde(rename = "unregisterations")]
    pub unregistrations: Vec<Unregistration>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Unregistration {
    pub id: String,
    pub method: String,
}

/// Params for `textDocument/didOpen` notification
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: u64,
    pub text: String,
}

/// Params for `textDocument/didClose` notification
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

//! JSON-RPC 2.0 support
//!
//! Support for the JSON-RPC 2.0 protocol as definied here
//! <https://www.jsonrpc.org/specification>. With the exception of batching which
//! is handled in [`read_message`](super::transport::LspReader::read_message).

use std::fmt;

use serde_derive::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    ResponseError(ResponseError),
    ResponseSuccess(ResponseSuccess),
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub id: RequestId,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ResponseError {
    pub jsonrpc: Version,
    pub error: Error,
    pub id: RequestId,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ResponseSuccess {
    pub jsonrpc: Version,
    #[serde(default)]
    pub result: serde_json::Value,
    pub id: RequestId,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Params {
    ByPosition(Vec<serde_json::Value>),
    ByName(serde_json::Map<String, serde_json::Value>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
    // It can also be null but we intentionally skip those as "invalid"
}

impl<S> PartialEq<S> for RequestId
where
    S: AsRef<str>,
{
    fn eq(&self, rhs: &S) -> bool {
        matches!(self, RequestId::String(lhs) if lhs == rhs.as_ref())
    }
}

/// ZST representation of the `"2.0"` version string
#[derive(Clone, Copy)]
pub struct Version;

impl serde::ser::Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

impl<'de> serde::de::Visitor<'de> for Version {
    type Value = Version;

    fn expecting(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.write_str(r#"string value "2.0""#)
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match v {
            "2.0" => Ok(Version),
            _ => Err(E::custom("unsupported JSON-RPC version")),
        }
    }
}

impl<'de> serde::de::Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(Version)
    }
}

impl From<Request> for Message {
    fn from(value: Request) -> Self {
        Message::Request(value)
    }
}

impl From<Notification> for Message {
    fn from(value: Notification) -> Self {
        Message::Notification(value)
    }
}

impl From<ResponseSuccess> for Message {
    fn from(value: ResponseSuccess) -> Self {
        Message::ResponseSuccess(value)
    }
}

impl From<ResponseError> for Message {
    fn from(value: ResponseError) -> Self {
        Message::ResponseError(value)
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let json = serde_json::to_string(self).expect("BUG: invalid message");
        f.write_str(&json)
    }
}

#[derive(Debug)]
pub struct NotResponseError(pub Message);

impl Message {
    pub fn into_response(self) -> Result<Result<ResponseSuccess, ResponseError>, NotResponseError> {
        match self {
            Message::ResponseSuccess(success) => Ok(Ok(success)),
            Message::ResponseError(error) => Ok(Err(error)),
            other => Err(NotResponseError(other)),
        }
    }
}

impl fmt::Display for NotResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "expected Response* Message found {msg:?}",
            msg = &self.0,
        ))
    }
}

impl std::error::Error for NotResponseError {}

#[cfg(test)]
mod tests {
    use serde_json::{from_value, json, to_value, Value};

    use super::*;

    fn test(input: Value) {
        let deserialized = from_value::<Message>(input.clone()).expect("failed to deserialize");
        let serialized = to_value(&deserialized).expect("failed to serialize");
        assert_eq!(input, serialized);
    }

    #[test]
    fn call_with_positional_parameters() {
        test(json!({
            "jsonrpc": "2.0",
            "method": "subtract",
            "params": [42, 23],
            "id": 1,
        }))
    }

    #[test]
    fn call_with_named_parameters() {
        test(json!({
            "jsonrpc": "2.0",
            "method": "subtract",
            "params": { "minuend": 42, "subtrahend": 23 },
            "id": 3,
        }))
    }

    #[test]
    fn response() {
        test(json!({
            "jsonrpc": "2.0",
            "result": 19,
            "id": 1,
        }))
    }

    #[test]
    fn notification() {
        test(json!({
            "jsonrpc": "2.0",
            "method": "update",
            "params": [1, 2, 3, 4, 5],
        }))
    }

    #[test]
    fn error() {
        test(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32601,
                "message": "Method not found",
            },
            "id": "1",
        }))
    }
}

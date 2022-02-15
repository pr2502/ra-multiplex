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

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::fmt::{self, Debug};
use std::io::{self, ErrorKind};
use std::str;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Every message begins with a HTTP-style header
///
/// Headers are terminated by `\r\n` sequence and the final header is followed by another `\r\n`.
/// The currently recognized headers are `content-type` which is optional and contains a `string`
/// (something like a MIME-type) and `content-length` which contains the lenght of the message body
/// after the final `\r\n` of the header. Header names and values are separated by `: `.
pub struct Header {
    pub content_length: usize,
    pub content_type: Option<String>,
}

impl Header {
    pub async fn from_reader<R: AsyncBufRead + Unpin>(
        buffer: &mut Vec<u8>,
        mut reader: R,
    ) -> Result<Option<Self>> {
        let mut content_type = None;
        let mut content_length = None;

        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut *buffer).await {
                Ok(0) => return Ok(None), // EOF
                Ok(_) => {}
                Err(err) => match err.kind() {
                    // reader is closed for some reason, no need to log an error about it
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => return Ok(None),
                    _ => bail!(err),
                },
            }
            let header_text = buffer
                .strip_suffix(b"\r\n")
                .context("malformed header, missing \\r\\n")?;
            let header_text = str::from_utf8(header_text).context("malformed header")?;

            if header_text.is_empty() {
                // headers are separated by an empty line from the body
                break;
            }
            let (name, value) = match header_text.split_once(": ") {
                Some(split) => split,
                None => bail!("malformed header, missing value separator: {}", header_text),
            };

            match name.to_ascii_lowercase().as_str() {
                "content-type" => {
                    ensure!(content_type.is_none(), "repeated header content-type");
                    content_type = Some(value.to_owned());
                }
                "content-length" => {
                    ensure!(content_length.is_none(), "repeated header content-length");
                    content_length = Some(value.parse::<usize>().context("content-length header")?);
                }
                _ => bail!("unknown header: {name}"),
            }
        }

        let content_length = content_length.context("missing required header content-length")?;
        Ok(Some(Header {
            content_length,
            content_type,
        }))
    }
}

/// reads one LSP message from a reader, deserializes it and leaves the serialized body of the
/// message in `buffer`
pub async fn read_message<R>(
    mut reader: R,
    buffer: &mut Vec<u8>,
) -> Result<Option<(Map<String, Value>, &[u8])>>
where
    R: AsyncBufRead + Unpin,
{
    let header = Header::from_reader(&mut *buffer, &mut reader)
        .await
        .context("parsing header")?;
    let header = match header {
        Some(header) => header,
        None => return Ok(None),
    };

    buffer.clear();
    buffer.resize(header.content_length, 0);
    if let Err(err) = reader.read_exact(&mut *buffer).await {
        match err.kind() {
            // reader is closed for some reason, no need to log an error about it
            ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe => return Ok(None),
            _ => bail!(err),
        }
    }

    let bytes = buffer.as_slice();
    let json = serde_json::from_slice(bytes).context("invalid body")?;
    Ok(Some((json, bytes)))
}

/// LSP messages
#[derive(Clone)]
pub struct Message {
    bytes: Arc<[u8]>,
}

impl Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Message")
    }
}

impl Message {
    /// construct a message from a byte buffer, should only contain the message body - no headers
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: Arc::from(bytes),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &*self.bytes
    }

    /// construct a message from a serializable value, like JSON
    pub fn from_json(json: &impl Serialize, buffer: &mut Vec<u8>) -> Self {
        buffer.clear();
        serde_json::to_writer(&mut *buffer, json).expect("invalid json");
        Self::from_bytes(&*buffer)
    }

    /// serialize LSP message into a writer, prepending the appropriate content-length header
    pub async fn to_writer<W>(&self, mut writer: W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", self.bytes.len()).as_bytes())
            .await?;
        writer.write_all(&*self.bytes).await?;
        writer.flush().await
    }
}

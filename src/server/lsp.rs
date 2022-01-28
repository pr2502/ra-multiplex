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
use std::str;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Every message begins with a HTTP-style header
///
/// Headers are terminated by `\r\n` sequence and the final header is followed by another `\r\n`.
/// The currently recognized headers are `Content-Type` which is optional and contains a `string`
/// (something like a MIME-type) and `Content-Length` which contains the lenght of the message body
/// after the final `\r\n` of the header. Header names and values are separated by `: `.
///
/// TODO
/// - we're parsing the headers case-sensitively, it's posible they're case insensitive like HTTP
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
            let bytes_read = reader
                .read_until(b'\n', &mut *buffer)
                .await
                .context("reading header")?;
            if bytes_read == 0 { return Ok(None); }
            let header_text = buffer
                .strip_suffix(b"\r\n")
                .context("malformed header, missing \\r\\n")?;

            if header_text.is_empty() {
                // header is separated by an empty line
                break;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Type: ") {
                ensure!(content_type.is_none(), "repeated content-type header");
                content_type = Some(
                    str::from_utf8(value)
                        .context("content-type header")?
                        .to_owned(),
                );
                continue;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Length: ") {
                ensure!(content_length.is_none(), "repeated content-length header");
                content_length = Some(
                    str::from_utf8(value)
                        .context("content-length header")?
                        .parse::<usize>()
                        .context("content-length header")?,
                );
                continue;
            }
            bail!("invalid header: {}", String::from_utf8_lossy(header_text));
        }

        let content_length = content_length.context("missing content-length header")?;
        Ok(Some(Header {
            content_length,
            content_type,
        }))
    }
}

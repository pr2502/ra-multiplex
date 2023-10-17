use std::fmt::{self, Debug};
use std::io::{self, ErrorKind};
use std::str;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct LspReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

/// Every message begins with a HTTP-style header
///
/// Headers are terminated by `\r\n` sequence and the final header is followed by another `\r\n`.
/// The currently recognized headers are `content-type` which is optional and contains a `string`
/// (something like a MIME-type) and `content-length` which contains the length of the message body
/// after the final `\r\n` of the header. Header names and values are separated by `: `.
pub struct Header {
    pub content_length: usize,
    pub content_type: Option<String>,
}

impl<R> LspReader<R>
where
    R: AsyncBufRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        LspReader {
            reader,
            buffer: Vec::with_capacity(1024),
        }
    }

    async fn read_header(&mut self) -> Result<Option<Header>> {
        let mut content_type = None;
        let mut content_length = None;

        loop {
            self.buffer.clear();
            match self.reader.read_until(b'\n', &mut self.buffer).await {
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
            let header_text = self
                .buffer
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

    /// reads one LSP message from a reader, deserializes it and leaves the serialized body of the
    /// message in `buffer`
    pub async fn read_message(&mut self) -> Result<Option<(Map<String, Value>, &[u8])>> {
        let header = self.read_header().await.context("parsing header")?;
        let header = match header {
            Some(header) => header,
            None => return Ok(None),
        };

        self.buffer.clear();
        self.buffer.resize(header.content_length, 0);
        if let Err(err) = self.reader.read_exact(&mut self.buffer).await {
            match err.kind() {
                // reader is closed for some reason, no need to log an error about it
                ErrorKind::UnexpectedEof
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe => return Ok(None),
                _ => bail!(err),
            }
        }

        let bytes = self.buffer.as_slice();
        let json = serde_json::from_slice(bytes).context("invalid body")?;
        Ok(Some((json, bytes)))
    }
}

pub struct LspWriter<W> {
    writer: W,
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

impl<W> LspWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        LspWriter { writer }
    }

    /// serialize LSP message into a writer, prepending the appropriate content-length header
    pub async fn write_message(&mut self, message: Message) -> io::Result<()> {
        self.writer
            .write_all(format!("Content-Length: {}\r\n\r\n", message.bytes.len()).as_bytes())
            .await?;
        self.writer.write_all(&message.bytes).await?;
        self.writer.flush().await
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
        &self.bytes
    }

    /// construct a message from a serializable value, like JSON
    pub fn from_json(json: &impl Serialize, buffer: &mut Vec<u8>) -> Self {
        buffer.clear();
        serde_json::to_writer(&mut *buffer, json).expect("invalid json");
        Self::from_bytes(&*buffer)
    }
}

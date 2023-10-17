use std::io::{self, ErrorKind};
use std::str;

use anyhow::{bail, ensure, Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::lsp::jsonrpc::Message;

pub struct LspReader<R> {
    reader: R,
    batch: Vec<Message>,
    buffer: Vec<u8>,
}

/// Every message begins with a HTTP-style header
///
/// Headers are terminated by `\r\n` sequence and the final header is followed by another `\r\n`.
/// The currently recognized headers are `content-type` which is optional and contains a `string`
/// (something like a MIME-type) and `content-length` which contains the length of the message body
/// after the final `\r\n` of the header. Header names and values are separated by `: `.
///
/// While we parse the `content-type` header ignore it completely and we don't forward it,
/// expecting both the server and client to assume the default.
///
/// For mor details see <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#headerPart>.
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
            batch: Vec::new(),
            buffer: Vec::with_capacity(1024),
        }
    }

    pub async fn read_header(&mut self) -> Result<Option<Header>> {
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
                .context(r"malformed header, missing `\r\n` terminator")?;
            let header_text = str::from_utf8(header_text)
                .context("malformed header, ascii encoding is a subset of utf-8")?;

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
                _ => bail!("unknown header name: {name:?}"),
            }
        }

        let content_length = content_length.context("missing required header content-length")?;
        Ok(Some(Header {
            content_length,
            content_type,
        }))
    }

    /// Read one message
    ///
    /// Returns `None` if the reader was closed and it'll never return another
    /// message after the first `None`.
    ///
    /// Batch messages are transparently split into individual messages and
    /// delivered in order.
    pub async fn read_message(&mut self) -> Result<Option<Message>> {
        // return pending messages until the last batch is drained
        if let Some(pending) = self.batch.pop() {
            return Ok(Some(pending));
        }

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
        let body = str::from_utf8(bytes)
            .with_context(|| {
                let lossy_utf8 = String::from_utf8_lossy(bytes);
                format!("parsing body `{lossy_utf8}`")
            })
            .context("parsing LSP message")?;

        // handle batches
        if body.starts_with('[') {
            self.batch = serde_json::from_str(body)
                .with_context(|| format!("parsing body `{body}`"))
                .context("parsing LSP message")?;
            // we're popping the messages from the end of the vec
            self.batch.reverse();
            let message = self.batch.pop().context("received an empty batch")?;
            Ok(Some(message))
        } else {
            let message = serde_json::from_str(body)
                .with_context(|| format!("parsing body `{body}`"))
                .context("parsing LSP message")?;
            Ok(Some(message))
        }
    }
}

pub struct LspWriter<W> {
    writer: W,
    buffer: Vec<u8>,
}

impl<W> LspWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        LspWriter {
            writer,
            buffer: Vec::with_capacity(1024),
        }
    }

    /// serialize LSP message into a writer, prepending the appropriate content-length header
    pub async fn write_message(&mut self, message: &Message) -> io::Result<()> {
        self.buffer.clear();
        serde_json::to_writer(&mut self.buffer, message).expect("BUG: invalid message");

        self.writer
            .write_all(format!("Content-Length: {}\r\n\r\n", self.buffer.len()).as_bytes())
            .await?;
        self.writer.write_all(&self.buffer).await?;
        self.writer.flush().await
    }
}

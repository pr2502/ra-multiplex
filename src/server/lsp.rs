use anyhow::{bail, ensure, Context, Result};
use std::str;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub struct Header {
    pub content_length: usize,
    pub content_type: Option<String>,
}

impl Header {
    pub async fn from_reader<R: AsyncBufRead + Unpin>(
        buffer: &mut Vec<u8>,
        mut reader: R,
    ) -> Result<Header> {
        let mut content_type = None;
        let mut content_length = None;

        loop {
            buffer.clear();
            reader
                .read_until(b'\n', &mut *buffer)
                .await
                .context("reading header")?;
            let header_text = buffer
                .strip_suffix(b"\r\n")
                .context("malformed header, missing \\r\\n")?;

            if header_text.is_empty() {
                // header is separated by nothing
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
        Ok(Header {
            content_length,
            content_type,
        })
    }
}

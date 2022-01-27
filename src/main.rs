//! # LSP Multiplexer
//! Some LSP clients are not very smart about spawning the servers, for example coc-rust-analyzer
//! in neovim will spawn a new rust-analyzer instance per neovim instance, unfortunately this
//! wastes a _lot_ of resources.
//!
//! LSP Multiplexer attempts to solve this problem by spawning a single rust-analyzer instance per
//! cargo workspace and routing the messages through TCP to multiple clients.
//!
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

#![feature(stdio_locked)]

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{self, Command, Stdio};
use std::{env, io, str, thread};

use serde_json::{Map, Value};

fn copy_log<R, W>(inp: R, mut out: W, log_path: &str) -> io::Result<()>
where
    W: Write,
    R: Read,
{
    let mut log = File::create(&log_path)?;
    let mut inp = BufReader::new(inp);

    let mut header = Vec::new();
    let mut packet = Vec::new();

    loop {
        let mut content_type = None;
        let mut content_len = None;

        loop {
            // read headers
            header.clear();
            inp.read_until(b'\n', &mut header)?;
            let header_text = header
                .strip_suffix(b"\r\n")
                .expect("malformed header, missing \\r\\n");

            if header_text.is_empty() {
                // header is separated by nothing
                break;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Type: ") {
                content_type = Some(value.to_owned());
                continue;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Length: ") {
                content_len = Some(
                    str::from_utf8(value)
                        .expect("invalid utf8")
                        .parse::<usize>()
                        .expect("invalid content length"),
                );
                continue;
            }
            panic!("invalid header: {}", String::from_utf8_lossy(header_text));
        }

        let _ = content_type; // ignore content-type if present
        let content_len = content_len.expect("missing content-length");

        packet.resize(content_len, 0);
        inp.read_exact(&mut packet)?;

        let json: Map<String, Value> = serde_json::from_slice(&packet).expect("invalid packet");
        if let Some(id) = json.get("id") {
            log.write_fmt(format_args!("{:?} -- ", id))?;
        }

        log.write_all(&packet)?;
        log.write_all(b"\n")?;
        log.flush()?;

        out.write_fmt(format_args!("Content-Length: {}\r\n\r\n", content_len))?;
        out.write_all(&packet)?;
        out.flush()?;
    }
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();

    let child = Command::new("rust-analyzer")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("cannot spawn rust-analyzer");

    let child_stdin = child.stdin.unwrap();
    let child_stdout = child.stdout.unwrap();
    let pid = process::id();

    let t1 = thread::spawn(move || {
        copy_log(
            child_stdout,
            io::stdout_locked(),
            &format!("/tmp/ra-multiplex.{pid}.out"),
        )
        .unwrap()
    });

    let t2 = thread::spawn(move || {
        copy_log(
            io::stdin_locked(),
            child_stdin,
            &format!("/tmp/ra-multiplex.{pid}.inp"),
        )
        .unwrap()
    });

    t1.join().unwrap();
    t2.join().unwrap();
}

#![feature(stdio_locked)]

use anyhow::{Context, Result};
use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::thread;
use ra_multiplex::{ProtoInit, PORT};

fn main() -> Result<()> {
    let mut stream =
        TcpStream::connect((Ipv4Addr::new(127, 0, 0, 1), PORT)).context("connect")?;

    serde_json::to_writer(&mut stream, &ProtoInit::new())
        .context("sending protocol initialization")?;
    stream
        .write_all(b"\0")
        .context("sending protocol initialization")?;

    let stream2 = stream.try_clone().context("duplicate tcp stream")?;
    let t1 = thread::spawn(move || {
        let mut stream = stream2;
        io::copy(&mut io::stdin_locked(), &mut stream).expect("io error");
    });
    io::copy(&mut stream, &mut io::stdout_locked()).expect("io error");
    t1.join().unwrap();
    Ok(())
}

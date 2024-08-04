#[cfg(target_family = "unix")]
use std::fs;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, net};

use anyhow::{Context as _, Result};
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{tcp, TcpListener, TcpStream};
#[cfg(target_family = "unix")]
use tokio::net::{unix, UnixListener, UnixStream};

use crate::config::Address;

pub enum SocketAddr {
    Ip(#[allow(dead_code)] net::SocketAddr),
    #[cfg(target_family = "unix")]
    Unix(#[allow(dead_code)] tokio::net::unix::SocketAddr),
}

impl From<net::SocketAddr> for SocketAddr {
    fn from(val: net::SocketAddr) -> Self {
        SocketAddr::Ip(val)
    }
}

#[cfg(target_family = "unix")]
impl From<tokio::net::unix::SocketAddr> for SocketAddr {
    fn from(val: tokio::net::unix::SocketAddr) -> Self {
        SocketAddr::Unix(val)
    }
}

#[cfg(target_family = "unix")]
pin_project! {
    #[project = OwnedReadHalfProj]
    pub enum OwnedReadHalf {
        Tcp{#[pin] tcp: tcp::OwnedReadHalf},
        Unix{#[pin] unix: unix::OwnedReadHalf},
    }
}
#[cfg(not(target_family = "unix"))]
pin_project! {
    #[project = OwnedReadHalfProj]
    pub enum OwnedReadHalf {
        Tcp{#[pin] tcp: tcp::OwnedReadHalf},
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            OwnedReadHalfProj::Tcp { tcp } => tcp.poll_read(cx, buf),
            #[cfg(target_family = "unix")]
            OwnedReadHalfProj::Unix { unix } => unix.poll_read(cx, buf),
        }
    }
}

#[cfg(target_family = "unix")]
pin_project! {
    #[project = OwnedWriteHalfProj]
    pub enum OwnedWriteHalf {
        Tcp{#[pin] tcp: tcp::OwnedWriteHalf},
        Unix{#[pin] unix: unix::OwnedWriteHalf},
    }
}
#[cfg(not(target_family = "unix"))]
pin_project! {
    #[project = OwnedWriteHalfProj]
    pub enum OwnedWriteHalf {
        Tcp{#[pin] tcp: tcp::OwnedWriteHalf},
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            OwnedWriteHalfProj::Tcp { tcp } => tcp.poll_write(cx, buf),
            #[cfg(target_family = "unix")]
            OwnedWriteHalfProj::Unix { unix } => unix.poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            OwnedWriteHalfProj::Tcp { tcp } => tcp.poll_write_vectored(cx, bufs),
            #[cfg(target_family = "unix")]
            OwnedWriteHalfProj::Unix { unix } => unix.poll_write_vectored(cx, bufs),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            OwnedWriteHalfProj::Tcp { tcp } => tcp.poll_flush(cx),
            #[cfg(target_family = "unix")]
            OwnedWriteHalfProj::Unix { unix } => unix.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            OwnedWriteHalfProj::Tcp { tcp } => tcp.poll_shutdown(cx),
            #[cfg(target_family = "unix")]
            OwnedWriteHalfProj::Unix { unix } => unix.poll_shutdown(cx),
        }
    }
}

#[cfg(target_family = "unix")]
pin_project! {
    #[project = StreamProj]
    pub enum Stream {
        Tcp{#[pin] tcp: TcpStream},
        Unix{#[pin] unix: UnixStream},
    }
}
#[cfg(not(target_family = "unix"))]
pin_project! {
    #[project = StreamProj]
    pub enum Stream {
        Tcp{#[pin] tcp: TcpStream},
    }
}

impl Stream {
    pub async fn connect(addr: &Address) -> Result<Stream> {
        match addr {
            Address::Tcp(ip_addr, port) => TcpStream::connect((*ip_addr, *port))
                .await
                .with_context(|| format!("connecting to tcp socket {ip_addr}:{port}"))
                .map(|tcp| Stream::Tcp { tcp }),
            #[cfg(target_family = "unix")]
            Address::Unix(path) => UnixStream::connect(path)
                .await
                .with_context(|| format!("connecting to unix socket {path:?}"))
                .map(|unix| Stream::Unix { unix }),
        }
    }

    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        match self {
            Stream::Tcp { tcp } => {
                let (read, write) = tcp.into_split();
                (
                    OwnedReadHalf::Tcp { tcp: read },
                    OwnedWriteHalf::Tcp { tcp: write },
                )
            }
            #[cfg(target_family = "unix")]
            Stream::Unix { unix } => {
                let (read, write) = unix.into_split();
                (
                    OwnedReadHalf::Unix { unix: read },
                    OwnedWriteHalf::Unix { unix: write },
                )
            }
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            StreamProj::Tcp { tcp } => tcp.poll_read(cx, buf),
            #[cfg(target_family = "unix")]
            StreamProj::Unix { unix } => unix.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            StreamProj::Tcp { tcp } => tcp.poll_write(cx, buf),
            #[cfg(target_family = "unix")]
            StreamProj::Unix { unix } => unix.poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            StreamProj::Tcp { tcp } => tcp.poll_write_vectored(cx, bufs),
            #[cfg(target_family = "unix")]
            StreamProj::Unix { unix } => unix.poll_write_vectored(cx, bufs),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            StreamProj::Tcp { tcp } => tcp.poll_flush(cx),
            #[cfg(target_family = "unix")]
            StreamProj::Unix { unix } => unix.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            StreamProj::Tcp { tcp } => tcp.poll_shutdown(cx),
            #[cfg(target_family = "unix")]
            StreamProj::Unix { unix } => unix.poll_shutdown(cx),
        }
    }
}

pub enum Listener {
    Tcp(TcpListener),
    #[cfg(target_family = "unix")]
    Unix(UnixListener),
}

impl Listener {
    pub async fn bind(addr: &Address) -> Result<Listener> {
        match addr {
            Address::Tcp(ip_addr, port) => TcpListener::bind((*ip_addr, *port))
                .await
                .with_context(|| format!("binding to tcp socket {ip_addr}:{port}"))
                .map(Listener::Tcp),
            #[cfg(target_family = "unix")]
            Address::Unix(path) => {
                match fs::remove_file(path) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("removing old unix socket file {path:?}"))
                    }
                }
                UnixListener::bind(path)
                    .with_context(|| format!("binding to unix socket {path:?}"))
                    .map(Listener::Unix)
            }
        }
    }

    pub async fn accept(&self) -> io::Result<(Stream, SocketAddr)> {
        match self {
            Listener::Tcp(tcp) => {
                let (stream, addr) = tcp.accept().await?;
                Ok((Stream::Tcp { tcp: stream }, addr.into()))
            }
            #[cfg(target_family = "unix")]
            Listener::Unix(unix) => {
                let (stream, addr) = unix.accept().await?;
                Ok((Stream::Unix { unix: stream }, addr.into()))
            }
        }
    }
}

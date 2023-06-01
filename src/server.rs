//! # LSP Multiplexer
//! Some LSP clients are not very smart about spawning the servers, for example coc-rust-analyzer
//! in neovim will spawn a new rust-analyzer instance per neovim instance, unfortunately this
//! wastes a _lot_ of resources.
//!
//! LSP Multiplexer attempts to solve this problem by spawning a single rust-analyzer instance per
//! cargo workspace and routing the messages through TCP to multiple clients.

use crate::server::client::Client;
use crate::server::instance::InstanceRegistry;
use anyhow::{Context, Result};
use ra_multiplex::config::Config;
use tokio::net::TcpListener;
use tokio::task;

mod async_once_cell;
mod client;
mod instance;

pub async fn main(dump: bool) -> Result<()> {
    if dump {
        todo!("--dump is not yet implemented");
    }
    let config = Config::load_or_default().await;
    let registry = InstanceRegistry::new().await;

    let listener = TcpListener::bind(config.listen).await.context("listen")?;
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let port = addr.port();
                let registry = registry.clone();

                log::info!("[{port}] client connected");
                task::spawn(async move {
                    match Client::process(socket, port, registry).await {
                        Ok(_) => log::debug!("[{port}] client initialized"),
                        Err(err) => log::error!("[{port}] client error: {err:?}"),
                    }
                });
            }
            Err(err) => match err.kind() {
                // ignore benign errors
                std::io::ErrorKind::NotConnected => {
                    log::warn!("listener error {err}");
                }
                _ => {
                    Err(err).context("accept connection")?;
                }
            },
        }
    }
}

//! # LSP Multiplexer
//! Some LSP clients are not very smart about spawning the servers, for example coc-rust-analyzer
//! in neovim will spawn a new rust-analyzer instance per neovim instance, unfortunately this
//! wastes a _lot_ of resources.
//!
//! LSP Multiplexer attempts to solve this problem by spawning a single rust-analyzer instance per
//! cargo workspace and routing the messages through TCP to multiple clients.

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::task;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::client::Client;
use crate::config::Config;
use crate::instance::InstanceMap;

pub async fn run() -> Result<()> {
    let config = Config::load_or_default().await;
    let instance_map = InstanceMap::new().await;

    let listener = TcpListener::bind(config.listen).await.context("listen")?;
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let port = addr.port();
                let instance_map = instance_map.clone();

                task::spawn(
                    async move {
                        info!("client connected");
                        match Client::process(socket, port, instance_map).await {
                            Ok(_) => debug!("client initialized"),
                            Err(err) => error!("client error: {err:?}"),
                        }
                    }
                    .instrument(info_span!("client", %port)),
                );
            }
            Err(err) => match err.kind() {
                // ignore benign errors
                std::io::ErrorKind::NotConnected => {
                    warn!("listener error {err}");
                }
                _ => {
                    Err(err).context("accept connection")?;
                }
            },
        }
    }
}

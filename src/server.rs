use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::task;
use tracing::{error, info, info_span, warn, Instrument};

use crate::client;
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
                        match client::process(socket, port, instance_map).await {
                            Ok(_) => {}
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

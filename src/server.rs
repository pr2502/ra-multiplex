use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use tokio::task;
use tracing::{error, info, info_span, warn, Instrument};

use crate::client;
use crate::config::Config;
use crate::instance::InstanceMap;
use crate::socketwrapper::Listener;

pub async fn run(config: &Config) -> Result<()> {
    let instance_map = InstanceMap::new(config).await;
    let next_client_id = AtomicUsize::new(0);

    let listener = Listener::bind(&config.listen).await.context("listen")?;
    loop {
        match listener.accept().await {
            Ok((socket, _addr)) => {
                let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
                let instance_map = instance_map.clone();

                task::spawn(
                    async move {
                        info!("client connected");
                        match client::process(socket, client_id, instance_map).await {
                            Ok(_) => {}
                            Err(err) => error!("client error: {err:?}"),
                        }
                    }
                    .instrument(info_span!("client", %client_id)),
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

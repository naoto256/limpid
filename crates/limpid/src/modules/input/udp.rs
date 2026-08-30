//! Shared Tokio UDP receive loop for datagram inputs.

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use super::rate_limit::RateLimiter;
use crate::event::Event;
use crate::metrics::InputMetrics;

pub(super) type DatagramValidator = fn(&[u8]) -> std::result::Result<(), String>;

pub(super) async fn shutdown_change_is_terminal(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

pub(super) struct UdpInputRuntime {
    pub module_name: &'static str,
    pub bind_addr: String,
    pub rate_limit: Option<u64>,
    pub metrics: Arc<InputMetrics>,
    pub validator: Option<DatagramValidator>,
}

impl UdpInputRuntime {
    pub async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let socket = UdpSocket::bind(&self.bind_addr).await?;
        crate::modules::input_startup_ready();
        info!("{} listening on {}", self.module_name, self.bind_addr);

        let limiter = self.rate_limit.map(RateLimiter::new);
        if let Some(rate) = self.rate_limit {
            info!("{} rate_limit: {} events/sec", self.module_name, rate);
        }

        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                biased;

                terminal = shutdown_change_is_terminal(&mut shutdown) => {
                    if terminal {
                        info!("{}: shutting down", self.module_name);
                        break;
                    }
                }

                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, addr)) => {
                            let data = &buf[..len];
                            self.metrics.bytes_received.inc_by(len as u64);

                            if let Some(validator) = self.validator
                                && let Err(error) = validator(data)
                            {
                                warn!(
                                    "{} [{}]: dropping invalid message ({})",
                                    self.module_name, addr, error
                                );
                                self.metrics.events_invalid.inc();
                                continue;
                            }

                            self.metrics.events_received.inc();
                            let event = Event::new(Bytes::copy_from_slice(data), addr);

                            if let Some(ref limiter) = limiter {
                                limiter.acquire().await;
                            }

                            if tx.send(event).await.is_err() {
                                info!(
                                    "{} [{}]: pipeline event channel closed, stopping input task",
                                    self.module_name, addr
                                );
                                break;
                            }
                        }
                        Err(error) => {
                            error!("{} recv error: {}", self.module_name, error);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

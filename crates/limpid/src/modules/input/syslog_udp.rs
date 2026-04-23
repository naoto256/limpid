//! Syslog UDP input: receives syslog messages as UDP datagrams.
//! Invalid PRI messages are dropped with a warning.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use bytes::Bytes;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use super::rate_limit::RateLimiter;
use super::validate::validate_pri;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{FromProperties, HasMetrics, Input};

pub struct SyslogUdpInput {
    pub bind_addr: String,
    pub rate_limit: Option<u64>,
    metrics: Arc<InputMetrics>,
}

impl FromProperties for SyslogUdpInput {
    fn from_properties(_name: &str, properties: &[Property]) -> Result<Self> {
        let bind =
            props::get_string(properties, "bind").unwrap_or_else(|| "0.0.0.0:514".to_string());
        let rate_limit = props::get_strictly_positive_int(properties, "rate_limit")?;
        Ok(Self {
            bind_addr: bind,
            rate_limit,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for SyslogUdpInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for SyslogUdpInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let socket = UdpSocket::bind(&self.bind_addr).await?;
        info!("syslog_udp listening on {}", self.bind_addr);

        let limiter = self.rate_limit.map(RateLimiter::new);
        if let Some(rate) = self.rate_limit {
            info!("syslog_udp rate_limit: {} events/sec", rate);
        }

        let metrics = self.metrics;
        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("syslog_udp: shutting down");
                        break;
                    }
                }

                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, addr)) => {
                            let data = &buf[..len];

                            if let Err(e) = validate_pri(data) {
                                warn!("syslog_udp [{}]: dropping invalid message ({})", addr, e);
                                metrics.events_invalid.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                            metrics.events_received.fetch_add(1, Ordering::Relaxed);

                            if let Some(ref limiter) = limiter {
                                limiter.acquire().await;
                            }

                            let raw = Bytes::copy_from_slice(data);
                            let event = Event::new(raw, addr);
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            error!("syslog_udp recv error: {}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

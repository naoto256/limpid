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
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const SYSLOG_UDP_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "bind",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "rate_limit",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

pub struct SyslogUdpInput {
    pub bind_addr: String,
    pub rate_limit: Option<u64>,
    metrics: Arc<InputMetrics>,
}

impl Module for SyslogUdpInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_UDP_INPUT_SCHEMA)
    }

    fn from_properties(
        _name: &str,
        properties: &crate::modules::ModuleProperties,
        _ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket as StdUdpSocket;

    fn spawn_input(
        bind_addr: String,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::mpsc::Receiver<Event>,
        Arc<InputMetrics>,
    ) {
        let metrics = Arc::new(InputMetrics::default());
        let input = SyslogUdpInput {
            bind_addr,
            rate_limit: None,
            metrics: Arc::clone(&metrics),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { input.run(tx, sd_rx).await });
        (handle, sd_tx, rx, metrics)
    }

    /// Pick an ephemeral port by binding briefly, then releasing.
    fn pick_port() -> u16 {
        let s = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn valid_pri_datagram_arrives_as_event() {
        // End-to-end: a valid `<PRI>...` datagram lands on the mpsc
        // channel with events_received bumped exactly once.
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let (handle, sd_tx, mut rx, metrics) = spawn_input(bind.clone());
        // Let the listener bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"<13>hello", &bind).unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        assert_eq!(&event.ingress[..], b"<13>hello");
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_invalid.load(Ordering::Relaxed), 0);

        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn invalid_pri_datagram_bumps_events_invalid_and_drops() {
        // A non-`<PRI>` datagram is rejected by validate_pri: the
        // events_invalid counter goes up but no Event reaches the
        // channel. A regression that forwarded invalid bytes would
        // bypass the PRI validator silently.
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let (handle, sd_tx, mut rx, metrics) = spawn_input(bind.clone());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"not a valid syslog line", &bind).unwrap();

        // Give the reject path a moment to run; assert NO event.
        let got = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
        assert!(
            got.is_err(),
            "expected no event from invalid datagram, got {got:?}"
        );
        assert_eq!(metrics.events_invalid.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 0);

        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn dropped_receiver_terminates_run_loop() {
        // The accept loop breaks when `tx.send(...).await.is_err()`
        // (receiver dropped). A regression that swallowed the Err
        // and kept looping would tighten into a busy loop on every
        // datagram after receiver drop.
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let (handle, _sd_tx, rx, _metrics) = spawn_input(bind.clone());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the receiver so the next send fails.
        drop(rx);

        // Send a valid datagram so the loop hits the send arm.
        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"<13>x", &bind).unwrap();

        // The run loop should exit within a short bounded window.
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        assert!(
            result.is_ok(),
            "run loop did not terminate after receiver drop"
        );
    }
}

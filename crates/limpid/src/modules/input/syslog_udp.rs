//! Syslog UDP input: receives syslog messages as UDP datagrams.
//! Invalid PRI messages are dropped with a warning.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use anyhow::Result;

use super::udp::UdpInputRuntime;
#[cfg(test)]
use super::udp::shutdown_change_is_terminal;
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
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
        let bind =
            props::get_string(properties, "bind").unwrap_or_else(|| "0.0.0.0:514".to_string());
        let rate_limit = props::get_strictly_positive_int(properties, "rate_limit")?;
        Ok(Self {
            bind_addr: bind,
            rate_limit,
            metrics: InputMetrics::register(&ctx.metrics, name)?,
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
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        UdpInputRuntime {
            module_name: "syslog_udp",
            bind_addr: self.bind_addr,
            rate_limit: self.rate_limit,
            metrics: self.metrics,
            validator: Some(validate_pri),
        }
        .run(tx, shutdown)
        .await
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
        let metrics = InputMetrics::for_testing();
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
        assert_eq!(
            metrics.bytes_received.load(Ordering::Relaxed),
            b"<13>hello".len() as u64
        );

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
        assert_eq!(
            metrics.bytes_received.load(Ordering::Relaxed),
            b"not a valid syslog line".len() as u64,
            "validation must not hide bytes that reached the adapter"
        );

        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn received_at_is_fixed_before_rate_limit_wait() {
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let metrics = InputMetrics::for_testing();
        let input = SyslogUdpInput {
            bind_addr: bind.clone(),
            rate_limit: Some(1),
            metrics,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { input.run(tx, shutdown_rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"<13>first", &bind).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("first event timed out")
            .expect("event channel closed");

        let sent_at = chrono::Utc::now();
        sender.send_to(b"<13>second", &bind).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let before_limiter_release = chrono::Utc::now();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "second event was delivered before the limiter wait"
        );
        let event = tokio::time::timeout(std::time::Duration::from_millis(1100), rx.recv())
            .await
            .expect("limited event did not resume")
            .expect("event channel closed");

        assert_eq!(&event.ingress[..], b"<13>second");
        assert!(event.received_at >= sent_at);
        assert!(
            event.received_at < before_limiter_release,
            "received_at was sampled after the limiter wait: {} >= {}",
            event.received_at,
            before_limiter_release
        );

        shutdown_tx.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn empty_datagram_is_a_zero_byte_noop() {
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let (handle, sd_tx, _rx, metrics) = spawn_input(bind.clone());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&[], &bind).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(metrics.bytes_received.load(Ordering::Relaxed), 0);

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

    #[tokio::test]
    async fn closed_shutdown_watch_is_terminal_for_syslog_udp_loop() {
        let (sender, mut receiver) = tokio::sync::watch::channel(false);
        drop(sender);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                shutdown_change_is_terminal(&mut receiver),
            )
            .await
            .expect("closed watch must resolve without spinning")
        );
        let marker = ["shutdown_change_is_terminal", "(&mut shutdown)"].concat();
        assert!(include_str!("udp.rs").contains(&marker));

        let (handle, shutdown, _events, _metrics) =
            spawn_input(format!("127.0.0.1:{}", pick_port()));
        drop(shutdown);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("actual UDP loop must terminate on a closed watch")
            .expect("UDP task must join")
            .expect("UDP loop must exit cleanly");
    }
}

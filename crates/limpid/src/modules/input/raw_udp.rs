//! Raw UDP input: maps every UDP datagram to one byte-exact event.

use std::sync::Arc;

use anyhow::Result;

use super::udp::UdpInputRuntime;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const RAW_UDP_INPUT_SCHEMA: &[PropertySpec] = &[
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

pub struct RawUdpInput {
    pub bind_addr: String,
    pub rate_limit: Option<u64>,
    metrics: Arc<InputMetrics>,
}

impl Module for RawUdpInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(RAW_UDP_INPUT_SCHEMA)
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

impl HasMetrics for RawUdpInput {
    type Stats = InputMetrics;

    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for RawUdpInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        UdpInputRuntime {
            module_name: "raw_udp",
            bind_addr: self.bind_addr,
            rate_limit: self.rate_limit,
            metrics: self.metrics,
            validator: None,
        }
        .run(tx, shutdown)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket as StdUdpSocket;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn spawn_input(
        bind_addr: String,
        rate_limit: Option<u64>,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::mpsc::Receiver<Event>,
        Arc<InputMetrics>,
    ) {
        let metrics = InputMetrics::for_testing();
        let input = RawUdpInput {
            bind_addr,
            rate_limit,
            metrics: Arc::clone(&metrics),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { input.run(tx, shutdown_rx).await });
        (handle, shutdown_tx, rx, metrics)
    }

    fn pick_port() -> u16 {
        let socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    }

    async fn wait_for_bind() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn raw_text_without_pri_preserves_payload_source_and_metrics() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, metrics) = spawn_input(bind.clone(), None);
        wait_for_bind().await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let source = sender.local_addr().unwrap();
        let payload = b"CEF:0|vendor|product|1|event|name|5|msg=raw";
        let before = chrono::Utc::now();
        assert_eq!(sender.send_to(payload, &bind).unwrap(), payload.len());
        let event = tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("raw datagram timed out")
            .expect("event channel closed");
        let after = chrono::Utc::now();

        assert_eq!(&event.ingress[..], payload);
        assert_eq!(&event.egress[..], payload);
        assert_eq!(event.source, source);
        assert!(event.received_at >= before && event.received_at <= after);
        assert_eq!(
            metrics.bytes_received.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_invalid.load(Ordering::Relaxed), 0);

        shutdown.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn arbitrary_binary_datagram_with_nul_and_non_utf8_is_byte_exact() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, metrics) = spawn_input(bind.clone(), None);
        wait_for_bind().await;

        let payload = [0x00, 0xff, 0x80, b'<', 0x00, b'>'];
        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&payload, &bind).unwrap();
        let event = tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("binary datagram timed out")
            .expect("event channel closed");

        assert_eq!(&event.ingress[..], &payload);
        assert_eq!(&event.egress[..], &payload);
        assert_eq!(
            metrics.bytes_received.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_invalid.load(Ordering::Relaxed), 0);

        shutdown.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn empty_datagram_is_one_zero_byte_event() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, metrics) = spawn_input(bind.clone(), None);
        wait_for_bind().await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(sender.send_to(&[], &bind).unwrap(), 0);
        let event = tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("empty datagram timed out")
            .expect("event channel closed");

        assert!(event.ingress.is_empty());
        assert!(event.egress.is_empty());
        assert_eq!(metrics.bytes_received.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_invalid.load(Ordering::Relaxed), 0);

        shutdown.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rate_limit_delays_events_after_the_initial_burst() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, _metrics) = spawn_input(bind.clone(), Some(1));
        wait_for_bind().await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"first", &bind).unwrap();
        sender.send_to(b"second", &bind).unwrap();
        tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("first event timed out")
            .expect("event channel closed");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "second event bypassed the configured limiter"
        );
        tokio::time::timeout(Duration::from_millis(1200), events.recv())
            .await
            .expect("limited event did not resume")
            .expect("event channel closed");

        shutdown.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn received_at_is_fixed_before_rate_limit_wait() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, shutdown, mut events, _metrics) = spawn_input(bind.clone(), Some(1));
        wait_for_bind().await;

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"first", &bind).unwrap();
        tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("first event timed out")
            .expect("event channel closed");

        let sent_at = chrono::Utc::now();
        sender.send_to(b"second", &bind).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let before_limiter_release = chrono::Utc::now();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "second event was delivered before the limiter wait"
        );
        let event = tokio::time::timeout(Duration::from_millis(1100), events.recv())
            .await
            .expect("limited event did not resume")
            .expect("event channel closed");

        assert_eq!(&event.ingress[..], b"second");
        assert!(event.received_at >= sent_at);
        assert!(
            event.received_at < before_limiter_release,
            "received_at was sampled after the limiter wait: {} >= {}",
            event.received_at,
            before_limiter_release
        );

        shutdown.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropped_receiver_terminates_run_loop() {
        let bind = format!("127.0.0.1:{}", pick_port());
        let (handle, _shutdown, events, _metrics) = spawn_input(bind.clone(), None);
        wait_for_bind().await;
        drop(events);

        let sender = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"raw", &bind).unwrap();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("raw UDP loop did not terminate after receiver close")
            .expect("raw UDP task did not join")
            .expect("raw UDP task returned an error");
    }

    #[tokio::test]
    async fn closed_shutdown_watch_is_terminal() {
        let (handle, shutdown, _events, _metrics) =
            spawn_input(format!("127.0.0.1:{}", pick_port()), None);
        drop(shutdown);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("raw UDP loop did not stop on a closed watch")
            .expect("raw UDP task did not join")
            .expect("raw UDP task returned an error");
    }
}

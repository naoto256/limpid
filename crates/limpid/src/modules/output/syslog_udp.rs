//! Syslog UDP output: sends event messages to remote syslog UDP endpoints.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

use crate::dsl::ast::Property;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::syslog_peers::{
    PEER_CONNECT_TIMEOUT, Peer, PeerList, PeerSendError, PreSendShutdownMarker, SyslogPayload,
    parse_host_port,
};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

const SYSLOG_UDP_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "host",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "port",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

const SYSLOG_UDP_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(SYSLOG_UDP_PEER_SCHEMA),
}];

const SYSLOG_UDP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_UDP_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_UDP_PEERS_SCHEMA),
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct SyslogUdpOutput {
    name: String,
    peers: PeerList<UdpSocket>,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

impl Module for SyslogUdpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_UDP_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        let properties = properties.user_properties();
        let peers = parse_peers(name, properties)?;
        Ok(Self {
            name: name.to_string(),
            peers: PeerList::new(peers),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
            shutdown_signal: ctx.shutdown_signal.clone(),
        })
    }
}

fn parse_peers(name: &str, properties: &[Property]) -> Result<Vec<Peer>> {
    crate::modules::output::syslog_peers::parse_peer_or_peers(
        name,
        properties,
        |label, peer_props| parse_peer(name, label, peer_props),
    )
}

fn parse_peer(name: &str, label: &str, properties: &[Property]) -> Result<Peer> {
    let label = format!("output '{}': {}", name, label);
    let (host, port) = parse_host_port(properties, 514, &label)?;
    Ok(Peer::new(host, port, None))
}

impl HasMetrics for SyslogUdpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for SyslogUdpOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            let payload = SyslogPayload {
                egress: event.egress.clone(),
            };
            let write_result = match self
                .write_payload_shutdown_aware(payload, &mut shutdown)
                .await
            {
                SyslogUdpWriteOutcome::Delivered => Ok(()),
                SyslogUdpWriteOutcome::Err(e) => Err(e),
                SyslogUdpWriteOutcome::PreSendShutdown => {
                    self.metrics.in_retry.set(0);
                    let reason = format!(
                        "output '{}': write attempt abandoned on shutdown (pre-send)",
                        self.name
                    );
                    let __dlq_outcome = crate::modules::route_event_to_dlq(
                        self.error_log.as_ref(),
                        self.error_log_fallback,
                        &self.metrics,
                        &self.name,
                        event,
                        ack.position(),
                        &reason,
                    )
                    .await;
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
                    return Ok(());
                }
            };
            match write_result {
                Ok(()) => {
                    self.metrics.in_retry.set(0);
                    // `write_payload_shutdown_aware` (and its transport-
                    // only sibling `write_payload`) intentionally do NOT
                    // bump `events_written`; disposition ownership lives
                    // with the caller so the steady-state and shutdown
                    // paths agree on a single "successful event" ==
                    // "one bump" contract. Sibling `syslog_tcp` uses the
                    // identical shape (see the comment at the analogous
                    // `write_payload_shutdown_aware` return site).
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.inc();
                    if attempt >= self.retry.max_attempts {
                        self.metrics.in_retry.set(0);
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    self.metrics.in_retry.set(1);
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    // Race the backoff sleep against shutdown. If the runtime
                    // signals shutdown mid-sleep, do NOT keep retrying — the
                    // retry budget (default 1+2+4+8 = 15 s) can outlast the
                    // runtime's 10 s shutdown budget, and if we don't return
                    // the queue consumer's select! never gets back to its
                    // shutdown arm. Route the pending event to DLQ, resolve
                    // `Recovered`, and return.
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        self.metrics.in_retry.set(0);
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let payload = SyslogPayload {
            egress: event.egress.clone(),
        };
        // Delegate disposition (metric bump + ack resolution + DLQ
        // route) to `finalize_shutdown_singleton_disposition` so the
        // "successful shutdown-drain event == one `events_written`
        // bump" contract is owned by the helper, not the sink.
        // Sibling `syslog_tcp::consume_shutdown` uses the identical
        // shape.
        let result = match tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            self.write_payload(payload),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {:?}",
                crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
            )),
        };
        crate::modules::finalize_shutdown_singleton_disposition(
            result,
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

/// Outcome of a single steady-state syslog_udp send attempt.
enum SyslogUdpWriteOutcome {
    Delivered,
    Err(anyhow::Error),
    PreSendShutdown,
}

impl SyslogUdpOutput {
    /// Shutdown-aware steady-state send. Same lookup / bind / connect
    /// / send flow as [`Self::write_payload`], but wraps the DNS
    /// lookup, socket bind, and UDP `connect` (all in-process or
    /// wire-preparation with no datagram side effect) in
    /// `pre_send_or_shutdown`. The `UdpSocket::send` call itself is
    /// deliberately **not** shutdown-cancellable and **not** wrapped
    /// in a wall-clock timeout: UDP writes are single-datagram (no
    /// partial-write state), the send syscall has a wire-visible
    /// effect (the datagram leaves the process), and there is no
    /// remote-peer wait for the sender to hang on — see the
    /// per-site comment on the send call for the full asymmetry
    /// rationale.
    async fn write_payload_shutdown_aware(
        &self,
        payload: SyslogPayload,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> SyslogUdpWriteOutcome {
        let metrics = Arc::clone(&self.metrics);
        let result = self
            .peers
            .write_with_rotation_shutdown_aware(
                Instant::now(),
                move |_idx, peer, state, shutdown| {
                    let egress = payload.egress.clone();
                    let address = peer.address();
                    let metrics = Arc::clone(&metrics);
                    Box::pin(async move {
                        if state.conn.is_none() {
                            // Pre-send phase: DNS lookup, ephemeral
                            // bind, and UDP `connect` are all
                            // wire-preparation with no datagram
                            // side effect — safe to race against
                            // shutdown.
                            let resolved: Vec<std::net::SocketAddr> =
                                match crate::modules::pre_send_or_shutdown(
                                    shutdown,
                                    tokio::time::timeout(
                                        PEER_CONNECT_TIMEOUT,
                                        tokio::net::lookup_host(address),
                                    ),
                                )
                                .await
                                {
                                    Some(Ok(Ok(iter))) => iter.collect(),
                                    Some(Ok(Err(e))) => {
                                        return Err(anyhow::Error::from(e)
                                            .context(format!("syslog_udp lookup {}", address)));
                                    }
                                    Some(Err(_)) => {
                                        return Err(anyhow::anyhow!(
                                            "syslog_udp lookup {} timed out",
                                            address
                                        ));
                                    }
                                    None => {
                                        return Err(anyhow::Error::new(PreSendShutdownMarker));
                                    }
                                };
                            if resolved.is_empty() {
                                anyhow::bail!(
                                    "syslog_udp: name resolution for {} returned no addresses",
                                    address
                                );
                            }
                            let mut socket: Option<UdpSocket> = None;
                            let mut last_err: Option<anyhow::Error> = None;
                            for addr in &resolved {
                                let bind_addr = match addr {
                                    std::net::SocketAddr::V4(_) => "0.0.0.0:0",
                                    std::net::SocketAddr::V6(_) => "[::]:0",
                                };
                                let candidate = match UdpSocket::bind(bind_addr).await {
                                    Ok(s) => s,
                                    Err(e) => {
                                        last_err = Some(anyhow::Error::from(e).context(format!(
                                            "syslog_udp output: failed to bind ephemeral \
                                                 socket ({})",
                                            bind_addr
                                        )));
                                        continue;
                                    }
                                };
                                // Per-address connect is also
                                // pre-send: `connect(2)` on a UDP
                                // socket only sets the default
                                // peer, no packet is sent.
                                let connect_res = match crate::modules::pre_send_or_shutdown(
                                    shutdown,
                                    tokio::time::timeout(
                                        PEER_CONNECT_TIMEOUT,
                                        candidate.connect(*addr),
                                    ),
                                )
                                .await
                                {
                                    Some(Ok(Ok(()))) => Ok(()),
                                    Some(Ok(Err(e))) => Err(anyhow::Error::from(e).context(
                                        format!("syslog_udp connect to {} ({})", address, addr),
                                    )),
                                    Some(Err(_)) => Err(anyhow::anyhow!(
                                        "syslog_udp connect to {} ({}) timed out",
                                        address,
                                        addr
                                    )),
                                    None => {
                                        return Err(anyhow::Error::new(PreSendShutdownMarker));
                                    }
                                };
                                match connect_res {
                                    Ok(()) => {
                                        socket = Some(candidate);
                                        break;
                                    }
                                    Err(e) => last_err = Some(e),
                                }
                            }
                            let socket = socket.ok_or_else(|| {
                                last_err.unwrap_or_else(|| {
                                    anyhow::anyhow!(
                                        "syslog_udp: no resolved address for {} could be \
                                         connected",
                                        address
                                    )
                                })
                            })?;
                            state.conn = Some(socket);
                        }

                        // Send phase: single-datagram `UdpSocket::send`.
                        //
                        // No timeout wrapper here (asymmetric with
                        // `syslog_tcp`, which does wrap `write_all` in
                        // `PEER_WRITE_TIMEOUT` — see the rationale on
                        // that call site). A connected UDP send has
                        // three terminal states: it succeeds
                        // immediately (datagram enters the kernel
                        // send buffer, the common case), it fails
                        // immediately (ECONNREFUSED / ENETDOWN /
                        // async ICMP surfaced synchronously by the
                        // kernel), or it briefly Pends on writable
                        // when the local send buffer / qdisc is full
                        // and drains within microseconds-to-
                        // milliseconds. There is no "remote peer
                        // stopped reading" state for UDP: unlike TCP,
                        // the sender doesn't wait on the peer at all.
                        // A 10-second wall-clock cap over this
                        // pattern only fires under kernel pathology
                        // (send buffer full and the tokio waker
                        // wedged), and in exchange for that
                        // vanishingly rare protection the wrapper
                        // adds per-event `Sleep` construction and
                        // its scheduler-wake fallout to every
                        // successful send. Profiling the passthrough
                        // workload showed the timer / wake pattern
                        // dominating this hot path.
                        //
                        // Removing the wrapper turns "send buffer
                        // full" into ordinary async backpressure that
                        // flows back through the queue to the input,
                        // which is the correct log-pipeline
                        // behaviour anyway: today a burst that fills
                        // the kernel buffer would time out per event
                        // for 10 s and route DLQ-shaped "Delivered"
                        // failures for events the kernel is about to
                        // drain, which is worse than pausing the
                        // pipeline. Shutdown safety is unaffected
                        // (the queue-consumer task is abort-ed by
                        // the runtime shutdown budget; the ack
                        // handle drops as `Dropped` and a disk
                        // queue holds the cursor).
                        let socket = state.conn.as_mut().expect("connection should be present");
                        let send_result = socket
                            .send(&egress)
                            .await
                            .with_context(|| format!("syslog_udp send to {}", address));
                        if send_result.is_err() {
                            state.conn = None;
                        }
                        send_result.map(|len| {
                            metrics.bytes_written.inc_by(len as u64);
                        })
                    })
                },
                shutdown,
            )
            .await;

        match result {
            Ok(()) => SyslogUdpWriteOutcome::Delivered,
            Err(PeerSendError::PreSendShutdown) => SyslogUdpWriteOutcome::PreSendShutdown,
            Err(e) => SyslogUdpWriteOutcome::Err(e.into()),
        }
    }

    /// Send one rendered datagram via the peer-rotation helper.
    /// Transport-only — does NOT mutate `OutputMetrics`. The disposition
    /// owner ([`Output::consume`]'s Delivered arm for steady-state, or
    /// [`finalize_shutdown_singleton_disposition`][crate::modules::finalize_shutdown_singleton_disposition]
    /// for shutdown drain) bumps `events_written` on success. Private —
    /// called from [`Output::consume_shutdown`] and unit tests that
    /// drive the transport directly without constructing an `Event`;
    /// the steady-state `Output::consume` uses
    /// [`Self::write_payload_shutdown_aware`] so its pre-send DNS /
    /// bind / connect phase can race the shutdown signal.
    async fn write_payload(&self, payload: SyslogPayload) -> Result<()> {
        let metrics = Arc::clone(&self.metrics);
        let result = self
            .peers
            .write_with_rotation_now(move |_idx, peer, state| {
                let egress = payload.egress.clone();
                let address = peer.address();
                let metrics = Arc::clone(&metrics);
                Box::pin(async move {
                    if state.conn.is_none() {
                        // Resolve the peer address and walk every
                        // result, picking the bind family per address
                        // — hostnames are typically dual-stack and a
                        // partial v6 outage (or a misconfigured AAAA)
                        // would otherwise leave us stuck on the first
                        // record. Pre-0.7.8 `socket.connect(host:port)`
                        // walked the resolution list internally; the
                        // 0.7.8 family-aware rewrite kept the v6-only
                        // destination working but regressed the
                        // failover by committing to `lookup_host().next()`.
                        // Now we explicitly retry each resolved
                        // SocketAddr (binding a fresh ephemeral socket
                        // of the matching family) and break on first
                        // success, mirroring the standard library's
                        // `TcpStream::connect` walking semantics.
                        let resolved: Vec<std::net::SocketAddr> = tokio::time::timeout(
                            PEER_CONNECT_TIMEOUT,
                            tokio::net::lookup_host(address),
                        )
                        .await
                        .with_context(|| format!("syslog_udp lookup {} timed out", address))?
                        .with_context(|| format!("syslog_udp lookup {}", address))?
                        .collect();
                        if resolved.is_empty() {
                            anyhow::bail!(
                                "syslog_udp: name resolution for {} returned no addresses",
                                address
                            );
                        }
                        let mut socket: Option<UdpSocket> = None;
                        let mut last_err: Option<anyhow::Error> = None;
                        for addr in &resolved {
                            let bind_addr = match addr {
                                std::net::SocketAddr::V4(_) => "0.0.0.0:0",
                                std::net::SocketAddr::V6(_) => "[::]:0",
                            };
                            let candidate = match UdpSocket::bind(bind_addr).await {
                                Ok(s) => s,
                                Err(e) => {
                                    last_err = Some(anyhow::Error::from(e).context(format!(
                                        "syslog_udp output: failed to bind ephemeral socket ({})",
                                        bind_addr
                                    )));
                                    continue;
                                }
                            };
                            let connect_res = tokio::time::timeout(
                                PEER_CONNECT_TIMEOUT,
                                candidate.connect(*addr),
                            )
                            .await
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "syslog_udp connect to {} ({}) timed out",
                                    address,
                                    addr
                                )
                            })
                            .and_then(|res| {
                                res.with_context(|| {
                                    format!("syslog_udp connect to {} ({})", address, addr)
                                })
                            });
                            match connect_res {
                                Ok(()) => {
                                    socket = Some(candidate);
                                    break;
                                }
                                Err(e) => last_err = Some(e),
                            }
                        }
                        let socket = socket.ok_or_else(|| {
                            last_err.unwrap_or_else(|| {
                                anyhow::anyhow!(
                                    "syslog_udp: no resolved address for {} could be connected",
                                    address
                                )
                            })
                        })?;
                        state.conn = Some(socket);
                    }

                    // No timeout wrapper on UDP send — see the
                    // rationale on the sibling shutdown-aware path.
                    let socket = state.conn.as_mut().expect("connection should be present");
                    let send_result = socket
                        .send(&egress)
                        .await
                        .with_context(|| format!("syslog_udp send to {}", address));
                    if send_result.is_err() {
                        state.conn = None;
                    }
                    send_result.map(|len| {
                        metrics.bytes_written.inc_by(len as u64);
                    })
                })
            })
            .await;

        result.map_err(|err| anyhow::anyhow!("{}", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::SchemaErrorKind;
    use std::time::Duration;

    /// Wrap a property list in a `ModuleProperties` shaped for this test module.
    /// Mirrors what the parser produces for `def input/output ... { type syslog_udp; ... }`
    /// without going through pest, so tests can drive `Module::{build,from_properties}`
    /// directly.
    fn mp(props: &[Property]) -> crate::dsl::module_props::ModuleProperties {
        crate::dsl::module_props::ModuleProperties::from_parts("syslog_udp", props.to_vec())
    }

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_quoted: false,
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_quoted: false,
            key_span: None,
            properties,
        }
    }

    fn peer(host: &str, port: i64) -> Property {
        block(
            "peer",
            vec![
                kv("host", ExprKind::StringLit(host.into())),
                kv("port", ExprKind::IntLit(port)),
            ],
        )
    }

    #[test]
    fn build_accepts_single_peer() {
        let u = SyslogUdpOutput::build(
            "u",
            &mp(&[peer("h", 1)]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("ok");
        assert_eq!(u.peers.len(), 1);
        assert_eq!(u.peers.peers()[0].address(), "h:1");
    }

    #[test]
    fn build_accepts_peer_with_default_port() {
        let u = SyslogUdpOutput::build(
            "u",
            &mp(&[block(
                "peer",
                vec![kv("host", ExprKind::StringLit("h".into()))],
            )]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("ok");
        assert_eq!(u.peers.peers()[0].address(), "h:514");
    }

    #[test]
    fn build_accepts_multiple_peers() {
        let u = SyslogUdpOutput::build(
            "u",
            &mp(&[block("peers", vec![peer("a", 1), peer("b", 2)])]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("ok");
        assert_eq!(u.peers.len(), 2);
        assert_eq!(u.peers.peers()[0].address(), "a:1");
        assert_eq!(u.peers.peers()[1].address(), "b:2");
    }

    #[test]
    fn build_rejects_missing_destination() {
        let err =
            SyslogUdpOutput::build("u", &mp(&[]), &crate::modules::BuildContext::for_testing())
                .err()
                .expect("missing destination");
        assert!(
            err.to_string()
                .contains("either 'peer' or 'peers' is required"),
            "{}",
            err
        );
    }

    #[test]
    fn build_rejects_unknown_key_with_did_you_mean() {
        let props = vec![block(
            "per",
            vec![kv("host", ExprKind::StringLit("h".into()))],
        )];
        let err = SyslogUdpOutput::build(
            "u",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("typo");
        let msg = err.to_string();
        assert!(msg.contains("per") && msg.contains("peer"), "{}", msg);
    }

    #[test]
    fn build_rejects_peer_and_peers_together() {
        let props = vec![peer("a", 1), block("peers", vec![peer("b", 2)])];
        let err = SyslogUdpOutput::build(
            "u",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("exclusive group"), "{}", msg);
        assert!(msg.contains("peer") && msg.contains("peers"), "{}", msg);
    }

    #[test]
    fn from_properties_rejects_peer_and_peers_together() {
        // `Module::build` validates the schema first and so always
        // catches the exclusive_group violation before reaching
        // `from_properties`. But callers that bypass schema validation
        // (snippet expansion, inline test fixtures) hit
        // `from_properties` directly — without this check it would
        // silently take the first `peer` block and discard the
        // `peers` block. Belt-and-braces.
        let props = vec![peer("a", 1), block("peers", vec![peer("b", 2)])];
        let err = SyslogUdpOutput::from_properties(
            "u",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("from_properties should reject both blocks");
        let msg = err.to_string();
        assert!(msg.contains("mutually exclusive"), "{}", msg);
    }

    #[test]
    fn build_rejects_empty_peers_block() {
        let err = SyslogUdpOutput::from_properties(
            "u",
            &mp(&[block("peers", vec![])]),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("should fail");
        assert!(
            err.to_string()
                .contains("peers block must contain at least one peer"),
            "{}",
            err
        );

        let schema_errs =
            crate::dsl::schema::validate(&[block("peers", vec![])], SYSLOG_UDP_OUTPUT_SCHEMA);
        assert!(
            schema_errs
                .iter()
                .any(|err| matches!(err.kind, SchemaErrorKind::MissingRequired))
        );
    }

    #[test]
    fn build_rejects_peer_missing_host() {
        let props = vec![block("peer", vec![kv("port", ExprKind::IntLit(514))])];
        let err = SyslogUdpOutput::build(
            "u",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("host"), "{}", err);
    }

    #[tokio::test]
    async fn send_to_ipv6_loopback_peer() {
        // Regression guard for the IPv4-forced bind. Listen on the
        // IPv6 loopback, configure the output to send there, and
        // verify the bytes arrive. Pre-fix, the output would bind a
        // v4 socket and `connect()` to a v6 destination, which fails
        // before the first datagram leaves.
        use bytes::Bytes;
        use std::net::SocketAddr;

        let listener = UdpSocket::bind("[::1]:0")
            .await
            .expect("ipv6 loopback unavailable on this host");
        let addr = listener.local_addr().unwrap();
        let port = addr.port() as i64;

        let output = SyslogUdpOutput::build(
            "u6",
            &mp(&[block(
                "peer",
                vec![
                    kv("host", ExprKind::StringLit("::1".into())),
                    kv("port", ExprKind::IntLit(port)),
                ],
            )]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");

        // Hand-craft a SyslogPayload the way the runtime would after
        // its inline render step.
        let payload = SyslogPayload {
            egress: Bytes::from_static(b"<134>hello-v6"),
        };
        output
            .write_payload(payload)
            .await
            .expect("write should succeed");

        // Read one datagram off the listener.
        let mut buf = [0u8; 64];
        let (n, src) = tokio::time::timeout(Duration::from_secs(1), listener.recv_from(&mut buf))
            .await
            .expect("timed out waiting for datagram")
            .expect("recv_from");
        assert_eq!(&buf[..n], b"<134>hello-v6");
        assert!(
            matches!(src, SocketAddr::V6(_)),
            "peer source must be v6, got {:?}",
            src
        );

        // The transport helper is metric-free by contract — the
        // steady-state `consume` and the shutdown-drain helper are the
        // sole owners of the `events_written` bump. Pin that so a
        // future refactor that re-adds a bump to `write_payload` is
        // caught here.
        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            0,
            "write_payload must not mutate events_written; disposition owner bumps"
        );
    }

    /// Steady-state success (delivered via `consume`) bumps
    /// `events_written` exactly once. Regression against the
    /// under-count where `write_payload_shutdown_aware` returning
    /// `Delivered` left the counter untouched, so normal traffic
    /// reported `events_written == 0` while shutdown-drain success
    /// counted.
    #[tokio::test]
    async fn steady_state_consume_success_bumps_events_written_once() {
        use crate::event::Event;
        use crate::queue::QueueAckHandle;
        use bytes::Bytes;

        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let port = addr.port() as i64;

        let output = SyslogUdpOutput::build(
            "u",
            &mp(&[block(
                "peer",
                vec![
                    kv("host", ExprKind::StringLit("127.0.0.1".into())),
                    kv("port", ExprKind::IntLit(port)),
                ],
            )]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");

        let event = Event::new(
            Bytes::from_static(b"<134>hello"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.expect("consume");

        // Drain the datagram off the listener so the test doesn't
        // race the socket's send-buffer flush.
        let mut buf = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(1), listener.recv_from(&mut buf))
            .await
            .expect("timed out waiting for datagram")
            .expect("recv_from");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "steady-state consume success must bump events_written exactly once"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful send must not bump events_failed"
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            event.egress.len() as u64,
            "UDP counts the length confirmed by send"
        );
    }

    /// Shutdown-drain success bumps `events_written` exactly once,
    /// via `finalize_shutdown_singleton_disposition`. Regression to
    /// pin that the fold to the helper preserves the one-bump-per-
    /// successful-event contract on the shutdown path.
    #[tokio::test]
    async fn shutdown_consume_success_bumps_events_written_once() {
        use crate::event::Event;
        use crate::queue::QueueAckHandle;
        use bytes::Bytes;

        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let port = addr.port() as i64;

        let output = SyslogUdpOutput::build(
            "u",
            &mp(&[block(
                "peer",
                vec![
                    kv("host", ExprKind::StringLit("127.0.0.1".into())),
                    kv("port", ExprKind::IntLit(port)),
                ],
            )]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");

        let event = Event::new(
            Bytes::from_static(b"<134>shutdown"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        output
            .consume_shutdown(&event, ack)
            .await
            .expect("consume_shutdown");

        let mut buf = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(1), listener.recv_from(&mut buf))
            .await
            .expect("timed out waiting for datagram")
            .expect("recv_from");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "consume_shutdown success must bump events_written exactly once via helper"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful drain must not bump events_failed"
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            event.egress.len() as u64,
            "shutdown and steady-state use the same confirmed-byte contract"
        );
    }

    #[tokio::test]
    async fn oversized_datagram_send_error_counts_no_confirmed_bytes() {
        use crate::event::Event;
        use crate::queue::QueueAckHandle;
        use bytes::Bytes;

        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port() as i64;
        let output = SyslogUdpOutput::build(
            "udp-failure",
            &mp(&[
                block(
                    "peer",
                    vec![
                        kv("host", ExprKind::StringLit("127.0.0.1".into())),
                        kv("port", ExprKind::IntLit(port)),
                    ],
                ),
                block(
                    "retry",
                    vec![
                        kv("max_attempts", ExprKind::IntLit(1)),
                        kv("initial_wait", ExprKind::StringLit("1ms".into())),
                        kv("max_wait", ExprKind::StringLit("1ms".into())),
                    ],
                ),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");
        // Exceeds the maximum UDP payload on every supported platform, so
        // send must fail locally without depending on ICMP timing.
        let event = Event::new(Bytes::from(vec![0; 65_536]), "127.0.0.1:0".parse().unwrap());
        let (ack, _ack_rx) = QueueAckHandle::for_test();

        tokio::time::timeout(Duration::from_secs(2), output.consume(&event, ack))
            .await
            .expect("oversized datagram failure must remain bounded")
            .unwrap();

        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.events_written.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.in_retry.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn oversized_datagram_backoff_sets_retry_gauge_until_shutdown() {
        use crate::event::Event;
        use crate::queue::QueueAckHandle;
        use bytes::Bytes;

        let listener = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port() as i64;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.shutdown_signal = shutdown_rx;
        let output = Arc::new(
            SyslogUdpOutput::build(
                "udp-retry",
                &mp(&[
                    block(
                        "peer",
                        vec![
                            kv("host", ExprKind::StringLit("127.0.0.1".into())),
                            kv("port", ExprKind::IntLit(port)),
                        ],
                    ),
                    block(
                        "retry",
                        vec![
                            kv("max_attempts", ExprKind::IntLit(3)),
                            kv("initial_wait", ExprKind::StringLit("5s".into())),
                            kv("max_wait", ExprKind::StringLit("5s".into())),
                        ],
                    ),
                ]),
                &ctx,
            )
            .expect("build"),
        );
        let event = Event::new(Bytes::from(vec![0; 65_536]), "127.0.0.1:0".parse().unwrap());
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(async move { task_output.consume(&event, ack).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while output.metrics.retries.load(Ordering::Relaxed) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("oversized datagram must enter backoff");
        assert_eq!(output.metrics.in_retry.load(Ordering::Relaxed), 1);
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown must stop UDP retry")
            .unwrap()
            .unwrap();
        assert_eq!(output.metrics.in_retry.load(Ordering::Relaxed), 0);
    }
}

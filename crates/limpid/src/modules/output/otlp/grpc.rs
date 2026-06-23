//! OTLP/gRPC output: forwards Events to one or more OpenTelemetry
//! collectors / SaaS backends via OTLP over gRPC.
//!
//! ```text
//! def output otlp_out {
//!     type otlp_grpc
//!     peers {
//!         peer {
//!             endpoint "https://collector-a.example.com:4317"
//!             tls { ca "/etc/limpid/ca.crt" }
//!         }
//!         peer {
//!             endpoint "https://collector-b.example.com:4317"
//!             tls {
//!                 ca   "/etc/limpid/ca.crt"
//!                 cert "/etc/limpid/client.crt"
//!                 key  "/etc/limpid/client.key"
//!             }
//!         }
//!     }
//!     batch_size 512
//!     batch_timeout "5s"
//!     headers {
//!         Authorization "Bearer ${env.OTLP_TOKEN}"
//!     }
//! }
//! ```
//!
//! ### Endpoint conventions
//!
//! Each `peer.endpoint` is the gRPC server URL (typically `:4317`).
//! The service name
//! (`opentelemetry.proto.collector.logs.v1.LogsService`) is implicit
//! in the generated client. `https://` and `http://` schemes select
//! TLS / plaintext respectively. Headers translate to gRPC metadata.
//!
//! ### Round-robin + cooldown
//!
//! On each flush, peers are tried in round-robin order. A peer that
//! fails the request is marked cooled-down for `PEER_COOLDOWN` (5s,
//! shared with the syslog outputs) and skipped on subsequent flushes
//! until the cooldown expires. The `retry { … }` block controls the
//! per-flush retry budget; within one budget the rotation transparently
//! picks the next available peer. If every peer is currently cooled
//! the rotation falls back to the cursor start — the retry budget,
//! not the cooldown, protects the single-peer-just-failed case.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, logs_service_client::LogsServiceClient,
};
use tokio::sync::Mutex;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::{BorrowedEvent, Event};
use crate::metrics::OutputMetrics;
use crate::modules::output::syslog_peers::{PEER_COOLDOWN, iter_peers_block};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::queue::{BackoffStrategy, RetryConfig};

use super::{BatchLevel, OTLP_RETRY_BLOCK_PROPERTIES, OtlpPayload, decode_drained_to_request};

/// Upper bound on a single gRPC export. A stalled collector (TCP
/// connection accepted but no HEADERS frame returned) would otherwise
/// hold the flush future open indefinitely and starve the
/// rotation/retry path. 30s is loose enough for normal collector
/// latency including TLS handshake; flushes that take longer are
/// almost certainly hung.
const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct GrpcPeer {
    endpoint: String,
    channel: Channel,
}

struct PeerState {
    cooldown_until: Mutex<Option<Instant>>,
}

impl Default for PeerState {
    fn default() -> Self {
        Self {
            cooldown_until: Mutex::new(None),
        }
    }
}

struct Inner {
    peers: Vec<GrpcPeer>,
    peer_state: Vec<PeerState>,
    cursor: AtomicUsize,
    batch_level: BatchLevel,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    /// Per-batch retry policy — see [`super::http`] for the
    /// rationale.
    retry_config: RetryConfig,
    /// Buffered per-Event singleton ResourceLogs proto bytes.
    batch: Mutex<Vec<Bytes>>,
}

pub struct OtlpGrpcOutput {
    /// Operator-facing instance name; surfaced on PR-P shutdown-recovery
    /// records as `(output <name> shutdown)`.
    name: String,
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

const GRPC_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "endpoint",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(crate::tls::TLS_CLIENT_BLOCK_PROPERTIES),
    },
];

const GRPC_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(GRPC_PEER_SCHEMA),
}];

const OTLP_GRPC_OUTPUT_SCHEMA: &[PropertySpec] = &[
    // Shorthand for the common single-collector case; mirrors the
    // syslog_tcp ergonomics. One of `peer` (single) or `peers` (multi)
    // is required; both at once is rejected by the schema layer.
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(GRPC_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(GRPC_PEERS_SCHEMA),
    },
    PropertySpec {
        name: "batch_size",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "batch_timeout",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "batch_level",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["none", "resource", "scope"]),
    },
    PropertySpec {
        name: "headers",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::StringMap,
    },
    PropertySpec {
        name: "retry",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(OTLP_RETRY_BLOCK_PROPERTIES),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

fn parse_peer(name: &str, peer_props: &[Property]) -> Result<GrpcPeer> {
    let endpoint = props::get_string(peer_props, "endpoint")
        .ok_or_else(|| anyhow!("output '{}': otlp_grpc peer requires 'endpoint'", name))?;

    let tls_block = props::get_block(peer_props, "tls");
    if tls_block.is_some() && !endpoint.to_ascii_lowercase().starts_with("https://") {
        // A `tls { ... }` block on a plaintext (http:// or
        // scheme-less) endpoint is almost always an operator
        // error: tonic only engages the TLS layer when the URI
        // scheme is https, so the configured CA / client identity
        // is silently dropped and the daemon ships gRPC in clear
        // text. Refuse at parse time so the misconfiguration is
        // visible. Mirrors the matching guard in `output otlp_http`.
        bail!(
            "output '{}': otlp_grpc peer endpoint '{}' uses a plaintext scheme but a tls {{ ... }} block was supplied — switch the endpoint to https:// or drop the tls block",
            name,
            endpoint
        );
    }
    let tls_cfg = tls_block
        .map(|block| {
            let cfg = crate::tls::ClientTlsConfig {
                ca_path: props::get_string(block, "ca"),
                cert_path: props::get_string(block, "cert"),
                key_path: props::get_string(block, "key"),
            };
            cfg.validate(&format!("output '{}'", name))?;
            Ok::<_, anyhow::Error>(cfg)
        })
        .transpose()?;

    let mut endpoint_builder = Endpoint::from_shared(endpoint.clone())
        .with_context(|| format!("output '{}': invalid gRPC endpoint '{}'", name, endpoint))?;

    let needs_tls = endpoint.starts_with("https://") || tls_cfg.is_some();
    if needs_tls {
        crate::tls::install_default_crypto_provider();
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(cfg) = &tls_cfg {
            if let Some(ca_path) = &cfg.ca_path {
                let pem = std::fs::read(ca_path).with_context(|| {
                    format!("output '{}': cannot read CA cert {}", name, ca_path)
                })?;
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
            }
            if let (Some(cert_path), Some(key_path)) = (&cfg.cert_path, &cfg.key_path) {
                let cert_pem = std::fs::read(cert_path).with_context(|| {
                    format!("output '{}': cannot read client cert {}", name, cert_path)
                })?;
                let key_pem = std::fs::read(key_path).with_context(|| {
                    format!("output '{}': cannot read client key {}", name, key_path)
                })?;
                tls = tls.identity(tonic::transport::Identity::from_pem(cert_pem, key_pem));
            }
        }
        endpoint_builder = endpoint_builder
            .tls_config(tls)
            .with_context(|| format!("output '{}': failed to configure gRPC TLS", name))?;
    }
    let channel = endpoint_builder.connect_lazy();
    Ok(GrpcPeer { endpoint, channel })
}

impl Module for OtlpGrpcOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(OTLP_GRPC_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();

        let batch_size = props::get_positive_int(properties, "batch_size")?.unwrap_or(1) as usize;
        let batch_timeout = match props::get_string(properties, "batch_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };
        let batch_level_str = props::get_string(properties, "batch_level")
            .or_else(|| props::get_ident(properties, "batch_level"))
            .unwrap_or_else(|| "none".to_string());
        let batch_level = BatchLevel::parse(&batch_level_str, name)?;

        let headers = props::get_string_map(properties, "headers");

        // Single-peer shorthand (`peer { endpoint ... }`) or multi-peer
        // (`peers { peer { ... } ... }`). The schema's exclusive_group
        // already forbids both at once, so we just probe in priority
        // order.
        let peers = if let Some(peer_block) = props::get_block(properties, "peer") {
            vec![parse_peer(name, peer_block)?]
        } else if let Some(peers_block) = props::get_block(properties, "peers") {
            iter_peers_block(
                peers_block,
                &format!("output '{}': peers", name),
                |peer_props| parse_peer(name, peer_props),
            )?
        } else {
            anyhow::bail!(
                "output '{}': otlp_grpc requires a 'peer {{ ... }}' or 'peers {{ peer {{ ... }} ... }}' block",
                name
            );
        };

        let peer_state = peers.iter().map(|_| PeerState::default()).collect();

        let retry_config = RetryConfig::from_output_properties(properties)?;

        Ok(Self {
            name: name.to_string(),
            inner: Arc::new(Inner {
                peers,
                peer_state,
                cursor: AtomicUsize::new(0),
                batch_level,
                headers,
                batch_timeout,
                retry_config,
                batch: Mutex::new(Vec::new()),
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpGrpcOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for OtlpGrpcOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(OtlpPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: OtlpPayload = payload.downcast()?;
        let proto = payload.egress;
        let mut batch = self.inner.batch.lock().await;
        batch.push(proto);
        let should_flush = batch.len() >= self.batch_size;
        drop(batch);

        if should_flush {
            self.flush().await?;
        } else {
            self.ensure_flush_timer().await;
        }
        Ok(())
    }

    /// Owned-event path (disk-queue replay, control-socket inject). The
    /// queue consumer needs a per-event ship verdict — Ok ⇒ drop from
    /// the queue, Err ⇒ retry / disk-replay / secondary — so routing
    /// Owned events through the batched buffer would silently merge them
    /// into a later flush and the caller would lose the verdict. Ship
    /// inline here, bypassing the batch.
    async fn write_owned(&self, event: &Event) -> Result<()> {
        crate::modules::ship_owned_inline(self, event, &self.metrics, |payload| async move {
            let payload: OtlpPayload = payload.downcast()?;
            send_batch(&self.inner, vec![payload.egress])
                .await
                .map(|o| o.rejected)
        })
        .await
    }

    async fn shutdown(
        &self,
        error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        // Same contract as `otlp_http::shutdown`: abort the timer to
        // avoid a race for the buffer lock, then drain in one final
        // send. Without this the queue consumer's shutdown call
        // races the timer task, and either Drop's abort or the
        // process exit can leave the in-flight buffer behind.
        if let Some(h) = self.flush_handle.lock().await.take() {
            h.abort();
        }
        match self.flush().await {
            Ok(()) => Ok(()),
            Err(e) => {
                // BC-4 / PR-P: `flush()` restored the drained batch
                // into `self.inner.batch` on transport error. Drain
                // it into `error_log` when the operator opted in;
                // otherwise return Err and preserve 0.7.7 behaviour
                // (queue consumer logs a warn and the buffer is
                // lost).
                if let Some(writer) = error_log {
                    let payloads: Vec<bytes::Bytes> =
                        std::mem::take(&mut *self.inner.batch.lock().await);
                    crate::modules::write_shutdown_buffer_to_error_log(
                        writer, &self.name, payloads, &e,
                    )
                    .await;
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl OtlpGrpcOutput {
    async fn flush(&self) -> Result<()> {
        let drained: Vec<Bytes> = {
            let mut batch = self.inner.batch.lock().await;
            std::mem::take(&mut *batch)
        };
        if drained.is_empty() {
            return Ok(());
        }
        let count = drained.len() as u64;
        // Clone for the send so the original survives for retry
        // restoration on transport error. `Bytes::clone` is a
        // refcount bump, no payload copy.
        let result = send_batch(&self.inner, drained.clone()).await;
        match result {
            Ok(outcome) => {
                let rejected = outcome.rejected.min(count);
                let written = count - rejected;
                if written > 0 {
                    self.metrics
                        .events_written
                        .fetch_add(written, Ordering::Relaxed);
                }
                if rejected > 0 {
                    self.metrics
                        .events_failed
                        .fetch_add(rejected, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(e) => {
                // Transport error: put the drained batch back into
                // the buffer for retry instead of dropping it.
                // Mirrors `output http` (PR limpid#33) and the
                // sibling `otlp_http` write-flush path. Do NOT bump
                // events_failed — the events are retained, not
                // permanently rejected.
                let mut batch = self.inner.batch.lock().await;
                let new_events = std::mem::take(&mut *batch);
                *batch = drained;
                batch.extend(new_events);
                Err(e)
            }
        }
    }

    async fn ensure_flush_timer(&self) {
        let mut handle = self.flush_handle.lock().await;
        if let Some(h) = handle.as_ref()
            && !h.is_finished()
        {
            return;
        }

        let inner = Arc::clone(&self.inner);
        let metrics = Arc::clone(&self.metrics);
        let new_handle = tokio::spawn(async move {
            tokio::time::sleep(inner.batch_timeout).await;
            let drained: Vec<Bytes> = {
                let mut batch = inner.batch.lock().await;
                std::mem::take(&mut *batch)
            };
            if drained.is_empty() {
                return;
            }
            let count = drained.len() as u64;
            match send_batch(&inner, drained.clone()).await {
                Ok(outcome) => {
                    let rejected = outcome.rejected.min(count);
                    let written = count - rejected;
                    if written > 0 {
                        metrics.events_written.fetch_add(written, Ordering::Relaxed);
                    }
                    if rejected > 0 {
                        metrics.events_failed.fetch_add(rejected, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    // Same retention contract as the write-triggered
                    // flush: return the drained batch to the buffer
                    // for the next write or timer firing to retry.
                    tracing::warn!(
                        "otlp_grpc flush timer: send failed ({}) — {} events returned to buffer",
                        e,
                        count
                    );
                    let mut buf = inner.batch.lock().await;
                    let new_events = std::mem::take(&mut *buf);
                    *buf = drained;
                    buf.extend(new_events);
                }
            }
        });
        *handle = Some(new_handle);
    }
}

impl Drop for OtlpGrpcOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
        if let Ok(buf) = self.inner.batch.try_lock()
            && !buf.is_empty()
        {
            tracing::warn!(
                "otlp_grpc output: {} events in buffer at shutdown (will be re-delivered from queue)",
                buf.len()
            );
        }
    }
}

async fn send_batch(inner: &Inner, drained: Vec<Bytes>) -> Result<super::SendOutcome> {
    let req = decode_drained_to_request(drained, inner.batch_level)?;
    let n = inner.peers.len();

    let cfg = &inner.retry_config;
    let max_attempts = cfg.max_attempts.max(1);
    let mut attempt = 0u32;
    let mut wait = cfg.initial_wait;

    let final_err = loop {
        let start = inner.cursor.fetch_add(1, Ordering::Relaxed) % n;
        let now = Instant::now();
        let mut idx = start;
        for offset in 0..n {
            let candidate = (start + offset) % n;
            let guard = inner.peer_state[candidate].cooldown_until.lock().await;
            if guard.is_none_or(|until| until <= now) {
                idx = candidate;
                break;
            }
        }

        let err = match send_once(&inner.peers[idx], inner, &req).await {
            Ok(outcome) => {
                *inner.peer_state[idx].cooldown_until.lock().await = None;
                return Ok(outcome);
            }
            Err(e) => {
                // Measure cooldown from failure time, not request start:
                // `now` was captured before `send_once`, so for any non-
                // trivial request latency (and especially after a 30s
                // GRPC_REQUEST_TIMEOUT firing) `now + PEER_COOLDOWN`
                // can already be in the past, defeating the rotation.
                *inner.peer_state[idx].cooldown_until.lock().await =
                    Some(Instant::now() + PEER_COOLDOWN);
                e
            }
        };
        if attempt + 1 >= max_attempts {
            break err;
        }
        attempt += 1;
        tracing::warn!(
            "otlp_grpc output: ship attempt {}/{} failed: {} — retrying in {:?}",
            attempt,
            max_attempts,
            err,
            wait,
        );
        tokio::time::sleep(wait).await;
        if matches!(cfg.backoff, BackoffStrategy::Exponential) {
            wait = wait.saturating_mul(2).min(cfg.max_wait);
        }
    };
    Err(final_err)
}

async fn send_once(
    peer: &GrpcPeer,
    inner: &Inner,
    req: &ExportLogsServiceRequest,
) -> Result<super::SendOutcome> {
    let mut client = LogsServiceClient::new(peer.channel.clone());
    let mut request = tonic::Request::new(req.clone());
    let metadata = request.metadata_mut();
    for (k, v) in &inner.headers {
        // Lower-case the metadata key per HTTP/2 / gRPC convention;
        // tonic enforces this and will refuse `Authorization` etc.
        let key_lc = k.to_ascii_lowercase();
        match (
            tonic::metadata::MetadataKey::<tonic::metadata::Ascii>::from_bytes(key_lc.as_bytes()),
            tonic::metadata::MetadataValue::try_from(v.as_str()),
        ) {
            (Ok(mk), Ok(mv)) => {
                metadata.insert(mk, mv);
            }
            _ => {
                // Never log the value: typical contents are bearer
                // tokens, API keys, or cookie material — the exact
                // secret an OTLP backend authenticates with. The key
                // alone is enough to diagnose a misconfiguration
                // without leaking the credential.
                tracing::warn!(
                    "otlp_grpc: skipping header with invalid name or value (key={:?}); value redacted",
                    k
                );
            }
        }
    }
    let response = tokio::time::timeout(GRPC_REQUEST_TIMEOUT, client.export(request))
        .await
        .map_err(|_| {
            anyhow!(
                "output otlp_grpc: export to {} timed out after {:?}",
                peer.endpoint,
                GRPC_REQUEST_TIMEOUT
            )
        })?
        .with_context(|| format!("output otlp_grpc: export to {} failed", peer.endpoint))?;
    // The receiver may report `partial_success.rejected_log_records`.
    // Logged here as a warning AND propagated to the caller via
    // `SendOutcome.rejected` so the flush path can split the batch's
    // events between `events_written` (accepted) and `events_failed`
    // (rejected by the server). Selective re-send of *only* the
    // rejected records is queued for a later release; the retry loop
    // in `send_batch` handles hard failures (connection refused, 5xx,
    // …) but not partial-success deltas, since the rejected set is a
    // strict subset of what already shipped.
    let inner_resp = response.into_inner();
    let rejected = inner_resp
        .partial_success
        .as_ref()
        .map(|p| p.rejected_log_records.max(0) as u64)
        .unwrap_or(0);
    if let Some(partial) = inner_resp.partial_success.as_ref()
        && partial.rejected_log_records > 0
    {
        tracing::warn!(
            "otlp_grpc: {} rejected {} log record(s){}",
            peer.endpoint,
            partial.rejected_log_records,
            if partial.error_message.is_empty() {
                String::new()
            } else {
                format!(" — {}", partial.error_message)
            }
        );
    }
    Ok(super::SendOutcome { rejected })
}

// Note: `verify false` is intentionally not a property on `otlp_grpc`.
// tonic does not expose an insecure-skip-verify knob the way reqwest
// does; users that need plaintext for development environments simply
// use an `http://` endpoint.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::event::Event;
    use opentelemetry_proto::tonic::collector::logs::v1::{
        ExportLogsServiceResponse,
        logs_service_server::{LogsService, LogsServiceServer},
    };
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use prost::Message;
    use std::net::SocketAddr;

    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("otlp_grpc", props.to_vec())
    }

    fn prop_str(key: &str, val: &str) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::StringLit(val.to_string())),
            value_span: None,
        }
    }

    fn prop_int(key: &str, val: i64) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::IntLit(val)),
            value_span: None,
        }
    }

    fn peer_block(endpoint: &str) -> Property {
        Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("endpoint", endpoint)],
        }
    }

    fn peers_block_with(peers: Vec<Property>) -> Property {
        Property::Block {
            key: "peers".into(),
            key_span: None,
            properties: peers,
        }
    }

    fn one_peer_props(endpoint: &str) -> Vec<Property> {
        vec![peers_block_with(vec![peer_block(endpoint)])]
    }

    #[test]
    fn requires_peer_or_peers_block() {
        let err = OtlpGrpcOutput::from_properties("o", &mp(&[]))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("'peer {") && err.to_string().contains("'peers {"),
            "unexpected: {err}"
        );
    }

    #[tokio::test]
    async fn accepts_single_peer_shorthand() {
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("endpoint", "http://x:4317")],
        }];
        let output = OtlpGrpcOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 1);
        assert_eq!(output.inner.peers[0].endpoint, "http://x:4317");
    }

    #[test]
    fn rejects_peers_block_with_no_peer() {
        let props = vec![peers_block_with(vec![])];
        let err = OtlpGrpcOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("at least one peer"));
    }

    #[tokio::test]
    async fn accepts_plain_http_endpoint() {
        let output =
            OtlpGrpcOutput::from_properties("o", &mp(&one_peer_props("http://localhost:4317")))
                .unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    #[test]
    fn rejects_tls_block_on_plaintext_endpoint() {
        // `tls { ... }` on an `http://` endpoint is almost always an
        // operator error — tonic only engages TLS when the URI scheme
        // is https, so the configured CA / client identity would be
        // silently dropped and the daemon would ship gRPC in clear
        // text. Fail fast at parse time. Mirrors the matching guard
        // in `output otlp_http`.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "http://collector.example.com:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("ca", "/etc/ca.pem")],
                },
            ],
        }])];
        let err = OtlpGrpcOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("plaintext") && msg.contains("https://"),
            "unexpected: {msg}"
        );
    }

    #[tokio::test]
    async fn accepts_empty_tls_block_on_https_endpoint() {
        // Regression guard for the plaintext-rejection check: https://
        // endpoints must still accept a (here empty) tls block. Empty
        // block keeps the test off-disk; the scheme check runs first.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://collector.example.com:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![],
                },
            ],
        }])];
        let output = OtlpGrpcOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    #[tokio::test]
    async fn accepts_https_endpoint_with_native_tls() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&one_peer_props("https://collector.example.com:4317")),
        )
        .unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    #[tokio::test]
    async fn parses_multi_peer_block() {
        let props = vec![peers_block_with(vec![
            peer_block("http://a:4317"),
            peer_block("http://b:4317"),
        ])];
        let output = OtlpGrpcOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 2);
        assert_eq!(output.inner.peers[0].endpoint, "http://a:4317");
    }

    #[test]
    fn rejects_tls_with_key_but_no_cert() {
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://x:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("key", "/k.pem")],
                },
            ],
        }])];
        let err = OtlpGrpcOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("cert and key"));
    }

    #[tokio::test]
    async fn batch_level_default_is_none() {
        let output =
            OtlpGrpcOutput::from_properties("o", &mp(&one_peer_props("http://x"))).unwrap();
        assert!(matches!(output.inner.batch_level, BatchLevel::None));
    }

    #[tokio::test]
    async fn batch_size_defaults_to_one() {
        let output =
            OtlpGrpcOutput::from_properties("o", &mp(&one_peer_props("http://x"))).unwrap();
        assert_eq!(output.batch_size, 1);
    }

    #[tokio::test]
    async fn retry_block_overrides_defaults() {
        let mut props = one_peer_props("http://x");
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 2),
                prop_str("initial_wait", "100ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.retry_config.max_attempts, 2);
    }

    // ---- wire-level round-trip ----

    fn singleton_bytes(time_unix_nano: u64) -> Bytes {
        let rl = ResourceLogs {
            resource: Some(Resource {
                attributes: vec![],
                dropped_attributes_count: 0,
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "limpid-test".into(),
                    version: "0.5.0".into(),
                    attributes: vec![],
                    dropped_attributes_count: 0,
                }),
                log_records: vec![LogRecord {
                    time_unix_nano,
                    severity_number: 9,
                    severity_text: "INFO".into(),
                    ..Default::default()
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        };
        let mut buf = Vec::with_capacity(rl.encoded_len());
        rl.encode(&mut buf).unwrap();
        Bytes::from(buf)
    }

    fn event_with_egress(egress: Bytes) -> Event {
        let mut e = Event::new(egress.clone(), "127.0.0.1:0".parse::<SocketAddr>().unwrap());
        e.egress = egress;
        e
    }

    async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        for _ in 0..50 {
            if let Some(v) = probe() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for receiver to record the request");
    }

    struct RecordingLogs {
        received: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
    }

    #[tonic::async_trait]
    impl LogsService for RecordingLogs {
        async fn export(
            &self,
            request: tonic::Request<ExportLogsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status>
        {
            self.received.lock().await.push(request.into_inner());
            Ok(tonic::Response::new(ExportLogsServiceResponse {
                partial_success: None,
            }))
        }
    }

    #[tokio::test]
    async fn round_trip_grpc_to_recording_collector() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 1));
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(
                1_700_000_000_000_000_000,
            )))
            .await
            .unwrap();

        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();

        assert_eq!(got.len(), 1);
        let lr = &got[0].resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(lr.time_unix_nano, 1_700_000_000_000_000_000);
    }

    /// Test-only LogsService that ALWAYS reports `partial_success.
    /// rejected_log_records = rejected_count`. Used to pin the
    /// behaviour of `events_written` / `events_failed` when the
    /// receiver advertises a partial-success rejection.
    struct PartialSuccessLogs {
        rejected_per_call: i64,
    }

    #[tonic::async_trait]
    impl LogsService for PartialSuccessLogs {
        async fn export(
            &self,
            _request: tonic::Request<ExportLogsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(ExportLogsServiceResponse {
                partial_success: Some(
                    opentelemetry_proto::tonic::collector::logs::v1::ExportLogsPartialSuccess {
                        rejected_log_records: self.rejected_per_call,
                        error_message: "test rejection".into(),
                    },
                ),
            }))
        }
    }

    #[tokio::test]
    async fn partial_success_rejected_log_records_routes_to_events_failed() {
        // Regression guard: when the receiver returns 2xx-equivalent
        // (gRPC OK) with `partial_success.rejected_log_records = N`,
        // events_written must NOT cover the N rejected records.
        // Previously the entire batch was counted as written and
        // operators saw zero events_failed for server-side rejections,
        // making partial-success outages invisible on dashboards.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let svc = PartialSuccessLogs {
            rejected_per_call: 2,
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        // batch_size=3 so the single Rendered write triggers flush at
        // exactly 3 events; receiver rejects 2 of them.
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 3));
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();

        let mut payloads: Vec<RenderedPayload> = Vec::new();
        for i in 0..3 {
            let ev = event_with_egress(singleton_bytes(1_000_000_000 + i));
            let payload = {
                let bump = bumpalo::Bump::new();
                let arena = EventArena::new(&bump);
                let bev = ev.view_in(&arena);
                output.render(&bev, &arena).unwrap()
            };
            payloads.push(payload);
        }
        for p in payloads {
            output.write(p).await.unwrap();
        }
        // Give the inline flush a moment.
        for _ in 0..50 {
            if output.metrics.events_written.load(Ordering::Relaxed)
                + output.metrics.events_failed.load(Ordering::Relaxed)
                >= 3
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();

        // 3 events total: 2 rejected → events_failed, 1 accepted → events_written.
        let written = output.metrics.events_written.load(Ordering::Relaxed);
        let failed = output.metrics.events_failed.load(Ordering::Relaxed);
        assert_eq!(
            written, 1,
            "expected 1 written, got {written} (failed={failed})"
        );
        assert_eq!(
            failed, 2,
            "expected 2 failed, got {failed} (written={written})"
        );
    }

    #[tokio::test]
    async fn drop_aborts_pending_flush_timer() {
        // The flush timer is only armed by the Rendered (memory hot
        // path) buffer write — Owned events bypass the batch entirely
        // (see `write_owned`) so they never arm the timer. Drive the
        // timer via the Rendered path here.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        let ev = event_with_egress(singleton_bytes(1));
        let payload = {
            let bump = bumpalo::Bump::new();
            let arena = EventArena::new(&bump);
            let bev = ev.view_in(&arena);
            output.render(&bev, &arena).unwrap()
        };
        output.write(payload).await.unwrap();
        let handle_before = output.flush_handle.lock().await.is_some();
        assert!(handle_before, "Rendered write must arm the flush timer");
        drop(output);
    }

    #[tokio::test(start_paused = true)]
    async fn export_timeout_fires_against_stalled_peer() {
        // A peer that accepts the TCP connection but never returns a
        // gRPC HEADERS frame must surface as a timeout failure within
        // GRPC_REQUEST_TIMEOUT (30 s). Without the
        // `tokio::time::timeout(GRPC_REQUEST_TIMEOUT, …)` wrapper a
        // single stalled collector would block the rotation forever.
        // The constant-value assertion in another test catches the
        // case where the constant gets renamed, but it would not catch
        // a regression where the wrapper itself was removed or
        // pointed at a different (e.g. much larger) duration. This
        // test exercises the firing path end-to-end.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        // Stalled "server": accept the TCP connection and hold the
        // socket open without ever sending HTTP/2 SETTINGS or a gRPC
        // response. tonic's client preface goes out, then it waits
        // forever for the server preface — exactly the case the
        // GRPC_REQUEST_TIMEOUT exists to bound.
        let stall = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    held.push(sock);
                }
            }
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 1));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        let send = tokio::spawn(async move {
            output
                .write_owned(&event_with_egress(singleton_bytes(1)))
                .await
        });

        // Let the spawned future reach the timeout-wrapped await,
        // then advance virtual time past GRPC_REQUEST_TIMEOUT so the
        // timeout fires. The TCP connect happens on real I/O; a short
        // wall-clock yield keeps the test from racing the connect.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(GRPC_REQUEST_TIMEOUT + Duration::from_secs(1)).await;

        let result = send.await.unwrap();
        stall.abort();

        let err = result.expect_err("stalled peer must surface as Err");
        let msg = err.to_string().to_ascii_lowercase();
        assert!(
            msg.contains("timed out") || msg.contains("timeout"),
            "expected timeout-flavoured error, got: {err}"
        );
    }

    #[tokio::test]
    async fn write_owned_bypasses_batch_buffer() {
        // Owned events ship inline rather than landing in the batch
        // buffer, so the queue consumer's per-event Ok/Err contract is
        // honored (Err → disk-replay / retry / secondary). Use an
        // unreachable endpoint and large batch_size so the only way
        // the assertion can fail is if write_owned routed through the
        // batch instead of bypassing it.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt with no backoff so the test finishes fast.
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with_egress(singleton_bytes(1)))
            .await
            .expect_err("send must fail against unreachable peer");
        assert!(
            err.to_string().contains("otlp_grpc"),
            "expected ship error, got: {err}"
        );
        let batch_len = output.inner.batch.lock().await.len();
        assert_eq!(batch_len, 0, "Owned event must not land in the batch");
        let timer_armed = output.flush_handle.lock().await.is_some();
        assert!(!timer_armed, "Owned event must not arm the flush timer");
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_batch_buffer() {
        // Regression mirror of `output http` / `otlp_http`: when
        // batch_size > 1 the queue-side `write()` returns Ok once
        // the event is in the buffer, so the memory queue considers
        // it delivered. If the daemon shuts down before the batch
        // fills, Drop alone aborts the timer and leaks the buffer.
        // `shutdown()` aborts the timer and runs one final flush.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        // Large batch + long timer: only shutdown drains the buffer.
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();

        let arena_bump = bumpalo::Bump::new();
        let arena = EventArena::new(&arena_bump);
        for ts in [1u64, 2u64] {
            let ev = event_with_egress(singleton_bytes(ts));
            let payload = output.render(&ev.view_in(&arena), &arena).unwrap();
            output.write(payload).await.unwrap();
        }
        assert_eq!(
            output.inner.batch.lock().await.len(),
            2,
            "writes must land in the buffer"
        );

        output.shutdown(None).await.unwrap();

        assert_eq!(
            output.inner.batch.lock().await.len(),
            0,
            "shutdown() must drain the buffer"
        );

        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();

        assert_eq!(got.len(), 1, "shutdown flush must send exactly once");
        let record_count: usize = got[0]
            .resource_logs
            .iter()
            .flat_map(|rl| rl.scope_logs.iter())
            .map(|sl| sl.log_records.len())
            .sum();
        assert_eq!(
            record_count, 2,
            "shutdown send must carry both buffered records"
        );
    }

    #[tokio::test]
    async fn flush_failure_restores_batch_to_buffer() {
        // Regression mirror of `otlp_http`'s same-named test: the
        // batched flush path used to drain the batch and, on
        // transport failure, bump events_failed and drop the events.
        // `output http` retained them for retry; OTLP was the odd
        // one out. The fix restores the drained batch so the next
        // write or timer firing can re-attempt.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 2));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt, no backoff — keeps the test fast.
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();

        let arena_bump = bumpalo::Bump::new();
        let arena = EventArena::new(&arena_bump);
        let e1 = event_with_egress(singleton_bytes(1));
        let p1 = output.render(&e1.view_in(&arena), &arena).unwrap();
        output.write(p1).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 1);

        let e2 = event_with_egress(singleton_bytes(2));
        let p2 = output.render(&e2.view_in(&arena), &arena).unwrap();
        let err = output
            .write(p2)
            .await
            .expect_err("flush against unreachable peer must fail");
        assert!(
            err.to_string().contains("otlp_grpc"),
            "expected ship error, got: {err}"
        );

        let batch_len = output.inner.batch.lock().await.len();
        assert_eq!(
            batch_len, 2,
            "flush failure must put the drained batch back into the buffer",
        );
        let failed = output.metrics.events_failed.load(Ordering::Relaxed);
        assert_eq!(
            failed, 0,
            "events retained for retry must NOT be counted as failed yet (got {failed})",
        );
    }

    // -----------------------------------------------------------------------
    // BC-4 / PR-P: shutdown-time buffer-loss recovery via `error_log`.
    // -----------------------------------------------------------------------

    fn shutdown_recovery_props(endpoint: &str) -> Vec<Property> {
        let mut props = one_peer_props(endpoint);
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt + minimal wait so the shutdown flush against
        // an unreachable peer completes quickly.
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        props
    }

    async fn buffer_two(output: &OtlpGrpcOutput) {
        let arena_bump = bumpalo::Bump::new();
        let arena = EventArena::new(&arena_bump);
        for ts in [1u64, 2u64] {
            let ev = event_with_egress(singleton_bytes(ts));
            let p = output.render(&ev.view_in(&arena), &arena).unwrap();
            output.write(p).await.unwrap();
        }
        assert_eq!(output.inner.batch.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shutdown_failure_with_error_log_persists_buffer() {
        // Unreachable peer → flush() fails inside shutdown and PR-F
        // restores the batch into the buffer. The new recovery path
        // drains it into the operator-configured `error_log` and the
        // override returns Ok so the consumer treats the daemon as
        // cleanly stopped.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let output = OtlpGrpcOutput::from_properties("myout", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));

        output.shutdown(Some(&writer)).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 0);

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["process"], "(output myout shutdown)");
            assert!(v["reason"].as_str().unwrap().contains("shutdown flush"));
        }
    }

    #[tokio::test]
    async fn shutdown_failure_without_error_log_matches_077() {
        // 0.7.7 parity: no DLQ configured → surface the flush error to
        // the queue consumer (which warns and exits). Buffer remains
        // for inspection on the way out.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let err = output.shutdown(None).await.expect_err("flush must Err");
        assert!(err.to_string().contains("otlp_grpc"), "got: {err}");
        assert_eq!(output.inner.batch.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shutdown_success_does_not_touch_error_log() {
        // Healthy server: shutdown flush succeeds, the operator's
        // audit trail must stay empty even with `error_log` set.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));

        output.shutdown(Some(&writer)).await.unwrap();
        server.abort();
        assert!(!path.exists(), "DLQ must stay untouched on clean shutdown");
    }

    #[tokio::test]
    async fn shutdown_recovery_writer_failure_does_not_recurse() {
        // error_log writer itself fails on every record (parent dir
        // missing). Helper must warn + continue, never loop or panic.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let output = OtlpGrpcOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(
            std::path::PathBuf::from("/nonexistent/limpid-grpc-test/errored.jsonl"),
        ));
        output.shutdown(Some(&writer)).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 0);
    }
}

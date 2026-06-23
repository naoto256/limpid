//! OTLP/HTTP output: forwards Events to one or more OpenTelemetry
//! collectors / SaaS backends via OTLP over HTTP, in either
//! `http_protobuf` (default) or `http_json` wire format.
//!
//! ```text
//! def output otlp_out {
//!     type otlp_http
//!     peers {
//!         peer {
//!             endpoint "https://collector-a.example.com:4318/v1/logs"
//!             tls { ca "/etc/limpid/ca.crt" }
//!         }
//!         peer {
//!             endpoint "https://collector-b.example.com:4318/v1/logs"
//!             tls {
//!                 ca   "/etc/limpid/ca.crt"
//!                 cert "/etc/limpid/client.crt"
//!                 key  "/etc/limpid/client.key"
//!             }
//!         }
//!     }
//!     protocol "http_protobuf"   // http_protobuf | http_json
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
//! Each `peer.endpoint` is the full OTLP/HTTP path (typically
//! `:4318/v1/logs`). limpid does not append `/v1/logs` automatically;
//! collectors that mount the receiver elsewhere (e.g. behind a path
//! prefix) just work.
//!
//! ### Round-robin + cooldown
//!
//! On each flush, peers are tried in round-robin order. A peer that
//! fails the request is marked cooled-down for `PEER_COOLDOWN` (5s,
//! shared with the syslog outputs) and skipped on subsequent flushes
//! until the cooldown expires. The `retry { … }` block controls the
//! per-flush retry budget; within one budget the rotation transparently
//! picks the next available peer.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use prost::Message;
use tokio::sync::Mutex;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::{BorrowedEvent, Event};
use crate::metrics::OutputMetrics;
use crate::modules::output::http_util::{ERROR_BODY_BYTE_CAP, error_snippet};
use crate::modules::output::syslog_peers::{PEER_COOLDOWN, iter_peers_block};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::queue::{BackoffStrategy, RetryConfig};
use crate::tls::ClientTlsConfig;

use super::{BatchLevel, OTLP_RETRY_BLOCK_PROPERTIES, OtlpPayload, decode_drained_to_request};

/// Upper bound on a single HTTP export — connect, TLS handshake,
/// request body send, response headers, response body. A peer that
/// accepts the connection but never replies would otherwise hold the
/// flush future open indefinitely and starve the rotation/retry path.
/// Matches the gRPC side.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpProtocol {
    Json,
    Protobuf,
}

impl HttpProtocol {
    fn parse(s: &str, output_name: &str) -> Result<Self> {
        match s {
            "http_json" => Ok(HttpProtocol::Json),
            "http_protobuf" => Ok(HttpProtocol::Protobuf),
            other => bail!(
                "output '{}': unknown protocol '{}' (expected http_protobuf or http_json)",
                output_name,
                other
            ),
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            HttpProtocol::Json => "application/json",
            HttpProtocol::Protobuf => "application/x-protobuf",
        }
    }
}

struct HttpPeer {
    endpoint: String,
    client: reqwest::Client,
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
    peers: Vec<HttpPeer>,
    /// Per-peer cooldown state. Same length as `peers`. Wrapped in
    /// `Mutex` because `Instant` is not `Copy`-loadable from an atomic
    /// — but contention is low (one short critical section per flush
    /// per peer).
    peer_state: Vec<PeerState>,
    /// Round-robin cursor. Incremented per flush so successive flushes
    /// start at successive peers; the rotation inside one flush handles
    /// retries.
    cursor: AtomicUsize,
    protocol: HttpProtocol,
    batch_level: BatchLevel,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    /// Per-batch retry policy. The shared `RetryConfig` parser used by
    /// the file / syslog_tcp / http outputs reads `retry { max_attempts
    /// initial_wait max_wait backoff }` from the output's properties;
    /// we re-use it so every limpid output speaks the same retry
    /// vocabulary.
    ///
    /// Internal retry matters for OTLP specifically because it batches
    /// Events from multiple `write()` calls — without an internal
    /// retry, a single transient ship failure would lose the whole
    /// drained batch (the queue layer's per-event retry only re-pushes
    /// the most recent Event).
    retry_config: RetryConfig,
    /// Buffered per-Event singleton ResourceLogs proto bytes.
    batch: Mutex<Vec<Bytes>>,
}

pub struct OtlpHttpOutput {
    /// Operator-facing instance name; surfaced on PR-P shutdown-recovery
    /// records as `(output <name> shutdown)`.
    name: String,
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

const HTTP_PEER_SCHEMA: &[PropertySpec] = &[
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

const HTTP_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(HTTP_PEER_SCHEMA),
}];

const OTLP_HTTP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    // Shorthand for the common single-collector case; mirrors the
    // syslog_tcp ergonomics. One of `peer` (single) or `peers` (multi)
    // is required; both at once is rejected by the schema layer.
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(HTTP_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(HTTP_PEERS_SCHEMA),
    },
    PropertySpec {
        name: "protocol",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["http_json", "http_protobuf"]),
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
        name: "verify",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Bool,
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

/// Parse one `peer { endpoint tls{...} }` block. `verify` is a
/// top-level toggle (applies to every peer); it is threaded in here so
/// the reqwest builder gets the right `danger_accept_invalid_certs`
/// flag at construction time.
fn parse_peer(name: &str, peer_props: &[Property], verify: bool) -> Result<HttpPeer> {
    let endpoint = props::get_string(peer_props, "endpoint")
        .ok_or_else(|| anyhow!("output '{}': otlp_http peer requires 'endpoint'", name))?;
    let tls_block = props::get_block(peer_props, "tls");
    if tls_block.is_some() && !endpoint.to_ascii_lowercase().starts_with("https://") {
        // A `tls { ... }` block on a plaintext `http://` endpoint is
        // almost always an operator error: reqwest would silently
        // ignore the CA / client-cert settings (TLS only kicks in on
        // an https scheme) and the daemon would happily ship in clear
        // text. Refuse at parse time so the misconfiguration is
        // visible.
        bail!(
            "output '{}': otlp_http peer endpoint '{}' uses a plaintext scheme but a tls {{ ... }} block was supplied — switch the endpoint to https:// or drop the tls block",
            name,
            endpoint
        );
    }
    let tls_config = tls_block
        .map(|block| {
            let cfg = ClientTlsConfig {
                ca_path: props::get_string(block, "ca"),
                cert_path: props::get_string(block, "cert"),
                key_path: props::get_string(block, "key"),
            };
            cfg.validate(&format!("output '{}'", name))?;
            Ok::<_, anyhow::Error>(cfg)
        })
        .transpose()?;

    let mut builder = reqwest::Client::builder().timeout(HTTP_REQUEST_TIMEOUT);
    if !verify {
        builder = builder.danger_accept_invalid_certs(true);
        if endpoint.to_ascii_lowercase().starts_with("https://") {
            // Loud, unconditional warning when TLS verification is
            // disabled on an https endpoint. `verify false` is a
            // config-level footgun — one line opens MITM. Emit the
            // warning once per peer at startup so ops can grep for
            // it. Mirrors the matching warn in `output http`.
            tracing::warn!(
                "output '{}': TLS certificate verification is DISABLED (verify false) — \
                 connections to {} are vulnerable to MITM. Debugging only; never use in production.",
                name,
                endpoint
            );
        }
    }
    if let Some(tls) = &tls_config {
        if let Some(ca_path) = &tls.ca_path {
            let pem = std::fs::read(ca_path)
                .with_context(|| format!("output '{}': cannot read CA cert {}", name, ca_path))?;
            let cert = reqwest::Certificate::from_pem(&pem).with_context(|| {
                format!("output '{}': invalid CA cert PEM at {}", name, ca_path)
            })?;
            builder = builder.add_root_certificate(cert);
        }
        if let (Some(cert_path), Some(key_path)) = (&tls.cert_path, &tls.key_path) {
            let cert_pem = std::fs::read(cert_path).with_context(|| {
                format!("output '{}': cannot read client cert {}", name, cert_path)
            })?;
            let key_pem = std::fs::read(key_path).with_context(|| {
                format!("output '{}': cannot read client key {}", name, key_path)
            })?;
            // reqwest expects the identity as a concatenated PEM blob
            // (cert chain followed by the private key); we hand-build it
            // here so users can keep cert and key in separate files
            // (matches the syslog_tcp / kafka mTLS disposition).
            let mut combined = cert_pem.clone();
            if !combined.ends_with(b"\n") {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&combined).with_context(|| {
                format!(
                    "output '{}': invalid client cert/key PEM ({}, {})",
                    name, cert_path, key_path
                )
            })?;
            builder = builder.identity(identity);
        }
    }

    let client = builder
        .build()
        .with_context(|| format!("output '{}': failed to build HTTP client", name))?;

    Ok(HttpPeer { endpoint, client })
}

impl Module for OtlpHttpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(OTLP_HTTP_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();

        let protocol_str = props::get_string(properties, "protocol")
            .or_else(|| props::get_ident(properties, "protocol"))
            .unwrap_or_else(|| "http_protobuf".to_string());
        let protocol = HttpProtocol::parse(&protocol_str, name)?;
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

        let verify = props::get_ident(properties, "verify")
            .map(|s| s != "false")
            .unwrap_or(true);

        // Single-peer shorthand (`peer { endpoint ... }`) or multi-peer
        // (`peers { peer { ... } ... }`). The schema's exclusive_group
        // already forbids both at once, so we just probe in priority
        // order.
        let peers = if let Some(peer_block) = props::get_block(properties, "peer") {
            vec![parse_peer(name, peer_block, verify)?]
        } else if let Some(peers_block) = props::get_block(properties, "peers") {
            iter_peers_block(
                peers_block,
                &format!("output '{}': peers", name),
                |peer_props| parse_peer(name, peer_props, verify),
            )?
        } else {
            anyhow::bail!(
                "output '{}': otlp_http requires a 'peer {{ ... }}' or 'peers {{ peer {{ ... }} ... }}' block",
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
                protocol,
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

impl HasMetrics for OtlpHttpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for OtlpHttpOutput {
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

    /// Owned-event path (disk-queue replay, control-socket inject). See
    /// the rationale on `OtlpGrpcOutput::write_owned`: the queue
    /// consumer needs a per-event ship verdict, and the batched buffer
    /// would lose it. Ship inline here, bypassing the batch.
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
        // Abort any pending timer so it cannot race with us for the
        // buffer lock. Then drain whatever is left in one final
        // send. Without this the queue consumer's `shutdown()` call
        // would race the timer and the in-flight events could be
        // dropped between the timer's abort (via `Drop`) and the
        // process exit.
        if let Some(h) = self.flush_handle.lock().await.take() {
            h.abort();
        }
        match self.flush().await {
            Ok(()) => Ok(()),
            Err(e) => {
                // BC-4 / PR-P: drain the put-back batch into
                // `error_log` when configured; preserve 0.7.7
                // (warn + drop via the queue consumer) when not.
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

impl OtlpHttpOutput {
    /// Drain the current batch, build an ExportLogsServiceRequest, and
    /// ship it. No-op if the buffer is empty.
    async fn flush(&self) -> Result<()> {
        let drained: Vec<Bytes> = {
            let mut batch = self.inner.batch.lock().await;
            std::mem::take(&mut *batch)
        };
        if drained.is_empty() {
            return Ok(());
        }
        let count = drained.len() as u64;
        // Clone for the send so the original is available for
        // restoration on transport error. `Bytes::clone` is a cheap
        // refcount bump (no payload copy), so this scales linearly
        // with batch length rather than with batch byte size.
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
                // the buffer for retry instead of dropping it. The
                // queue layer counts Rendered payloads as failed and
                // does not replay them (see queue/mod.rs), so this
                // is the only chance these events have. `output
                // http` does the same (PR limpid#33); OTLP was the
                // odd one out. Do NOT bump events_failed here — the
                // events are retained, not permanently rejected.
                let mut batch = self.inner.batch.lock().await;
                let new_events = std::mem::take(&mut *batch);
                *batch = drained;
                batch.extend(new_events);
                Err(e)
            }
        }
    }

    /// Schedule (or refresh) a deferred flush so events do not sit in
    /// the buffer indefinitely when traffic is below `batch_size`.
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
                    // flush above: put the drained batch back so the
                    // next write or timer firing can retry, instead
                    // of silently dropping events here.
                    tracing::warn!(
                        "otlp_http flush timer: send failed ({}) — {} events returned to buffer",
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

impl Drop for OtlpHttpOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
        if let Ok(buf) = self.inner.batch.try_lock()
            && !buf.is_empty()
        {
            tracing::warn!(
                "otlp_http output: {} events in buffer at shutdown (will be re-delivered from queue)",
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
        // Pick the next peer to try. Rotation starts at `cursor` and
        // walks forward looking for one whose cooldown has expired. If
        // every peer is currently cooled (typical single-peer retry
        // path: the peer just failed on the previous attempt) fall
        // back to the rotation start — the retry budget is what
        // protects us, not the cooldown. The cooldown's actual job is
        // to bias *future flushes* away from a known-bad peer when an
        // alternative exists.
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
                // Reset cooldown on success so future flushes pick
                // this peer freely.
                *inner.peer_state[idx].cooldown_until.lock().await = None;
                return Ok(outcome);
            }
            Err(e) => {
                // Measure cooldown from failure time, not request start:
                // `now` was captured before `send_once`, so for any non-
                // trivial request latency (and especially after a 30s
                // HTTP_REQUEST_TIMEOUT firing) `now + PEER_COOLDOWN`
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
            "otlp_http output: ship attempt {}/{} failed: {} — retrying in {:?}",
            attempt,
            max_attempts,
            err,
            wait,
        );
        tokio::time::sleep(wait).await;
        if matches!(cfg.backoff, BackoffStrategy::Exponential) {
            // saturating_mul is the safe doubling: `Duration * 2`
            // panics on overflow (~584 years), and while the practical
            // reach of that limit is "never", making the bound
            // explicit is the defensive choice — `.min(max_wait)` then
            // clamps back to the configured ceiling.
            wait = wait.saturating_mul(2).min(cfg.max_wait);
        }
    };
    Err(final_err)
}

async fn send_once(
    peer: &HttpPeer,
    inner: &Inner,
    req: &ExportLogsServiceRequest,
) -> Result<super::SendOutcome> {
    let body = match inner.protocol {
        HttpProtocol::Protobuf => {
            let mut buf = Vec::with_capacity(req.encoded_len());
            req.encode(&mut buf)
                .map_err(|e| anyhow!("output otlp_http: protobuf encode failed: {e}"))?;
            buf
        }
        HttpProtocol::Json => serde_json::to_vec(req)
            .map_err(|e| anyhow!("output otlp_http: JSON encode failed: {e}"))?,
    };
    let mut http_req = peer
        .client
        .post(&peer.endpoint)
        .header("Content-Type", inner.protocol.content_type())
        .body(body);
    for (k, v) in &inner.headers {
        http_req = http_req.header(k, v);
    }
    let resp = http_req
        .send()
        .await
        .with_context(|| format!("output otlp_http: POST {} failed", peer.endpoint))?;
    let status = resp.status();
    if !status.is_success() {
        // 4 KiB byte cap, 500 chars on the lossy decode for the log
        // line; gzip / brotli / deflate get a placeholder so the
        // daemon log doesn't fill with replacement-char soup (limpid's
        // reqwest is built without those decompression features).
        let snippet = error_snippet(resp, ERROR_BODY_BYTE_CAP, 500).await;
        bail!(
            "output otlp_http: {} returned HTTP {} — {}",
            peer.endpoint,
            status.as_u16(),
            snippet
        );
    }
    // Parse the response body for `partial_success.rejected_log_records`.
    // The OTLP/HTTP spec guarantees the body is an
    // `ExportLogsServiceResponse` in the same wire form (protobuf or
    // JSON) as the request. A peer that returns 2xx with an empty body,
    // or with a body we can't decode, is treated as fully accepted —
    // the alternative (failing the call) would convert lenient
    // collectors into ship errors. Same semantics as
    // `otlp_grpc::send_once` so the two transports report identical
    // metrics for identical receiver behaviour.
    let body_bytes = resp.bytes().await.unwrap_or_default();
    let rejected = if body_bytes.is_empty() {
        0
    } else {
        let parsed = match inner.protocol {
            HttpProtocol::Protobuf => ExportLogsServiceResponse::decode(&*body_bytes).ok(),
            HttpProtocol::Json => {
                serde_json::from_slice::<ExportLogsServiceResponse>(&body_bytes).ok()
            }
        };
        let r = parsed
            .as_ref()
            .and_then(|r| r.partial_success.as_ref())
            .map(|p| p.rejected_log_records.max(0) as u64)
            .unwrap_or(0);
        if r > 0
            && let Some(partial) = parsed.as_ref().and_then(|r| r.partial_success.as_ref())
        {
            tracing::warn!(
                "otlp_http: {} rejected {} log record(s){}",
                peer.endpoint,
                partial.rejected_log_records,
                if partial.error_message.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", partial.error_message)
                }
            );
        }
        r
    };
    Ok(super::SendOutcome { rejected })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::event::Event;
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use std::net::SocketAddr;

    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("otlp_http", props.to_vec())
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
        let err = OtlpHttpOutput::from_properties("o", &mp(&[]))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("'peer {") && err.to_string().contains("'peers {"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn accepts_single_peer_shorthand() {
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("endpoint", "http://x:4318/v1/logs")],
        }];
        let output = OtlpHttpOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 1);
        assert_eq!(output.inner.peers[0].endpoint, "http://x:4318/v1/logs");
    }

    #[test]
    fn rejects_peers_block_with_no_peer() {
        let props = vec![peers_block_with(vec![])];
        let err = OtlpHttpOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("at least one peer"));
    }

    #[test]
    fn peer_requires_endpoint() {
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![],
        }])];
        let err = OtlpHttpOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn defaults_protocol_to_http_protobuf() {
        let output =
            OtlpHttpOutput::from_properties("o", &mp(&one_peer_props("http://x"))).unwrap();
        assert!(matches!(output.inner.protocol, HttpProtocol::Protobuf));
    }

    #[test]
    fn rejects_unknown_protocol_value() {
        let mut props = one_peer_props("http://x");
        props.push(prop_str("protocol", "carrier_pigeon"));
        let err = OtlpHttpOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn batch_level_default_is_none() {
        let output =
            OtlpHttpOutput::from_properties("o", &mp(&one_peer_props("http://x"))).unwrap();
        assert!(matches!(output.inner.batch_level, BatchLevel::None));
    }

    #[test]
    fn batch_size_defaults_to_one() {
        let output =
            OtlpHttpOutput::from_properties("o", &mp(&one_peer_props("http://x"))).unwrap();
        assert_eq!(output.batch_size, 1);
    }

    #[test]
    fn parses_multi_peer_block() {
        let props = vec![peers_block_with(vec![
            peer_block("http://a"),
            peer_block("http://b"),
            peer_block("http://c"),
        ])];
        let output = OtlpHttpOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 3);
        assert_eq!(output.inner.peers[0].endpoint, "http://a");
        assert_eq!(output.inner.peers[2].endpoint, "http://c");
    }

    #[test]
    fn rejects_tls_with_cert_but_no_key() {
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://x"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("cert", "/c.pem")],
                },
            ],
        }])];
        let err = OtlpHttpOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        assert!(err.to_string().contains("cert and key"));
    }

    #[test]
    fn retry_block_overrides_defaults() {
        let mut props = one_peer_props("http://x");
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 2),
                prop_str("initial_wait", "100ms"),
                prop_str("max_wait", "500ms"),
            ],
        });
        let output = OtlpHttpOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.retry_config.max_attempts, 2);
        assert_eq!(
            output.inner.retry_config.initial_wait,
            Duration::from_millis(100)
        );
    }

    // ---- wire-level round-trips ----

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

    async fn run_http_collector(
        protocol: &'static str,
    ) -> (
        SocketAddr,
        Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
        };

        #[derive(Clone)]
        struct AppState {
            received: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
            protocol: &'static str,
        }

        async fn handle(
            State(state): State<AppState>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let ct = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let req: ExportLogsServiceRequest = match state.protocol {
                "http_protobuf" => {
                    if !ct.starts_with("application/x-protobuf") {
                        return (
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            format!("expected protobuf, got {ct:?}"),
                        )
                            .into_response();
                    }
                    match ExportLogsServiceRequest::decode(&*body) {
                        Ok(r) => r,
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("decode: {e}"))
                                .into_response();
                        }
                    }
                }
                "http_json" => {
                    if !ct.starts_with("application/json") {
                        return (
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            format!("expected json, got {ct:?}"),
                        )
                            .into_response();
                    }
                    match serde_json::from_slice(&body) {
                        Ok(r) => r,
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("json: {e}")).into_response();
                        }
                    }
                }
                _ => unreachable!("test-only enumeration"),
            };
            state.received.lock().await.push(req);
            (StatusCode::OK, "").into_response()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/logs", post(handle))
            .with_state(AppState {
                received: Arc::clone(&received),
                protocol,
            });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, received, handle)
    }

    #[tokio::test]
    async fn retries_until_success_single_peer() {
        use axum::{
            Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
        };
        use std::sync::atomic::Ordering as AtomicOrdering;

        #[derive(Clone)]
        struct AppState {
            attempts: Arc<AtomicUsize>,
            fail_until: usize,
        }

        async fn handle(
            State(state): State<AppState>,
            _body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let n = state.attempts.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            if n <= state.fail_until {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/logs", post(handle))
            .with_state(AppState {
                attempts: Arc::clone(&attempts),
                fail_until: 2,
            });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 1));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 5),
                prop_str("initial_wait", "10ms"),
                prop_str("max_wait", "50ms"),
            ],
        });
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();

        output
            .write_owned(&event_with_egress(singleton_bytes(123)))
            .await
            .unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn rotates_to_healthy_peer_when_first_fails() {
        // Two peers: A always 500, B always 200. Round-robin starts at
        // A, fails, cools it down, retries onto B which succeeds.
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        async fn fail(_: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        async fn ok(_: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::OK
        }

        let l_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l_a.local_addr().unwrap();
        let l_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = l_b.local_addr().unwrap();
        let app_a = Router::new().route("/v1/logs", post(fail));
        let app_b = Router::new().route("/v1/logs", post(ok));
        let s_a = tokio::spawn(async move {
            let _ = axum::serve(l_a, app_a).await;
        });
        let s_b = tokio::spawn(async move {
            let _ = axum::serve(l_b, app_b).await;
        });

        let props = vec![
            peers_block_with(vec![
                peer_block(&format!("http://{}/v1/logs", a)),
                peer_block(&format!("http://{}/v1/logs", b)),
            ]),
            prop_str("protocol", "http_protobuf"),
            prop_int("batch_size", 1),
            Property::Block {
                key: "retry".into(),
                key_span: None,
                properties: vec![
                    prop_int("max_attempts", 3),
                    prop_str("initial_wait", "10ms"),
                    prop_str("max_wait", "20ms"),
                ],
            },
        ];
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(42)))
            .await
            .unwrap();

        s_a.abort();
        s_b.abort();
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_all_peers_fail() {
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        async fn always_fail(_: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::SERVICE_UNAVAILABLE
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/logs", post(always_fail));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 1));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 3),
                prop_str("initial_wait", "10ms"),
                prop_str("max_wait", "20ms"),
            ],
        });
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with_egress(singleton_bytes(456)))
            .await
            .expect_err("send must fail after retries exhausted");
        assert!(
            err.to_string().contains("503") || err.to_string().contains("HTTP"),
            "unexpected error after retry exhaustion: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn round_trip_protobuf() {
        let (addr, received, server) = run_http_collector("http_protobuf").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 1));
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(123)))
            .await
            .unwrap();
        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
            123
        );
    }

    #[tokio::test]
    async fn round_trip_json() {
        let (addr, received, server) = run_http_collector("http_json").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_json"));
        props.push(prop_int("batch_size", 1));
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(456)))
            .await
            .unwrap();
        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
            456
        );
    }

    #[tokio::test]
    async fn partial_success_rejected_log_records_routes_to_events_failed() {
        // Regression guard: when the receiver returns 2xx with a body
        // containing `partial_success.rejected_log_records = N`,
        // events_written must NOT cover the N rejected records. The
        // pre-fix code did not parse the response body at all, so
        // operators saw zero events_failed for server-side rejections.
        // Mirror of the equivalent test in otlp_grpc — both transports
        // must report identical metrics for identical receiver
        // behaviour.
        use axum::{
            Router, body::Bytes as AxumBytes, extract::State, http::StatusCode,
            response::IntoResponse, routing::post,
        };
        use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsPartialSuccess;
        use std::sync::atomic::Ordering as AtomicOrdering;

        #[derive(Clone)]
        struct AppState {
            received_count: Arc<AtomicUsize>,
        }

        async fn handle(State(state): State<AppState>, body: AxumBytes) -> impl IntoResponse {
            // Decode the request only to confirm it parsed; the test
            // exercise is the response body, not the request.
            let _ = ExportLogsServiceRequest::decode(&body[..]);
            state.received_count.fetch_add(1, AtomicOrdering::SeqCst);
            // Always claim 2 records were rejected.
            let resp = ExportLogsServiceResponse {
                partial_success: Some(ExportLogsPartialSuccess {
                    rejected_log_records: 2,
                    error_message: "test partial-success".into(),
                }),
            };
            let mut buf = Vec::with_capacity(resp.encoded_len());
            resp.encode(&mut buf).unwrap();
            (
                StatusCode::OK,
                [("content-type", "application/x-protobuf")],
                buf,
            )
                .into_response()
        }

        let received_count = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/logs", post(handle))
            .with_state(AppState {
                received_count: Arc::clone(&received_count),
            });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 3));
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();

        let mut payloads: Vec<RenderedPayload> = Vec::new();
        for i in 0..3 {
            let ev = event_with_egress(singleton_bytes(900_000_000 + i));
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
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
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

    #[tokio::test]
    async fn write_owned_bypasses_batch_buffer() {
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with_egress(singleton_bytes(1)))
            .await
            .expect_err("send must fail against unreachable peer");
        assert!(
            err.to_string().contains("otlp_http"),
            "expected ship error, got: {err}"
        );
        let batch_len = output.inner.batch.lock().await.len();
        assert_eq!(batch_len, 0, "Owned event must not land in the batch");
        let timer_armed = output.flush_handle.lock().await.is_some();
        assert!(!timer_armed, "Owned event must not arm the flush timer");
    }

    #[tokio::test]
    async fn flush_failure_restores_batch_to_buffer() {
        // Regression: `flush()` used to drain the batch and, on
        // transport failure, just bump events_failed and drop the
        // drained events. `output http` retained them for retry; OTLP
        // was the odd one out. The fix restores the drained batch
        // into the buffer so the next write or timer firing has a
        // chance to ship them.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 2));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt against the unreachable peer so the test
        // doesn't sit through the default retry backoff.
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();

        // First write fills the buffer below batch_size — no flush.
        let arena_bump = bumpalo::Bump::new();
        let arena = EventArena::new(&arena_bump);
        let e1 = event_with_egress(singleton_bytes(1));
        let p1 = output.render(&e1.view_in(&arena), &arena).unwrap();
        output.write(p1).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 1);

        // Second write tips the batch over batch_size, triggers
        // flush() against the unreachable peer, which must Err.
        let e2 = event_with_egress(singleton_bytes(2));
        let p2 = output.render(&e2.view_in(&arena), &arena).unwrap();
        let err = output
            .write(p2)
            .await
            .expect_err("flush against unreachable peer must fail");
        assert!(
            err.to_string().contains("otlp_http"),
            "expected ship error, got: {err}"
        );

        // Regression assertion: both events must still be in the
        // buffer (old code dropped them; the events_failed counter
        // would have been bumped to 2).
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

    #[tokio::test]
    async fn shutdown_flushes_pending_batch_buffer() {
        // Regression mirror of `output http`: when batch_size > 1 the
        // queue-side `write()` returns Ok once the event is in the
        // buffer, so the memory queue considers it delivered. If the
        // daemon shuts down before the batch fills, Drop alone aborts
        // the timer and leaks the buffer. `shutdown()` aborts the
        // timer and runs one final flush.
        let (addr, received, server) = run_http_collector("http_protobuf").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        // Large batch + long timer: nothing but `shutdown()` can
        // drain this buffer.
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();

        // Drive the batched path with two render→write round trips.
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
            "writes must land in the buffer (batch_size and timer both far away)"
        );
        assert!(received.lock().await.is_empty());

        output.shutdown(None).await.unwrap();

        assert_eq!(
            output.inner.batch.lock().await.len(),
            0,
            "shutdown() must drain the buffer"
        );

        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        let got = received.lock().await.clone();
        assert_eq!(got.len(), 1, "shutdown flush must POST exactly once");
        let record_count: usize = got[0]
            .resource_logs
            .iter()
            .flat_map(|rl| rl.scope_logs.iter())
            .map(|sl| sl.log_records.len())
            .sum();
        assert_eq!(
            record_count, 2,
            "shutdown POST must carry both buffered records",
        );
    }

    #[test]
    fn rejects_tls_block_on_plaintext_endpoint() {
        // `tls { ... }` on an `http://` endpoint is almost always an
        // operator error — reqwest silently ignores the CA / client
        // cert when the URL is plaintext, so the daemon would ship in
        // clear text without any indication that the tls block did
        // nothing. Fail fast at parse time.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "http://collector.example.com:4318/v1/logs"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("ca", "/etc/ca.pem")],
                },
            ],
        }])];
        let err = OtlpHttpOutput::from_properties("o", &mp(&props))
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("plaintext") && msg.contains("https://"),
            "unexpected: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn export_timeout_fires_against_stalled_peer() {
        // A peer that accepts the TCP connection but never sends a
        // response must surface as a timeout failure within
        // HTTP_REQUEST_TIMEOUT (30 s). Without `reqwest::Client::
        // builder().timeout(HTTP_REQUEST_TIMEOUT)` a single stalled
        // collector would block the rotation forever. Constant-value
        // checks (or a non-existent constant rename) wouldn't catch a
        // regression that removed the builder call or pointed it at
        // a much larger duration — this test exercises the firing
        // path end-to-end.
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        // Stalled "server": accept the TCP connection and hold the
        // socket open without ever writing a response line, so reqwest
        // hangs waiting for HTTP status bytes.
        let stall = tokio::spawn(async move {
            let held = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    held.lock().await.push(sock);
                }
            }
        });

        let endpoint = format!("http://{}/v1/logs", addr);
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
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        let send = tokio::spawn(async move {
            output
                .write_owned(&event_with_egress(singleton_bytes(1)))
                .await
        });

        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(HTTP_REQUEST_TIMEOUT + Duration::from_secs(1)).await;

        let result = send.await.unwrap();
        stall.abort();

        let err = result.expect_err("stalled peer must surface as Err");
        // Walk the anyhow chain; reqwest's timeout shows up as
        // `error sending request for url (...): operation timed out`
        // in the underlying reqwest::Error, while the top-level
        // anyhow context says "POST ... failed".
        let mut chain_text = String::new();
        let mut current: &dyn std::error::Error = err.as_ref();
        chain_text.push_str(&current.to_string());
        while let Some(src) = current.source() {
            chain_text.push_str(" -> ");
            chain_text.push_str(&src.to_string());
            current = src;
        }
        let chain_lower = chain_text.to_ascii_lowercase();
        assert!(
            chain_lower.contains("timed out") || chain_lower.contains("timeout"),
            "expected timeout-flavoured error somewhere in the chain, got: {chain_text}"
        );
    }

    #[test]
    fn accepts_empty_tls_block_on_https_endpoint() {
        // Regression guard for the plaintext-rejection check: https://
        // endpoints must still accept a (here empty) tls block. We use
        // an empty block so the test doesn't try to read a CA pem off
        // disk; the validation we care about (scheme check) runs
        // before the file read.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://collector.example.com:4318/v1/logs"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![],
                },
            ],
        }])];
        let output = OtlpHttpOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    // -----------------------------------------------------------------------
    // BC-4 / PR-P: shutdown-time buffer-loss recovery via `error_log`.
    // -----------------------------------------------------------------------

    fn shutdown_recovery_props(endpoint: &str) -> Vec<Property> {
        let mut props = one_peer_props(endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt, minimal wait → flush against an unreachable
        // peer fails quickly.
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

    async fn buffer_two(output: &OtlpHttpOutput) {
        let arena_bump = bumpalo::Bump::new();
        let arena = EventArena::new(&arena_bump);
        for ts in [1u64, 2u64] {
            let ev = event_with_egress(singleton_bytes(ts));
            let payload = output.render(&ev.view_in(&arena), &arena).unwrap();
            output.write(payload).await.unwrap();
        }
        assert_eq!(output.inner.batch.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shutdown_failure_with_error_log_persists_buffer() {
        let props = shutdown_recovery_props("http://127.0.0.1:1/v1/logs");
        let output = OtlpHttpOutput::from_properties("myout", &mp(&props)).unwrap();
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
        let props = shutdown_recovery_props("http://127.0.0.1:1/v1/logs");
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let err = output.shutdown(None).await.expect_err("flush must Err");
        assert!(err.to_string().contains("otlp_http"), "got: {err}");
        assert_eq!(output.inner.batch.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shutdown_success_does_not_touch_error_log() {
        let (addr, received, server) = run_http_collector("http_protobuf").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_str("protocol", "http_protobuf"));
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));

        output.shutdown(Some(&writer)).await.unwrap();
        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        assert!(!received.lock().await.is_empty(), "shutdown must POST");
        assert!(!path.exists(), "DLQ must stay untouched on clean shutdown");
    }

    #[tokio::test]
    async fn shutdown_recovery_writer_failure_does_not_recurse() {
        let props = shutdown_recovery_props("http://127.0.0.1:1/v1/logs");
        let output = OtlpHttpOutput::from_properties("test", &mp(&props)).unwrap();
        buffer_two(&output).await;

        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(
            std::path::PathBuf::from("/nonexistent/limpid-otlp-http-test/errored.jsonl"),
        ));
        output.shutdown(Some(&writer)).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 0);
    }
}

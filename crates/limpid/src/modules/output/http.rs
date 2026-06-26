//! HTTP output: sends events to one or more HTTP/HTTPS endpoints.
//!
//! Supports Elasticsearch Bulk API, Splunk HEC, Datadog, Loki, and any
//! generic HTTP endpoint. Multiple peers are tried in round-robin
//! order with per-peer cooldown on failure.
//!
//! ```text
//! def output es_cluster {
//!     type http
//!     peers {
//!         peer {
//!             url "https://es01.example.com:9200/_bulk"
//!             tls { ca "/etc/limpid/ca.crt" }
//!         }
//!         peer {
//!             url "https://es02.example.com:9200/_bulk"
//!             tls {
//!                 ca   "/etc/limpid/ca.crt"
//!                 cert "/etc/limpid/client.crt"   # mTLS
//!                 key  "/etc/limpid/client.key"
//!             }
//!         }
//!     }
//!     method POST
//!     content_type "application/json"
//!     headers { Authorization "Bearer xxx" }
//! }
//! ```
//!
//! Single-peer setups use the `peer { ... }` shorthand (same shape
//! `output syslog_tcp` and `output otlp_http` accept):
//!
//! ```text
//! def output one {
//!     type http
//!     peer {
//!         url "https://es.example.com:9200/_bulk"
//!         tls { ca "/etc/limpid/ca.crt" }
//!     }
//! }
//! ```
//!
//! ### Round-robin + cooldown
//!
//! On each send the rotation picks the next available peer (cooldown
//! expired) and tries it. On failure that peer is marked cooled-down
//! for `PEER_COOLDOWN` (5 s, shared with the syslog / otlp outputs)
//! and subsequent sends rotate past it until the cooldown expires.
//! Within a single send the rotation falls back to the cursor start
//! when every peer is cooled (single-peer retry path) — the output's
//! own retry loop (driven by `retry { ... }`) then handles re-delivery
//! on persistent failure. The queue layer advances its cursor when the
//! ack handle resolves, not when `consume` returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::http_util::{ERROR_BODY_BYTE_CAP, error_snippet};
use crate::modules::output::syslog_peers::{PEER_COOLDOWN, iter_peers_block};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};
use crate::tls::ClientTlsConfig;

const HTTP_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "url",
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

const HTTP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    // Single-peer shorthand or multi-peer block. Exactly one of the
    // two must be present; both at once is rejected by the schema
    // layer.
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
    // Verbs accepted by reqwest — kept as plain String rather than an
    // Enum so users can pass uncommon ones (PROPFIND, MKCOL, etc.)
    // without bumping the schema.
    PropertySpec {
        name: "method",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "content_type",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
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
        name: "verify",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Bool,
    },
    PropertySpec {
        name: "compress",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["gzip"]),
    },
    PropertySpec {
        name: "headers",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::StringMap,
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

struct HttpPeer {
    url: String,
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

/// Shared state between `consume()` (= the queue consumer's hand-off
/// into the buffer) and the flush timer task.
struct Inner {
    peers: Vec<HttpPeer>,
    peer_state: Vec<PeerState>,
    cursor: AtomicUsize,
    method: reqwest::Method,
    content_type: String,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    compress: bool,
    /// Buffered events awaiting flush, paired with their queue ack
    /// handles. Render happens at flush time so per-event render
    /// failures can be routed to DLQ on their own without dropping
    /// the rest of the batch; the ack handle resolves when the
    /// event's disposition is decided (delivered on flush success,
    /// recovered on DLQ landing).
    batch: Mutex<Vec<(Event, QueueAckHandle)>>,
    /// Operator-facing instance name; surfaced on shutdown-flush
    /// recovery and render-failure records.
    name: String,
    /// Per-flush retry policy. Without an internal retry, one
    /// transient ship failure would lose the whole drained batch
    /// (the queue layer cannot re-push a buffered batch — its cursor
    /// only advances when each event's ack handle resolves). The
    /// retry budget for batched outputs lives entirely here.
    retry: RetryConfig,
    /// `error_log` writer injected at construction time by the
    /// runtime via `BuildContext` in `from_properties`.
    /// Used by the flush path to route per-event render failures and
    /// shutdown-flush leftovers into the DLQ. `None` when the
    /// operator did not configure `control { error_log "..." }` —
    /// the flush path then falls back to a `tracing::error!` line.
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    metrics: Arc<OutputMetrics>,
}

pub struct HttpOutput {
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

/// Parse one `peer { url tls{...} }` block and build the per-peer
/// `reqwest::Client` with the appropriate root CA / mTLS identity.
/// `verify` is top-level (applies to every peer) — it disables
/// certificate verification globally when set to `false`; the per-peer
/// `tls` block is then ignored. The `name` and `output_label` strings
/// only affect error context wording.
fn parse_peer(name: &str, peer_props: &[Property], verify: bool) -> Result<HttpPeer> {
    let url = props::get_string(peer_props, "url")
        .ok_or_else(|| anyhow::anyhow!("output '{}': http peer requires 'url'", name))?;

    let is_https = url.starts_with("https://");
    let tls_block = props::get_block(peer_props, "tls");
    let has_tls_block = tls_block.is_some();

    if !is_https && has_tls_block {
        tracing::warn!(
            "output '{}': 'tls' block on peer '{}' has no effect on non-HTTPS URL",
            name,
            url
        );
    }
    if is_https && !verify {
        // Loud, unconditional warning when TLS verification is
        // disabled on HTTPS. `verify false` is a config-level
        // footgun — one line opens MITM. Emit the warning once at
        // startup so ops can grep for it, regardless of whether a
        // `tls { ca ... }` block is also present.
        tracing::warn!(
            "output '{}': TLS certificate verification is DISABLED (verify false) — \
             connections to {} are vulnerable to MITM. Debugging only; never use in production.",
            name,
            url
        );
    }

    // Parse the tls block regardless of `verify`. Client certs / keys
    // (mTLS identity) must still be applied when verification is
    // disabled — `verify false` only relaxes server-cert checks, the
    // server may still demand a client certificate.
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

    if !verify
        && let Some(tls) = &tls_config
        && tls.ca_path.is_some()
    {
        // CA bundling is meaningless when verification is off; warn the
        // operator so the misconfiguration is visible, then proceed
        // with the remaining (identity) bits of the tls block.
        tracing::warn!(
            "output '{}': peer '{}' ignores tls.ca because 'verify false' disables certificate validation",
            name,
            url
        );
    }

    let mut client_builder = reqwest::Client::builder().timeout(Duration::from_secs(30));

    if !verify {
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }

    if let Some(tls) = &tls_config {
        // Skip CA loading when verify is off — `danger_accept_invalid_certs`
        // already short-circuits server-cert validation, and adding a root
        // would be wasted work (and could mask the warning above).
        if verify && let Some(ca_path) = &tls.ca_path {
            let pem = std::fs::read(ca_path).with_context(|| {
                format!("output '{}': failed to read CA cert: {}", name, ca_path)
            })?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("output '{}': invalid CA cert: {}", name, ca_path))?;
            client_builder = client_builder.add_root_certificate(cert);
        }
        if let (Some(cert_path), Some(key_path)) = (&tls.cert_path, &tls.key_path) {
            let cert_pem = std::fs::read(cert_path).with_context(|| {
                format!("output '{}': cannot read client cert {}", name, cert_path)
            })?;
            let key_pem = std::fs::read(key_path).with_context(|| {
                format!("output '{}': cannot read client key {}", name, key_path)
            })?;
            // reqwest expects the identity as a concatenated PEM blob
            // (cert chain followed by the private key); we hand-build
            // it here so users can keep cert and key in separate files
            // (matches the syslog_tcp / kafka / otlp mTLS disposition).
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
            client_builder = client_builder.identity(identity);
        }
    }

    let client = client_builder
        .build()
        .with_context(|| format!("output '{}': failed to build HTTP client", name))?;

    Ok(HttpPeer { url, client })
}

impl Module for HttpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(HTTP_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::modules::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let error_log = ctx.error_log.as_ref().map(Arc::clone);
        let properties = properties.user_properties();

        // Parse the configured method into a typed `reqwest::Method`
        // at config-load time so unsupported verbs fail fast (rather
        // than at the first send) and so the hot path doesn't reparse
        // per request. Reqwest's `FromStr` covers every standard
        // method (GET / POST / PUT / DELETE / PATCH / HEAD / OPTIONS
        // / CONNECT / TRACE) plus any RFC-compliant extension token.
        let method_str = props::get_ident(properties, "method")
            .unwrap_or_else(|| "POST".to_string())
            .to_uppercase();
        let method = method_str.parse::<reqwest::Method>().with_context(|| {
            format!(
                "output '{}': invalid http method '{}' (expected GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS or an RFC-compliant extension token)",
                name, method_str
            )
        })?;
        let content_type = props::get_string(properties, "content_type")
            .unwrap_or_else(|| "application/json".to_string());
        let batch_size = props::get_positive_int(properties, "batch_size")?.unwrap_or(1) as usize;
        let batch_timeout = match props::get_string(properties, "batch_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };
        let compress = props::get_ident(properties, "compress")
            .map(|s| s == "gzip")
            .unwrap_or(false);
        let headers = props::get_string_map(properties, "headers");

        let verify = props::get_ident(properties, "verify")
            .map(|s| s != "false")
            .unwrap_or(true);

        // Single-peer shorthand (`peer { url ... }`) or multi-peer
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
                "output '{}': http requires a 'peer {{ ... }}' or 'peers {{ peer {{ ... }} ... }}' block",
                name
            );
        };

        let peer_state = peers.iter().map(|_| PeerState::default()).collect();

        let retry = RetryConfig::from_output_properties(properties)?;
        let metrics = Arc::new(OutputMetrics::default());
        Ok(Self {
            inner: Arc::new(Inner {
                peers,
                peer_state,
                cursor: AtomicUsize::new(0),
                method,
                content_type,
                headers,
                batch_timeout,
                compress,
                batch: Mutex::new(Vec::with_capacity(batch_size.max(1))),
                name: name.to_string(),
                retry,
                error_log,
                metrics: Arc::clone(&metrics),
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics,
        })
    }
}

impl HasMetrics for HttpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for HttpOutput {
    /// Batched-buffer consume: park the `(Event, ack)` pair in the
    /// in-memory buffer and arm/run the flush. The ack handle stays
    /// with the event and resolves at flush time (delivered or
    /// recovered) — not now. Returning `Ok(())` only signals that
    /// the output took ownership of the lifecycle.
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        if self.batch_size <= 1 {
            return self.consume_singleton(event, ack).await;
        }
        let should_flush = {
            let mut buf = self.inner.batch.lock().await;
            buf.push((event.clone(), ack));
            buf.len() >= self.batch_size
        };
        if should_flush {
            self.cancel_timer().await;
            self.flush().await;
            // After flush, the buffer should be empty (flush_events
            // resolved every handle). Defensive: re-arm if anything
            // raced its way back in.
            if !self.inner.batch.lock().await.is_empty() {
                self.reset_timer().await;
            }
        } else {
            self.reset_timer().await;
        }
        Ok(())
    }

    async fn shutdown(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        // Order: cancel the timer first so it cannot race in another
        // flush, then take the buffer in one shot, then run the
        // shutdown-mode flush which owns the entire disposition
        // (Delivered on success, DLQ + Recovered otherwise). The
        // queue consumer has already stopped pushing into us by the
        // time `shutdown` is called, so there is no re-entrant-push
        // race to defend against here.
        self.cancel_timer().await;
        let batch = std::mem::take(&mut *self.inner.batch.lock().await);
        self.inner.flush_events_at_shutdown(batch).await;
        Ok(())
    }
}

impl HttpOutput {
    /// Render + ship one event inline (used when `batch_size <= 1`).
    /// Drives the retry budget internally; resolves the ack based on
    /// final disposition.
    async fn consume_singleton(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let msg = match render_event(event) {
            Ok(m) => m,
            Err(e) => {
                let reason = format!("render failed: {}", e);
                crate::modules::route_event_to_dlq(
                    self.inner.error_log.as_ref(),
                    &self.inner.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
                return Ok(());
            }
        };
        let mut attempt = 0u32;
        let mut wait = self.inner.retry.initial_wait;
        loop {
            match self.inner.send_batch(std::slice::from_ref(&msg)).await {
                Ok(()) => {
                    self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.inner.retry.max_attempts {
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        crate::modules::route_event_to_dlq(
                            self.inner.error_log.as_ref(),
                            &self.inner.name,
                            event,
                            &reason,
                        )
                        .await;
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        ack.resolve_recovered();
                        return Ok(());
                    }
                    tracing::warn!(
                        "output '{}': send failed (attempt {}/{}): {} — retrying in {:?}",
                        self.inner.name,
                        attempt,
                        self.inner.retry.max_attempts,
                        e,
                        wait
                    );
                    tokio::time::sleep(wait).await;
                    wait = self.inner.retry.next_wait(wait);
                }
            }
        }
    }

    /// Drain the buffer and run one flush. Returns nothing — every
    /// dispatched event has its disposition resolved internally,
    /// either via successful delivery or via DLQ routing.
    async fn flush(&self) {
        let batch = {
            let mut buf = self.inner.batch.lock().await;
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };
        self.inner.flush_events(batch).await;
    }

    async fn cancel_timer(&self) {
        let mut handle = self.flush_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
    }

    async fn reset_timer(&self) {
        let mut handle = self.flush_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        let inner = Arc::clone(&self.inner);
        let timeout = self.inner.batch_timeout;
        *handle = Some(tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let batch = {
                let mut buf = inner.batch.lock().await;
                if buf.is_empty() {
                    return;
                }
                std::mem::take(&mut *buf)
            };
            inner.flush_events(batch).await;
        }));
    }
}

/// Render a single event into its HTTP body string. Called from the
/// consumer-side flush path on an owned `Event`, so no borrowed-view
/// arena setup is required.
fn render_event(event: &Event) -> Result<String> {
    Ok(String::from_utf8_lossy(&event.egress).into_owned())
}

impl Inner {
    /// Drain + ship one batch, resolving each handle to its final
    /// disposition. Infallible from the caller's POV: every entry
    /// has its disposition committed before this returns. Render
    /// failures route the offending event to DLQ on its own
    /// (resolve_recovered); the rest proceed to the HTTP send loop.
    /// Transport failures consume the per-flush retry budget; on
    /// exhaust the whole shippable subset is routed to DLQ.
    async fn flush_events(&self, batch: Vec<(Event, QueueAckHandle)>) {
        if batch.is_empty() {
            return;
        }
        let mut messages: Vec<String> = Vec::with_capacity(batch.len());
        let mut shippable: Vec<(Event, QueueAckHandle)> = Vec::with_capacity(batch.len());
        let mut render_failures: Vec<(Event, QueueAckHandle, anyhow::Error)> = Vec::new();
        for (ev, ack) in batch {
            match render_event(&ev) {
                Ok(m) => {
                    messages.push(m);
                    shippable.push((ev, ack));
                }
                Err(e) => render_failures.push((ev, ack, e)),
            }
        }
        for (ev, ack, err) in render_failures {
            let reason = format!("render failed during batch flush: {}", err);
            crate::modules::route_event_to_dlq(self.error_log.as_ref(), &self.name, &ev, &reason)
                .await;
            self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
        if messages.is_empty() {
            return;
        }
        let count = shippable.len() as u64;
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let final_err = loop {
            match self.send_batch(&messages).await {
                Ok(()) => {
                    self.metrics
                        .events_written
                        .fetch_add(count, Ordering::Relaxed);
                    for (_, ack) in shippable {
                        ack.resolve_delivered();
                    }
                    return;
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.retry.max_attempts {
                        break e;
                    }
                    tracing::warn!(
                        "output '{}': flush attempt {}/{} failed: {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    tokio::time::sleep(wait).await;
                    wait = self.retry.next_wait(wait);
                }
            }
        };
        // Retry exhausted: route every shippable event to DLQ and
        // resolve Recovered. The batch is gone from the buffer at
        // this point — the disk-queue cursor will not advance until
        // every handle resolves, so a daemon crash here replays the
        // batch on restart (the ack-handle invariant).
        let reason = format!("flush failed after {} attempts: {}", attempt, final_err);
        for (ev, ack) in shippable {
            crate::modules::route_event_to_dlq(self.error_log.as_ref(), &self.name, &ev, &reason)
                .await;
            self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
    }

    /// Shutdown single-attempt flush; never uses the steady-state retry
    /// budget. Render failures route per-event to DLQ as before; the
    /// shippable subset gets one `send_batch` call wrapped in
    /// `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT`. The `shippable` vector is held
    /// in this frame across the `timeout()` boundary so an `Elapsed`
    /// outcome does NOT drop it — otherwise the inner handles would
    /// fire `QueueAckHandle::Drop` and be counted as silent loss.
    async fn flush_events_at_shutdown(&self, batch: Vec<(Event, QueueAckHandle)>) {
        if batch.is_empty() {
            return;
        }
        let mut messages: Vec<String> = Vec::with_capacity(batch.len());
        let mut shippable: Vec<(Event, QueueAckHandle)> = Vec::with_capacity(batch.len());
        let mut render_failures: Vec<(Event, QueueAckHandle, anyhow::Error)> = Vec::new();
        for (ev, ack) in batch {
            match render_event(&ev) {
                Ok(m) => {
                    messages.push(m);
                    shippable.push((ev, ack));
                }
                Err(e) => render_failures.push((ev, ack, e)),
            }
        }
        for (ev, ack, err) in render_failures {
            let reason = format!("render failed during shutdown flush: {}", err);
            crate::modules::route_event_to_dlq(self.error_log.as_ref(), &self.name, &ev, &reason)
                .await;
            self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
        if messages.is_empty() {
            return;
        }
        let count = shippable.len() as u64;
        let send_outcome = tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            self.send_batch(&messages),
        )
        .await;
        match send_outcome {
            Ok(Ok(())) => {
                self.metrics
                    .events_written
                    .fetch_add(count, Ordering::Relaxed);
                for (_, ack) in shippable {
                    ack.resolve_delivered();
                }
            }
            Ok(Err(send_err)) => {
                let err = anyhow::anyhow!("transport error: {}", send_err);
                crate::modules::route_shutdown_batch_to_dlq(
                    self.error_log.as_ref(),
                    &self.metrics,
                    &self.name,
                    shippable,
                    &err,
                )
                .await;
            }
            Err(_elapsed) => {
                let err = anyhow::anyhow!(
                    "deadline exceeded after {:?} during shutdown flush",
                    crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
                );
                crate::modules::route_shutdown_batch_to_dlq(
                    self.error_log.as_ref(),
                    &self.metrics,
                    &self.name,
                    shippable,
                    &err,
                )
                .await;
            }
        }
    }

    /// Ship `messages` to one of the configured peers, rotating to the
    /// next peer in round-robin order. A peer that fails the request
    /// is cooled down for `PEER_COOLDOWN` and skipped on subsequent
    /// flushes. When every peer is currently cooled the rotation falls
    /// back to the cursor start — the per-flush retry loop on `Inner`
    /// then handles longer-term re-delivery without dropping the batch.
    async fn send_batch(&self, messages: &[String]) -> Result<()> {
        let body_str = messages.join("\n");

        let body: Vec<u8> = if self.compress {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder
                .write_all(body_str.as_bytes())
                .context("http output: gzip compression failed")?;
            encoder
                .finish()
                .context("http output: gzip finalization failed")?
        } else {
            body_str.into_bytes()
        };

        let n = self.peers.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        let now = Instant::now();

        // Pick the next non-cooled peer in rotation; if every peer is
        // cooled, fall back to the rotation start (the queue layer's
        // retry then handles the persistent-failure case).
        let mut idx = start;
        for offset in 0..n {
            let candidate = (start + offset) % n;
            let guard = self.peer_state[candidate].cooldown_until.lock().await;
            if guard.is_none_or(|until| until <= now) {
                idx = candidate;
                break;
            }
        }

        let peer = &self.peers[idx];
        match send_once(peer, self, &body).await {
            Ok(()) => {
                *self.peer_state[idx].cooldown_until.lock().await = None;
                Ok(())
            }
            Err(e) => {
                // Cool down relative to the *failure time*, not request
                // start. With a 30s request timeout and a 5s cooldown,
                // measuring from `now` could record an already-expired
                // cooldown and immediately reselect the same bad peer.
                *self.peer_state[idx].cooldown_until.lock().await =
                    Some(Instant::now() + PEER_COOLDOWN);
                Err(e)
            }
        }
    }
}

async fn send_once(peer: &HttpPeer, inner: &Inner, body: &[u8]) -> Result<()> {
    let mut request = peer.client.request(inner.method.clone(), &peer.url);

    request = request.header("Content-Type", &inner.content_type);

    if inner.compress {
        request = request.header("Content-Encoding", "gzip");
    }

    for (key, value) in &inner.headers {
        request = request.header(key.as_str(), value.as_str());
    }

    let response = request
        .body(body.to_vec())
        .send()
        .await
        .with_context(|| format!("http output: request to {} failed", peer.url))?;

    let status = response.status();
    if !status.is_success() {
        // `error_snippet` byte-caps the body via `read_body_capped`,
        // then trims to 200 chars on the lossy decode for a readable
        // log line; if the peer advertises a Content-Encoding limpid
        // doesn't decode (gzip / brotli / deflate are all off in our
        // reqwest build), it substitutes a `<gzip-encoded body, N
        // bytes>` placeholder so the daemon log doesn't fill with
        // replacement-char soup.
        let snippet = error_snippet(response, ERROR_BODY_BYTE_CAP, 200).await;
        anyhow::bail!(
            "http output: {} returned {} — {}",
            peer.url,
            status,
            snippet
        );
    }

    Ok(())
}

impl Drop for HttpOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
        // Best-effort warn on leaked buffered events. Holding an Arc
        // here would block the warn under contention; try_lock is the
        // right behaviour for a Drop path. Each leftover handle's
        // own Drop impl fires `Dropped` back at the queue consumer
        // — the cursor will not advance for them.
        if let Ok(buf) = self.inner.batch.try_lock()
            && !buf.is_empty()
        {
            tracing::warn!(
                "http output: {} events in buffer lost on shutdown (will be re-delivered from queue)",
                buf.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::event::Event;
    use std::net::SocketAddr;

    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("http", props.to_vec())
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

    fn peer_block(url: &str) -> Property {
        Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("url", url)],
        }
    }

    fn ident_prop(key: &str, val: &str) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::Ident(vec![val.to_string()])),
            value_span: None,
        }
    }

    fn peers_block_with(peers: Vec<Property>) -> Property {
        Property::Block {
            key: "peers".into(),
            key_span: None,
            properties: peers,
        }
    }

    /// Single-attempt `retry { max_attempts 1 ... }` block so tests
    /// driving a failing peer don't churn through the default 5-attempt
    /// retry budget (which would also sleep on each backoff). Retry
    /// lives inside the output, so tests that used to assert "first
    /// attempt errors" need to pin the budget to 1 to preserve that
    /// semantic without surprise extra retries.
    fn fast_retry_block() -> Property {
        Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        }
    }

    #[test]
    fn requires_peer_or_peers_block() {
        let err = HttpOutput::from_properties(
            "o",
            &mp(&[]),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("'peer {") && err.to_string().contains("'peers {"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn peer_requires_url() {
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![],
        }];
        let err = HttpOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("url"), "unexpected: {err}");
    }

    #[test]
    fn accepts_single_peer_shorthand() {
        let props = vec![peer_block("http://x:8080/")];
        let output = HttpOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.inner.peers.len(), 1);
        assert_eq!(output.inner.peers[0].url, "http://x:8080/");
    }

    #[test]
    fn parses_multi_peer_block() {
        let props = vec![peers_block_with(vec![
            peer_block("http://a:8080/"),
            peer_block("http://b:8080/"),
            peer_block("http://c:8080/"),
        ])];
        let output = HttpOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.inner.peers.len(), 3);
        assert_eq!(output.inner.peers[2].url, "http://c:8080/");
    }

    #[test]
    fn rejects_tls_with_cert_but_no_key() {
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("url", "https://x:8443/"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("cert", "/c.pem")],
                },
            ],
        }];
        let err = HttpOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("cert and key"),
            "unexpected: {err}"
        );
    }

    fn event_with(msg: &str) -> Event {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bytes = bytes::Bytes::from(msg.to_string());
        let mut e = Event::new(bytes.clone(), addr);
        e.egress = bytes;
        e
    }

    /// Test shim mirroring the queue consumer's call into
    /// `Output::consume`. Synthesises a 0.7.x-style `Result<()>` for
    /// the test bodies that already speak that vocabulary:
    /// - `Ok(())` — Delivered, OR no disposition yet (event parked
    ///   in the in-memory batch buffer waiting for a future flush).
    /// - `Err(...)` — Recovered (= DLQ-routed; retry exhausted or
    ///   render failure).
    async fn consume(output: &HttpOutput, ev: &Event) -> Result<()> {
        let (ack, mut rx) = QueueAckHandle::for_test();
        let _ = output.consume(ev, ack).await;
        match rx.try_recv() {
            Ok((_, crate::queue::AckDisposition::Delivered)) => Ok(()),
            Ok((_, crate::queue::AckDisposition::Recovered)) => {
                Err(anyhow::anyhow!("recovered to DLQ"))
            }
            Ok((_, crate::queue::AckDisposition::Dropped)) => Err(anyhow::anyhow!("dropped")),
            // No disposition yet — event is parked in the buffer.
            // Treated as Ok for test purposes; tests that need to
            // observe the eventual flush dispose separately.
            Err(_) => Ok(()),
        }
    }

    async fn run_echo_collector() -> (
        SocketAddr,
        Arc<Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
        };
        #[derive(Clone)]
        struct AppState {
            received: Arc<Mutex<Vec<String>>>,
        }
        async fn handle(
            State(state): State<AppState>,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            state
                .received
                .lock()
                .await
                .push(String::from_utf8_lossy(&body).into_owned());
            (StatusCode::OK, "")
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route("/", post(handle)).with_state(AppState {
            received: Arc::clone(&received),
        });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, received, handle)
    }

    #[tokio::test]
    async fn round_trip_single_peer() {
        let (addr, received, server) = run_echo_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[peer_block(&url), prop_int("batch_size", 1)]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with("hello-single")).await.unwrap();
        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        let got = received.lock().await.clone();
        assert_eq!(got, vec!["hello-single".to_string()]);
    }

    #[tokio::test]
    async fn round_robin_distributes_across_peers() {
        // Three echo collectors; send 9 events; expect 3 per peer.
        let (a, r_a, s_a) = run_echo_collector().await;
        let (b, r_b, s_b) = run_echo_collector().await;
        let (c, r_c, s_c) = run_echo_collector().await;
        let props = vec![
            peers_block_with(vec![
                peer_block(&format!("http://{}/", a)),
                peer_block(&format!("http://{}/", b)),
                peer_block(&format!("http://{}/", c)),
            ]),
            prop_int("batch_size", 1),
        ];
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        for i in 0..9 {
            consume(&output, &event_with(&format!("rr-{}", i)))
                .await
                .unwrap();
        }
        for _ in 0..50 {
            let na = r_a.lock().await.len();
            let nb = r_b.lock().await.len();
            let nc = r_c.lock().await.len();
            if na + nb + nc == 9 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        s_a.abort();
        s_b.abort();
        s_c.abort();
        assert_eq!(r_a.lock().await.len(), 3);
        assert_eq!(r_b.lock().await.len(), 3);
        assert_eq!(r_c.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn rotates_to_healthy_peer_when_first_fails() {
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
        let s_a = tokio::spawn(async move {
            let _ = axum::serve(l_a, Router::new().route("/", post(fail))).await;
        });
        let s_b = tokio::spawn(async move {
            let _ = axum::serve(l_b, Router::new().route("/", post(ok))).await;
        });
        let props = vec![
            peers_block_with(vec![
                peer_block(&format!("http://{}/", a)),
                peer_block(&format!("http://{}/", b)),
            ]),
            prop_int("batch_size", 1),
        ];
        let mut props = props;
        props.push(fast_retry_block());
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        // First send goes to A (cursor 0), fails. With max_attempts=1
        // the failure routes to Recovered (DLQ). A is cooled down.
        let first = consume(&output, &event_with("rr-fail")).await;
        assert!(
            first.is_err(),
            "first attempt should fail (peer A is 500): {:?}",
            first
        );
        // Second event goes to peer B (next in rotation, A cooled).
        consume(&output, &event_with("rr-ok")).await.unwrap();
        s_a.abort();
        s_b.abort();
    }

    #[test]
    fn invalid_method_rejected_at_config_load() {
        // Misconfigured methods should fail fast at parse time, not at
        // the first send. The error message names the offending value
        // and lists the expected forms so the operator can correct
        // their config.
        let props = vec![
            peer_block("http://example.com/"),
            ident_prop("method", "CARRIER PIGEON"),
        ];
        let err = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("invalid method must reject");
        let msg = err.to_string();
        assert!(msg.contains("invalid http method"), "got: {msg}");
        assert!(msg.contains("CARRIER PIGEON"), "got: {msg}");
    }

    #[test]
    fn verify_false_with_client_identity_parses_ok() {
        // Even with `verify false`, a tls block carrying client
        // cert/key must still parse successfully so mTLS keeps working
        // for ops who intentionally disable server-cert validation
        // (typically dev environments with self-signed servers behind
        // a corporate CA they don't want to bundle). Generate a real
        // ephemeral cert/key with rcgen so the test exercises
        // reqwest's identity loader, not just the config plumbing.
        use rcgen::{CertificateParams, KeyPair};
        use tempfile::TempDir;

        let key_pair = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("c.pem");
        let key_path = dir.path().join("k.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

        let props = vec![
            Property::Block {
                key: "peer".into(),
                key_span: None,
                properties: vec![
                    prop_str("url", "https://example.com/"),
                    Property::Block {
                        key: "tls".into(),
                        key_span: None,
                        properties: vec![
                            prop_str("cert", cert_path.to_str().unwrap()),
                            prop_str("key", key_path.to_str().unwrap()),
                        ],
                    },
                ],
            },
            ident_prop("verify", "false"),
        ];
        // `verify false` must NOT discard the client identity. Old
        // behaviour: tls block silently ignored; reqwest builds a
        // plain client without the identity → mTLS broken at runtime.
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    #[tokio::test]
    async fn honors_configured_method() {
        // Method other than POST/PUT used to silently degrade to POST.
        // Now PATCH should reach the server as PATCH.
        use axum::{
            Router, extract::State, http::StatusCode, response::IntoResponse, routing::any,
        };

        #[derive(Clone)]
        struct AppState {
            received: Arc<Mutex<Vec<String>>>,
        }

        async fn handle(
            State(state): State<AppState>,
            method: axum::http::Method,
            _body: axum::body::Bytes,
        ) -> impl IntoResponse {
            state.received.lock().await.push(method.to_string());
            (StatusCode::OK, "")
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route("/", any(handle)).with_state(AppState {
            received: Arc::clone(&received),
        });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}/", addr);
        let props = vec![
            peer_block(&url),
            prop_int("batch_size", 1),
            ident_prop("method", "PATCH"),
        ];
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with("hello")).await.unwrap();
        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        let got = received.lock().await.clone();
        assert_eq!(got, vec!["PATCH".to_string()]);
    }

    #[tokio::test]
    async fn caps_error_response_body() {
        // A peer returning a huge error body used to be buffered in
        // full via `response.text().await` before being trimmed. The
        // cap now stops reading at ERROR_BODY_BYTE_CAP bytes, so the
        // diagnostic line stays bounded regardless of peer behaviour.
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        // 256 KiB — way over the 4 KiB cap so any unbounded read
        // would manifest as a much larger error message.
        const BIG: usize = 256 * 1024;
        async fn handle() -> impl IntoResponse {
            let body = "X".repeat(BIG);
            (StatusCode::INTERNAL_SERVER_ERROR, body)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(handle));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}/", addr);
        let props = vec![peer_block(&url), prop_int("batch_size", 1)];
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        // `consume` resolves the ack and swallows the underlying
        // transport error inside its DLQ-routing path.
        // Hit `Inner::send_batch` directly so we can still assert on
        // the snippet-cap behaviour at the transport layer.
        let err = output
            .inner
            .send_batch(&["hello".to_string()])
            .await
            .expect_err("500 must surface as Err at the transport layer");
        server.abort();
        let msg = err.to_string();
        assert!(
            msg.len() < 1024,
            "error message must stay bounded, got {} bytes: {}",
            msg.len(),
            &msg[..msg.len().min(80)]
        );
        assert!(msg.contains("returned 500"), "got: {msg}");
    }

    #[tokio::test]
    async fn error_body_with_unsupported_content_encoding_renders_placeholder() {
        // limpid's reqwest is built without gzip/brotli/deflate
        // decompression, so a peer that advertises Content-Encoding:
        // gzip on its error body returns still-compressed bytes from
        // `Response::chunk()`. Running `from_utf8_lossy` over that
        // produces a daemon log line full of � replacement chars.
        // `error_snippet` substitutes a placeholder noting the
        // encoding + byte count so the log stays readable.
        use axum::{
            Router,
            http::{HeaderValue, StatusCode, header::CONTENT_ENCODING},
            response::IntoResponse,
            routing::post,
        };

        async fn handle() -> impl IntoResponse {
            // Bytes that look superficially binary; the exact contents
            // don't matter, we're asserting on the rendering path.
            let body: Vec<u8> = (0u8..=200).collect();
            let mut resp = (StatusCode::BAD_GATEWAY, body).into_response();
            resp.headers_mut()
                .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            resp
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(handle));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}/", addr);
        let props = vec![peer_block(&url), prop_int("batch_size", 1)];
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let err = output
            .inner
            .send_batch(&["hello".to_string()])
            .await
            .expect_err("502 must surface as Err at the transport layer");
        server.abort();
        let msg = err.to_string();
        assert!(
            msg.contains("gzip-encoded body"),
            "must show placeholder, got: {msg}"
        );
        assert!(msg.contains("returned 502"), "got: {msg}");
    }

    #[tokio::test]
    async fn cooldown_measured_from_failure_time_not_request_start() {
        // With a slow-responding peer, the cooldown timestamp must be
        // captured AFTER the request returns. The old code captured a
        // `now` before send and added PEER_COOLDOWN to it; if the
        // request took longer than the cooldown, the cooldown would
        // be already expired by the time it was written, so the next
        // rotation immediately picks the same broken peer.
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        async fn slow_fail() -> impl IntoResponse {
            tokio::time::sleep(Duration::from_millis(300)).await;
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(slow_fail));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}/", addr);
        let props = vec![
            peer_block(&url),
            prop_int("batch_size", 1),
            fast_retry_block(),
        ];
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let pre_call = Instant::now();
        let _ = consume(&output, &event_with("hello")).await;
        let cooldown_until = output.inner.peer_state[0]
            .cooldown_until
            .lock()
            .await
            .expect("cooldown must be set after failure");
        server.abort();

        // The cooldown was set at *failure time*, so it must be later
        // than `pre_call + PEER_COOLDOWN` by at least the artificial
        // 300 ms request delay (minus a small tolerance for scheduling
        // jitter). The old behaviour set it at exactly `pre_call +
        // PEER_COOLDOWN`.
        let expected_floor = pre_call + PEER_COOLDOWN + Duration::from_millis(200);
        assert!(
            cooldown_until > expected_floor,
            "cooldown_until ({:?}) must exceed pre_call + PEER_COOLDOWN + delay ({:?})",
            cooldown_until,
            expected_floor
        );
    }

    #[tokio::test]
    async fn singleton_batch_size_ships_inline_without_buffering() {
        // When `batch_size <= 1`, `consume` short-circuits through the
        // singleton path that renders + ships in one shot (no buffer,
        // no timer). Mirrors the prior `write_owned` bypass behaviour
        // but for the unified queue path.
        let mut props = vec![peer_block("http://127.0.0.1:1/")];
        props.push(prop_int("batch_size", 1));
        props.push(fast_retry_block());
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let err = consume(&output, &event_with("singleton"))
            .await
            .expect_err("send must fail against unreachable peer");
        // With the new lifecycle the underlying transport message
        // is consumed by the DLQ-routing path, so we only check that
        // the disposition surfaced as Recovered (= test-shim Err).
        assert!(err.to_string().contains("recovered"), "got: {err}");
        let batch_len = output.inner.batch.lock().await.len();
        assert_eq!(
            batch_len, 0,
            "singleton path must not land the event in the batch"
        );
        let timer_armed = output.flush_handle.lock().await.is_some();
        assert!(!timer_armed, "singleton path must not arm the flush timer");
    }

    #[tokio::test]
    async fn flush_failure_routes_batch_to_dlq_and_resolves_recovered() {
        // When a batch flush exhausts the per-flush retry budget
        // against a permanently failing peer, every event in the
        // batch routes to the DLQ and its ack handle resolves as
        // `Recovered`. The buffer must be empty afterwards (no more
        // "restore on failure" — the queue's cursor advances when the
        // ack channel fires).
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
        async fn always_fail(_: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(always_fail));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::TempDir::new().unwrap();
        let dlq_path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(dlq_path.clone()));

        let url = format!("http://{}/", addr);
        let props = vec![
            peer_block(&url),
            prop_int("batch_size", 2),
            prop_str("batch_timeout", "10s"),
            fast_retry_block(),
        ];
        let ctx = crate::modules::BuildContext {
            funcs: Arc::new(crate::functions::FunctionRegistry::new()),
            error_log: Some(Arc::clone(&writer)),
        };
        let output = HttpOutput::from_properties("test", &mp(&props), &ctx).unwrap();

        // First consume parks the event with no flush. Watch the ack
        // channel: it must NOT resolve until the flush actually runs.
        let (ack1, mut rx1) = QueueAckHandle::for_test();
        output.consume(&event_with("e1"), ack1).await.unwrap();
        assert!(
            rx1.try_recv().is_err(),
            "ack must not resolve until flush runs"
        );

        // Second consume hits batch_size, triggers flush, which
        // exhausts the budget and DLQ-routes both events.
        let (ack2, mut rx2) = QueueAckHandle::for_test();
        output.consume(&event_with("e2"), ack2).await.unwrap();
        server.abort();

        assert!(matches!(
            rx1.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert!(matches!(
            rx2.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert_eq!(
            output.inner.batch.lock().await.len(),
            0,
            "buffer must be empty after flush — handles already resolved"
        );

        // Both events landed in the DLQ JSONL.
        let body = tokio::fs::read_to_string(&dlq_path).await.unwrap();
        let n_lines = body.lines().count();
        assert_eq!(n_lines, 2, "expected one DLQ record per buffered event");
    }

    #[tokio::test]
    async fn batched_consume_holds_ack_until_flush_succeeds() {
        // Key invariant: a batched output must NOT resolve the ack
        // handle when `consume` returns. The handle resolves on the
        // eventual flush — keeping the queue cursor parked at the
        // un-flushed event so a crash mid-batch replays it.
        let (addr, received, server) = run_echo_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 2),
                prop_str("batch_timeout", "10s"),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let (ack, mut rx) = QueueAckHandle::for_test();
        output.consume(&event_with("e1"), ack).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "ack channel must be empty while event is buffered"
        );
        // Fill the batch → flush fires → ack resolves Delivered.
        let (ack2, mut rx2) = QueueAckHandle::for_test();
        output.consume(&event_with("e2"), ack2).await.unwrap();
        assert!(matches!(
            rx.recv().await,
            Some((_, crate::queue::AckDisposition::Delivered))
        ));
        assert!(matches!(
            rx2.recv().await,
            Some((_, crate::queue::AckDisposition::Delivered))
        ));
        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        assert_eq!(received.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_batch_buffer() {
        // Regression: under batch_size > 1 the queue-side `write()`
        // returns Ok once the event is in the buffer, so the memory
        // queue considers it delivered. If the daemon shuts down
        // before the batch fills or the timer fires, Drop alone
        // aborts the timer and leaks the buffered events. The fix
        // gives Output a `shutdown()` method that the queue consumer
        // calls once the consume loop exits, and HttpOutput
        // overrides it to cancel the timer and run one final flush.
        let (addr, received, server) = run_echo_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block(&url),
                // Large batch + long timer so the only thing that
                // can drain this buffer is the explicit shutdown.
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        consume(&output, &event_with("ev1")).await.unwrap();
        consume(&output, &event_with("ev2")).await.unwrap();
        assert_eq!(
            output.inner.batch.lock().await.len(),
            2,
            "events must sit in the buffer (batch_size and timer both far away)"
        );
        // Server has nothing yet — neither write triggered a flush.
        assert!(received.lock().await.is_empty());

        output.shutdown(None).await.unwrap();

        // Buffer drained.
        assert_eq!(
            output.inner.batch.lock().await.len(),
            0,
            "shutdown() must drain the buffer"
        );

        // And the request actually went out.
        for _ in 0..50 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        let got = received.lock().await.clone();
        assert_eq!(got.len(), 1, "shutdown flush must POST exactly once");
        let body = &got[0];
        assert!(
            body.contains("ev1") && body.contains("ev2"),
            "shutdown POST must carry the buffered events; got: {body}"
        );
    }

    // -----------------------------------------------------------------------
    // Shutdown-flush recovery: shutdown-time buffer-loss recovery via `error_log`.
    // -----------------------------------------------------------------------

    async fn run_failing_collector() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
        async fn fail(_: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(fail));
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, handle)
    }

    /// shutdown flush fails + error_log set → buffered rendered bodies
    /// land in the DLQ as `(output http shutdown)` records so a
    /// `jq | inject` recipe can replay them.
    #[tokio::test]
    async fn shutdown_failure_with_error_log_persists_buffer() {
        let (addr, server) = run_failing_collector().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            funcs: Arc::new(crate::functions::FunctionRegistry::new()),
            error_log: Some(Arc::clone(&writer)),
        };
        let output = HttpOutput::from_properties(
            "myout",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                fast_retry_block(),
            ]),
            &ctx,
        )
        .unwrap();

        // Drop two events into the buffer (no flush triggered).
        consume(&output, &event_with("ev1")).await.unwrap();
        consume(&output, &event_with("ev2")).await.unwrap();
        assert_eq!(output.inner.batch.lock().await.len(), 2);

        // shutdown -> flush() → server returns 500 → retry exhausts →
        // each entry routes to DLQ with `Recovered`. Buffer empty,
        // shutdown returns Ok.
        output.shutdown(Some(&writer)).await.unwrap();
        server.abort();

        assert_eq!(
            output.inner.batch.lock().await.len(),
            0,
            "shutdown recovery must drain the buffer"
        );

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected one DLQ record per buffered body");
        let recovered: String = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]["ingress"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            recovered.contains("ev1") && recovered.contains("ev2"),
            "got: {recovered}"
        );
    }

    /// shutdown flush fails + error_log unset → shutdown is
    /// infallible from the caller's POV. The output drains the
    /// buffer via DLQ-or-warn paths and returns Ok; the per-entry
    /// handles still resolve so the consumer can wrap up.
    #[tokio::test]
    async fn shutdown_failure_without_error_log_returns_ok() {
        let (addr, server) = run_failing_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                fast_retry_block(),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with("ev1")).await.unwrap();

        output.shutdown(None).await.expect("shutdown is infallible");
        server.abort();
        // Buffer drained: flush_events routes to log-without-DLQ
        // (because error_log is None) and resolves Recovered.
        assert_eq!(output.inner.batch.lock().await.len(), 0);
    }

    /// shutdown flush succeeds + error_log set → success path is
    /// completely unchanged from the prior retain-on-failure path.
    /// The DLQ writer must not be
    /// touched (regression check: a stray write would change the
    /// operator's audit trail).
    #[tokio::test]
    async fn shutdown_success_does_not_touch_error_log() {
        let (addr, received, server) = run_echo_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with("ev1")).await.unwrap();

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
        assert_eq!(received.lock().await.len(), 1);
        assert!(
            !path.exists(),
            "error_log file must not be touched on a clean shutdown"
        );
    }

    /// error_log itself fails during shutdown recovery → warn-and-drop,
    /// no recursion. Use a path under a non-existent directory so the
    /// `OpenOptions::create` call inside `ErrorLogWriter::write` fails
    /// on every record. The shutdown override must still complete
    /// successfully (we already accepted the loss) instead of looping
    /// or panicking.
    #[tokio::test]
    async fn shutdown_recovery_writer_failure_does_not_recurse() {
        let (addr, server) = run_failing_collector().await;
        let url = format!("http://{}/", addr);
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                fast_retry_block(),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with("ev1")).await.unwrap();
        consume(&output, &event_with("ev2")).await.unwrap();

        // Parent dir does not exist → every `write_shutdown_payload`
        // call returns Err. The helper must warn and continue rather
        // than abort or loop.
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(
            std::path::PathBuf::from("/nonexistent/limpid-test-dir/errored.jsonl"),
        ));

        output.shutdown(Some(&writer)).await.unwrap();
        server.abort();
        // Buffer is drained either way (we took() before attempting
        // the DLQ write); the contract is that we don't crash.
        assert_eq!(output.inner.batch.lock().await.len(), 0);
    }

    /// Constructor-time error_log injection (replaces the prior
    /// `attach_error_log(&self, ...)` setter). The runtime always
    /// goes through `from_properties` with a `BuildContext` carrying
    /// the `error_log`; this test pins that the writer ends up on the
    /// Inner field so subsequent flush paths (render-failure routing,
    /// shutdown recovery) can reach it without any post-construction
    /// wiring.
    #[tokio::test]
    async fn constructor_injects_error_log_into_inner() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path));

        let ctx = crate::modules::BuildContext {
            funcs: Arc::new(crate::functions::FunctionRegistry::new()),
            error_log: Some(Arc::clone(&writer)),
        };
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[peer_block("http://127.0.0.1:1/"), prop_int("batch_size", 8)]),
            &ctx,
        )
        .unwrap();
        // The Inner's `error_log` field must point at the same writer
        // the runtime would have handed to us — no Mutex, no None
        // window between construction and consumer spawn.
        let stored = output
            .inner
            .error_log
            .as_ref()
            .expect("error_log must be set");
        assert!(
            Arc::ptr_eq(stored, &writer),
            "constructor must store the exact Arc passed in"
        );
    }
}

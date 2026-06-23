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
//! when every peer is cooled (single-peer retry path) — the queue
//! layer's per-event retry then handles re-delivery on persistent
//! failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
    crate::queue::QUEUE_PROPERTY_SPEC,
];

struct HttpPayload {
    msg: String,
}

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

/// Shared state between write() and the flush timer task.
struct Inner {
    peers: Vec<HttpPeer>,
    peer_state: Vec<PeerState>,
    cursor: AtomicUsize,
    method: reqwest::Method,
    content_type: String,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    compress: bool,
    batch: Mutex<Vec<String>>,
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

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
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
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
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
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        // The HTTP body is text — building the `String` here (a small,
        // unavoidable allocation for the request body) keeps the
        // serialisation cost on the pipeline thread instead of the
        // sink consumer thread, but does not duplicate any work.
        let msg = String::from_utf8_lossy(&event.egress).into_owned();
        Ok(RenderedPayload::new(HttpPayload { msg }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: HttpPayload = payload.downcast()?;
        let msg = payload.msg;

        if self.batch_size <= 1 {
            self.inner.send_batch(&[msg]).await?;
            self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Batching mode
        let should_flush = {
            let mut buf = self.inner.batch.lock().await;
            buf.push(msg);
            buf.len() >= self.batch_size
        };

        if should_flush {
            self.cancel_timer().await;
            self.flush().await?;
        } else {
            self.reset_timer().await;
        }

        Ok(())
    }

    /// Owned-event path (disk-queue replay, control-socket inject). The
    /// queue consumer needs a per-event ship verdict; routing Owned
    /// events through the batched buffer would silently merge them into
    /// a later flush and the caller would lose the verdict. Ship inline
    /// here, bypassing the batch (same disposition as the
    /// `batch_size <= 1` short-circuit in `write`).
    async fn write_owned(&self, event: &Event) -> Result<()> {
        crate::modules::ship_owned_inline(self, event, &self.metrics, |payload| async move {
            let payload: HttpPayload = payload.downcast()?;
            // Plain HTTP has no partial-success concept; either the
            // POST succeeds end-to-end or it errors. Always Ok(0).
            self.inner.send_batch(&[payload.msg]).await.map(|()| 0)
        })
        .await
    }
}

impl HttpOutput {
    async fn flush(&self) -> Result<()> {
        let batch = {
            let mut buf = self.inner.batch.lock().await;
            if buf.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *buf)
        };

        let count = batch.len();
        match self.inner.send_batch(&batch).await {
            Ok(()) => {
                self.metrics
                    .events_written
                    .fetch_add(count as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                // Put events back for retry
                let mut buf = self.inner.batch.lock().await;
                let new_events = std::mem::take(&mut *buf);
                *buf = batch;
                buf.extend(new_events);
                Err(e)
            }
        }
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
        let metrics = Arc::clone(&self.metrics);
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

            let count = batch.len();
            if let Err(e) = inner.send_batch(&batch).await {
                tracing::warn!(
                    "http output: timer flush failed: {} — {} events returned to buffer",
                    e,
                    count
                );
                let mut buf = inner.batch.lock().await;
                let new_events = std::mem::take(&mut *buf);
                *buf = batch;
                buf.extend(new_events);
            } else {
                metrics
                    .events_written
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
        }));
    }
}

impl Inner {
    /// Ship `messages` to one of the configured peers, rotating to the
    /// next peer in round-robin order. A peer that fails the request
    /// is cooled down for `PEER_COOLDOWN` and skipped on subsequent
    /// flushes. When every peer is currently cooled the rotation falls
    /// back to the cursor start — the queue layer's per-event retry
    /// then handles longer-term re-delivery.
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
        anyhow::bail!("http output: {} returned {} — {}", peer.url, status, snippet);
    }

    Ok(())
}

impl Drop for HttpOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
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

    #[test]
    fn requires_peer_or_peers_block() {
        let err = HttpOutput::from_properties("o", &mp(&[])).err().unwrap();
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
        let err = HttpOutput::from_properties("o", &mp(&props)).err().unwrap();
        assert!(err.to_string().contains("url"), "unexpected: {err}");
    }

    #[test]
    fn accepts_single_peer_shorthand() {
        let props = vec![peer_block("http://x:8080/")];
        let output = HttpOutput::from_properties("o", &mp(&props)).unwrap();
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
        let output = HttpOutput::from_properties("o", &mp(&props)).unwrap();
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
        let err = HttpOutput::from_properties("o", &mp(&props)).err().unwrap();
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
        )
        .unwrap();
        output
            .write_owned(&event_with("hello-single"))
            .await
            .unwrap();
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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        for i in 0..9 {
            output
                .write_owned(&event_with(&format!("rr-{}", i)))
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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        // First send goes to A (cursor 0), fails, A cools down.
        // The queue layer would normally re-send; here we just send
        // again and expect B to take it (cursor advances to 1).
        let first = output.write_owned(&event_with("rr-fail")).await;
        assert!(first.is_err(), "first attempt should fail (peer A is 500)");
        // Second event goes to peer B (next in rotation, A cooled).
        output.write_owned(&event_with("rr-ok")).await.unwrap();
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
        let err = HttpOutput::from_properties("test", &mp(&props))
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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        assert_eq!(output.inner.peers.len(), 1);
    }

    #[tokio::test]
    async fn honors_configured_method() {
        // Method other than POST/PUT used to silently degrade to POST.
        // Now PATCH should reach the server as PATCH.
        use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::any};

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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        output.write_owned(&event_with("hello")).await.unwrap();
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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with("hello"))
            .await
            .expect_err("500 must surface as Err");
        server.abort();
        let msg = err.to_string();
        // Snippet caps at 200 chars; the full message is "http output:
        // <url> returned 500 Internal Server Error — XXXX…" which
        // tops out well under 1 KiB even with a 16 KiB peer payload.
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
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with("hello"))
            .await
            .expect_err("502 must surface as Err");
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
        let props = vec![peer_block(&url), prop_int("batch_size", 1)];
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        let pre_call = Instant::now();
        let _ = output.write_owned(&event_with("hello")).await;
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
    async fn write_owned_bypasses_batch_buffer() {
        // Owned events ship inline rather than landing in the batch
        // buffer, so the queue consumer's per-event Ok/Err contract is
        // honored. Verify the buffer stays empty and no flush timer is
        // armed even with batch_size > 1.
        let mut props = vec![peer_block("http://127.0.0.1:1/")];
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        let output = HttpOutput::from_properties("test", &mp(&props)).unwrap();
        let err = output
            .write_owned(&event_with("bypass"))
            .await
            .expect_err("send must fail against unreachable peer");
        assert!(err.to_string().contains("http"), "got: {err}");
        let batch_len = output.inner.batch.lock().await.len();
        assert_eq!(batch_len, 0, "Owned event must not land in the batch");
        let timer_armed = output.flush_handle.lock().await.is_some();
        assert!(!timer_armed, "Owned event must not arm the flush timer");
    }
}

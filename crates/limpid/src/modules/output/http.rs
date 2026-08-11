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
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::batched::{BatchSinkPolicy, BatchedSink, SendOutcome};
use crate::modules::output::http_util::{ERROR_BODY_BYTE_CAP, error_snippet};
use crate::modules::output::syslog_peers::{RotatingPeers, iter_peers_block};
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

/// Transport policy plugged into the shared [`BatchedSink`] skeleton:
/// render = verbatim (byte-preserving) `event.egress` bytes,
/// prepare = newline join + optional gzip, send = one attempt against
/// the next rotation candidate. Buffering, retry, and the shutdown
/// lifecycle live in `crate::modules::output::batched`. Binary payload
/// fidelity is pinned by `http_output_forwards_non_utf8_egress_verbatim` —
/// do NOT re-introduce a `from_utf8_lossy` here.
struct HttpSinkPolicy {
    peers: Vec<HttpPeer>,
    /// Round-robin cursor + per-peer failure cooldown; one candidate
    /// per send attempt. See `RotatingPeers` for the selection and
    /// cooldown contract.
    rotation: RotatingPeers,
    method: reqwest::Method,
    content_type: String,
    headers: Vec<(String, String)>,
    compress: bool,
}

pub struct HttpOutput {
    sink: BatchedSink<HttpSinkPolicy>,
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
        // A `tls { ... }` block on a plaintext (`http://` or
        // scheme-less) URL is almost always an operator error:
        // reqwest only engages the TLS layer when the URL scheme
        // is https, so the configured CA / client identity is
        // silently dropped and the daemon ships requests in clear
        // text. The earlier shape emitted a `tracing::warn!` at
        // build time and continued, which meant an operator who
        // added a `tls { ca ... }` line to lock down egress could
        // still be shipping plaintext bytes to the peer while
        // trusting the config had done what it said. Refuse at
        // parse time so the misconfiguration is visible instead of
        // hidden in the startup log. Mirrors the matching guard in
        // `output otlp_http` and `output otlp_grpc`.
        anyhow::bail!(
            "output '{}': http peer url '{}' uses a plaintext scheme but a tls {{ ... }} block was supplied — switch the url to https:// or drop the tls block",
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

    // Explicit ≥ TLS 1.2 floor, mirroring the rustls-side pin in
    // `crate::tls::TLS_PROTOCOL_VERSIONS` (see the rationale there).
    // No behaviour change with the rustls backend — it only implements
    // 1.2 / 1.3 — but the floor is now stated, not inherited.
    let mut client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .min_tls_version(reqwest::tls::Version::TLS_1_2);

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
        properties: &crate::dsl::module_props::ModuleProperties,
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

        // Read through `get_bool`, NOT `get_ident`: the parser emits
        // `ExprKind::BoolLit` for `verify false` (the pest `atom` rule
        // tries `bool_lit` before `ident_path`), so a `get_ident` read
        // never matches and silently falls back to the default. That
        // exact bug shipped until v0.7.9 — `verify false` was ignored
        // and verification stayed on.
        let verify = props::get_bool(properties, "verify").unwrap_or(true);

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

        let rotation = RotatingPeers::new(peers.len());

        let retry = RetryConfig::from_output_properties(properties)?;
        let metrics = OutputMetrics::register(&ctx.metrics, name)?;
        let policy = HttpSinkPolicy {
            peers,
            rotation,
            method,
            content_type,
            headers,
            compress,
        };
        // The shared skeleton spawns the flusher actor that owns
        // every send — both batched flushes and singleton
        // (`batch_size <= 1`). See `crate::modules::output::batched`
        // for the actor / shutdown lifecycle contract.
        let sink = BatchedSink::new(
            policy,
            name,
            batch_size,
            batch_timeout,
            retry,
            error_log,
            ctx.error_log_fallback,
            Arc::clone(&metrics),
            ctx.shutdown_signal.clone(),
        );

        Ok(Self { sink, metrics })
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
    /// sink buffer; the ack handle resolves at flush time (delivered
    /// or recovered) — not now. Returning `Ok(())` only signals that
    /// the output took ownership of the lifecycle. See
    /// `BatchedSink::consume` for the actor hand-off rationale.
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.sink.consume(event, ack).await
    }

    /// Drain-time per-event entry: buffer only; the post-loop
    /// `shutdown()` call drains it bounded. See
    /// `BatchedSink::consume_shutdown` for the shutdown contract.
    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.sink.consume_shutdown(event, ack).await
    }

    async fn shutdown(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        self.sink.shutdown().await
    }

    async fn shutdown_wedged(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        self.sink.shutdown_wedged().await
    }
}

#[async_trait::async_trait]
impl BatchSinkPolicy for HttpSinkPolicy {
    type Payload = Bytes;
    type Prepared = Vec<u8>;

    fn kind(&self) -> &'static str {
        "http output"
    }

    /// Render a single event into its per-event HTTP body payload
    /// bytes. Called from the flush path on an owned `Event`, so no
    /// borrowed-view arena setup is required. The egress bytes are
    /// forwarded verbatim — `Bytes` is refcounted, so this is a
    /// share, not a copy — because HTTP treats the request body as
    /// an opaque byte stream and the daemon must not silently rewrite
    /// customer payload content. `render` stays fallible so per-
    /// event DLQ routing remains available if a stricter render is
    /// ever introduced.
    fn render(&self, event: &Event) -> Result<Bytes> {
        Ok(event.egress.clone())
    }

    /// Join the batch bodies with a single `b'\n'` separator between
    /// records and optionally gzip. Byte-preserving: no UTF-8
    /// conversion at any point, so non-UTF-8 egress reaches the
    /// receiver verbatim (there is no U+FFFD substitution). Runs
    /// once per flush (the skeleton prepares before its retry loop),
    /// so compression is not re-done per attempt.
    fn prepare(&self, messages: Vec<Bytes>) -> Result<Vec<u8>> {
        // Preallocate: sum of payload lengths + one newline per
        // separator (there are `n - 1` separators for `n` payloads,
        // so `saturating_sub(1)` avoids overflow on the empty case).
        let total_len: usize =
            messages.iter().map(|m| m.len()).sum::<usize>() + messages.len().saturating_sub(1);
        let mut body = Vec::with_capacity(total_len);
        for (i, msg) in messages.iter().enumerate() {
            if i > 0 {
                body.push(b'\n');
            }
            body.extend_from_slice(msg);
        }
        if self.compress {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder
                .write_all(&body)
                .context("http output: gzip compression failed")?;
            encoder
                .finish()
                .context("http output: gzip finalization failed")
        } else {
            Ok(body)
        }
    }

    /// One send attempt against the next rotation candidate. A peer
    /// that fails is cooled down for `PEER_COOLDOWN` and skipped on
    /// subsequent sends; when every peer is cooled the rotation falls
    /// back to the cursor start — the skeleton's per-flush retry loop
    /// then handles longer-term re-delivery without dropping the
    /// batch. Plain HTTP has no partial-success concept, so success
    /// reports `rejected: 0` (= the whole batch is Delivered).
    async fn send(&self, body: &Vec<u8>) -> Result<SendOutcome> {
        let idx = self.rotation.select().await;
        let peer = &self.peers[idx];
        match send_once(peer, self, body).await {
            Ok(()) => {
                self.rotation.mark_success(idx).await;
                Ok(SendOutcome { rejected: 0 })
            }
            Err(e) => {
                self.rotation.mark_failure(idx).await;
                Err(e)
            }
        }
    }
}

async fn send_once(peer: &HttpPeer, policy: &HttpSinkPolicy, body: &[u8]) -> Result<()> {
    let mut request = peer.client.request(policy.method.clone(), &peer.url);

    request = request.header("Content-Type", &policy.content_type);

    if policy.compress {
        request = request.header("Content-Encoding", "gzip");
    }

    for (key, value) in &policy.headers {
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

// Drop lifecycle (cooperative signal → last-resort abort) lives on
// `BatchedSink`; `HttpOutput` needs no Drop of its own.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::event::Event;
    use crate::modules::output::syslog_peers::PEER_COOLDOWN;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::sync::Mutex;

    fn mp(props: &[Property]) -> crate::dsl::module_props::ModuleProperties {
        crate::dsl::module_props::ModuleProperties::from_parts("http", props.to_vec())
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
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
        assert_eq!(output.sink.inner.policy.peers[0].url, "http://x:8080/");
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
        assert_eq!(output.sink.inner.policy.peers.len(), 3);
        assert_eq!(output.sink.inner.policy.peers[2].url, "http://c:8080/");
    }

    #[test]
    fn rejects_tls_block_on_plaintext_url() {
        // `tls { ... }` on a plaintext (`http://` or scheme-less)
        // URL is almost always an operator error — reqwest only
        // engages the TLS layer when the scheme is https, so the
        // configured CA / client identity is silently dropped
        // and the daemon ships plaintext. Fail fast at parse
        // time. Mirrors the matching guard in `output otlp_http`
        // and `output otlp_grpc`.
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("url", "http://x:8080/"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("ca", "/etc/ca.pem")],
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
        let msg = err.to_string();
        assert!(
            msg.contains("plaintext") && msg.contains("https://"),
            "expected the error to name the mismatch and the fix; got: {msg}"
        );
    }

    #[test]
    fn accepts_tls_block_on_https_url() {
        // Regression guard for the plaintext-rejection check:
        // `https://` URLs must still accept an (empty) tls block
        // so no on-disk file is required for the round-trip.
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("url", "https://x:8443/"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![],
                },
            ],
        }];
        let output = HttpOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
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
    /// `Output::consume`. Since the actor refactor `consume`
    /// just buffers + notifies and returns immediately; the eventual
    /// ack disposition surfaces from the flusher actor. This shim
    /// captures the "ownership-accepted" return value only and
    /// reports `Ok(())` regardless of whether a disposition has
    /// landed yet — tests that need the disposition (delivered /
    /// recovered) should use `consume_and_wait_disposition` instead.
    async fn consume(output: &HttpOutput, ev: &Event) -> Result<()> {
        let (ack, mut rx) = QueueAckHandle::for_test();
        let _ = output.consume(ev, ack).await;
        match rx.try_recv() {
            Ok((_, crate::queue::AckDisposition::Delivered)) => Ok(()),
            Ok((_, crate::queue::AckDisposition::Recovered)) => {
                Err(anyhow::anyhow!("recovered to DLQ"))
            }
            Ok((_, crate::queue::AckDisposition::Dropped)) => Err(anyhow::anyhow!("dropped")),
            Err(_) => Ok(()),
        }
    }

    /// Push an event via `consume()` then await the flusher actor's
    /// resolution. Returns Delivered / Recovered / Dropped as
    /// surfaced by the ack handle. Bounded by `timeout` — tests
    /// that need to drive paused virtual time should advance time
    /// before calling this (or use the lower-level
    /// `consume_with_handle` helper to interleave their own time
    /// advances).
    async fn consume_and_wait_disposition(
        output: &HttpOutput,
        ev: &Event,
        timeout: Duration,
    ) -> Result<crate::queue::AckDisposition> {
        let (ack, mut rx) = QueueAckHandle::for_test();
        output.consume(ev, ack).await?;
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some((_, disp))) => Ok(disp),
            Ok(None) => Err(anyhow::anyhow!("ack channel closed unexpectedly")),
            Err(_) => Err(anyhow::anyhow!(
                "actor did not resolve ack within {:?}",
                timeout
            )),
        }
    }

    /// Lower-level helper: push an event, return the ack receiver
    /// for the test to await/poll directly. Used by paused-virtual-
    /// time tests that need to advance time between `consume()` and
    /// the disposition recv.
    #[allow(dead_code)]
    async fn consume_with_handle(
        output: &HttpOutput,
        ev: &Event,
    ) -> Result<
        tokio::sync::mpsc::UnboundedReceiver<(
            crate::queue::AckPosition,
            crate::queue::AckDisposition,
        )>,
    > {
        let (ack, rx) = QueueAckHandle::for_test();
        output.consume(ev, ack).await?;
        Ok(rx)
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
        // Drive each event end-to-end before pushing the next, so
        // batch_size=1 → one event per send → cursor advances
        // once per event (= the round-robin invariant). With the
        // actor refactor, a fast sequence of `consume()` calls can
        // otherwise batch multiple events into one flush; awaiting
        // the per-event disposition keeps the test deterministic
        // without needing larger sleeps.
        for i in 0..9 {
            let disp = consume_and_wait_disposition(
                &output,
                &event_with(&format!("rr-{}", i)),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert!(
                matches!(disp, crate::queue::AckDisposition::Delivered),
                "event {} must Deliver, got {:?}",
                i,
                disp
            );
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
        let disp1 =
            consume_and_wait_disposition(&output, &event_with("rr-fail"), Duration::from_secs(2))
                .await
                .unwrap();
        assert!(
            matches!(disp1, crate::queue::AckDisposition::Recovered),
            "first attempt should fail (peer A is 500) → Recovered, got {:?}",
            disp1
        );
        // Second event goes to peer B (next in rotation, A cooled).
        let disp2 =
            consume_and_wait_disposition(&output, &event_with("rr-ok"), Duration::from_secs(2))
                .await
                .unwrap();
        assert!(
            matches!(disp2, crate::queue::AckDisposition::Delivered),
            "second send should hit peer B → Delivered, got {:?}",
            disp2
        );
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

    /// Parse a DSL config source through the real parser and hand back
    /// the named output's `ModuleProperties`. Regression tests below use
    /// this instead of the hand-built `ident_prop` / `prop_str` AST
    /// helpers: hand-built ASTs encode `verify false` as
    /// `ExprKind::Ident(["false"])`, but the pest grammar produces
    /// `ExprKind::BoolLit(false)` — the two shapes diverged for years
    /// and the runtime only handled the shape that never occurs in a
    /// real config file (the v0.7.9 `get_bool` fix).
    fn parsed_output_props(src: &str, name: &str) -> crate::modules::ModuleProperties {
        let cfg = crate::dsl::parser::parse_config(src).expect("config should parse");
        let compiled = crate::pipeline::CompiledConfig::from_config(cfg).expect("compile");
        compiled.outputs[name].properties.clone()
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
        //
        // Goes through the real parser (not the `ident_prop` helper)
        // so `verify false` arrives as the `BoolLit` the grammar
        // actually produces.
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

        let src = format!(
            r#"
def output o {{
    type http
    peer {{
        url "https://example.com/"
        tls {{
            cert "{}"
            key "{}"
        }}
    }}
    verify false
}}
"#,
            cert_path.display(),
            key_path.display()
        );
        // `verify false` must NOT discard the client identity. Old
        // behaviour: tls block silently ignored; reqwest builds a
        // plain client without the identity → mTLS broken at runtime.
        let output = HttpOutput::from_properties(
            "o",
            &parsed_output_props(&src, "o"),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
    }

    #[tokio::test]
    async fn parser_spelled_verify_false_disables_certificate_verification() {
        // Regression for the v0.7.9 get_bool fix. `verify false` in a
        // real config file parses as `ExprKind::BoolLit(false)`; the
        // old `props::get_ident` read never matched that shape, so the
        // toggle was silently ignored and verification stayed on. Pin
        // the whole chain — DSL source → parser → from_properties →
        // reqwest client — by hitting an HTTPS server whose
        // self-signed certificate no root store would accept:
        //
        //   verify false  → send succeeds (danger_accept_invalid_certs)
        //   default       → send fails on the certificate
        //
        // The second half proves the first didn't pass because of an
        // over-permissive default client.
        use axum::{Router, http::StatusCode, routing::post};
        use rcgen::{CertificateParams, KeyPair};

        // axum-server's rustls needs a process-level CryptoProvider;
        // production installs it via the same helper.
        crate::tls::install_default_crypto_provider();

        let key_pair = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert.pem().into_bytes(),
            key_pair.serialize_pem().into_bytes(),
        )
        .await
        .unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", post(|| async { StatusCode::OK }));
        let server = tokio::spawn(async move {
            let _ = axum_server::from_tcp_rustls(listener, rustls_config)
                .serve(app.into_make_service())
                .await;
        });

        let src_insecure = format!(
            r#"
def output o {{
    type http
    peer {{ url "https://{addr}/" }}
    batch_size 1
    verify false
}}
"#
        );
        // The verify toggle lives on the HttpSinkPolicy, so exercise
        // the trait's `send` directly through it. Same code path that
        // the skeleton's flush loop takes at runtime — just without
        // the buffer/notify/actor scaffolding a per-event driver would
        // need for a single fixture request.
        use crate::modules::output::batched::BatchSinkPolicy;
        let insecure = HttpOutput::from_properties(
            "o",
            &parsed_output_props(&src_insecure, "o"),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let body = insecure
            .sink
            .inner
            .policy
            .prepare(vec![bytes::Bytes::from_static(b"hello")])
            .unwrap();
        insecure
            .sink
            .inner
            .policy
            .send(&body)
            .await
            .expect("verify false must accept the self-signed cert");

        let src_default = format!(
            r#"
def output o {{
    type http
    peer {{ url "https://{addr}/" }}
    batch_size 1
}}
"#
        );
        let strict = HttpOutput::from_properties(
            "o",
            &parsed_output_props(&src_default, "o"),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let body = strict
            .sink
            .inner
            .policy
            .prepare(vec![bytes::Bytes::from_static(b"hello")])
            .unwrap();
        let err = strict
            .sink
            .inner
            .policy
            .send(&body)
            .await
            .expect_err("default client must reject the self-signed cert");
        server.abort();
        let msg = format!("{:#}", err).to_ascii_lowercase();
        assert!(
            msg.contains("certificate") || msg.contains("unknownissuer"),
            "expected a certificate-verification failure, got: {msg}"
        );
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
        // Hit the policy's prepare + send directly so we can still
        // assert on the snippet-cap behaviour at the transport layer.
        let policy = &output.sink.inner.policy;
        let body = policy
            .prepare(vec![bytes::Bytes::from_static(b"hello")])
            .unwrap();
        let err = policy
            .send(&body)
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
        let policy = &output.sink.inner.policy;
        let body = policy
            .prepare(vec![bytes::Bytes::from_static(b"hello")])
            .unwrap();
        let err = policy
            .send(&body)
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
        let _ = consume_and_wait_disposition(&output, &event_with("hello"), Duration::from_secs(5))
            .await;
        let cooldown_until = output
            .sink
            .inner
            .policy
            .rotation
            .cooldown_until(0)
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
    async fn singleton_batch_size_ships_via_actor() {
        // `batch_size <= 1` no longer takes a separate inline path —
        // every event flows through the flusher actor, which keeps
        // the queue consumer's task off the transport `await` (the
        // shutdown lifecycle regression). Each consume pushes one event,
        // notifies the actor (threshold=1), the actor takes the
        // 1-element batch and ships. The peer is unreachable so the
        // disposition lands as Recovered via the DLQ path.
        let mut props = vec![peer_block("http://127.0.0.1:1/")];
        props.push(prop_int("batch_size", 1));
        props.push(fast_retry_block());
        let output = HttpOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let disp =
            consume_and_wait_disposition(&output, &event_with("singleton"), Duration::from_secs(2))
                .await
                .unwrap();
        assert!(
            matches!(disp, crate::queue::AckDisposition::Recovered),
            "send must fail against unreachable peer → Recovered, got {:?}",
            disp
        );
        // After the actor drains the 1-element batch, the buffer
        // must be empty again.
        let batch_len = output.sink.inner.batch.lock().await.len();
        assert_eq!(batch_len, 0, "actor must drain the singleton batch");
        let actor_spawned = output.sink.actor_handle.lock().await.is_some();
        assert!(
            actor_spawned,
            "the flusher actor is spawned for every batch_size (singleton included)"
        );
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
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
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
            output.sink.inner.batch.lock().await.len(),
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
        // Regression: under batch_size > 1 `consume()` parks the
        // (event, ack) pair in the output buffer; the queue cursor
        // cannot advance until that handle resolves. If shutdown
        // happens before the batch fills or before the actor's
        // batch_timeout wake, `shutdown()` must signal/join the
        // actor and final-drain the buffer so every parked handle
        // resolves (Delivered on flush success, Recovered on DLQ
        // route).
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
            output.sink.inner.batch.lock().await.len(),
            2,
            "events must sit in the buffer (batch_size and timer both far away)"
        );
        // Server has nothing yet — neither write triggered a flush.
        assert!(received.lock().await.is_empty());

        output.shutdown(None).await.unwrap();

        // Buffer drained.
        assert_eq!(
            output.sink.inner.batch.lock().await.len(),
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
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
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
        assert_eq!(output.sink.inner.batch.lock().await.len(), 2);

        // shutdown -> flush() → server returns 500 → retry exhausts →
        // each entry routes to DLQ with `Recovered`. Buffer empty,
        // shutdown returns Ok.
        output.shutdown(Some(&writer)).await.unwrap();
        server.abort();

        assert_eq!(
            output.sink.inner.batch.lock().await.len(),
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
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
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
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
    }

    /// Counts every POST the failing collector receives so the test
    /// can assert how many attempts the shutdown actually made.
    async fn run_counting_failing_collector() -> (
        SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
        };
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        #[derive(Clone)]
        struct AppState {
            count: Arc<AtomicUsize>,
        }
        async fn handle(State(s): State<AppState>) -> impl IntoResponse {
            s.count.fetch_add(1, AtomicOrdering::Relaxed);
            (StatusCode::INTERNAL_SERVER_ERROR, "")
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/", post(handle)).with_state(AppState {
            count: Arc::clone(&count),
        });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, count, server)
    }

    /// Pins the steady-state retry budget: `max_attempts = N` means
    /// exactly N send attempts (the first try + N-1 retries) before
    /// the batch routes to DLQ as Recovered. Guards the shared
    /// `BatchedSink` retry loop against off-by-one drift — the http
    /// and otlp outputs historically counted attempts with different
    /// (but equivalent) arithmetic, and this test keeps the unified
    /// loop honest.
    #[tokio::test]
    async fn retry_budget_makes_exactly_max_attempts_sends() {
        let (addr, post_count, server) = run_counting_failing_collector().await;
        let url = format!("http://{}/", addr);
        let retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 3),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[peer_block(&url), prop_int("batch_size", 1), retry]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        let disp =
            consume_and_wait_disposition(&output, &event_with("retry-pin"), Duration::from_secs(5))
                .await
                .unwrap();
        server.abort();
        assert!(
            matches!(disp, crate::queue::AckDisposition::Recovered),
            "budget exhaust must resolve Recovered, got {:?}",
            disp
        );
        let posts = post_count.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            posts, 3,
            "max_attempts=3 must make exactly 3 send attempts, got {}",
            posts
        );
        assert_eq!(
            output
                .metrics
                .retries
                .load(std::sync::atomic::Ordering::Relaxed),
            3,
            "the retries metric counts every failed attempt"
        );
    }

    /// Regression pin: the retry backoff sleep must NOT wake early when
    /// a fresh `consume()` fires a threshold flush notification. Before
    /// the fix the retry loop raced the sleep against `flush_notify`,
    /// which `consume()` also pings when the buffer crosses
    /// `batch_size`. Under continuous traffic that turned every retry
    /// backoff into an instant re-send — hammering a failing collector
    /// and ignoring `initial_wait`. Now the retry sleep only races
    /// against `shutdown_notify`, so a new event arriving mid-backoff
    /// enqueues silently until the sleep elapses on its own.
    #[tokio::test]
    async fn retry_backoff_survives_threshold_notify_race() {
        let (addr, post_count, server) = run_counting_failing_collector().await;
        let url = format!("http://{}/", addr);
        // 400 ms backoff between attempts. Long enough that we can
        // reliably observe "second attempt hasn't fired yet" through a
        // scheduling window, short enough that the whole test finishes
        // in about a second even without the wake bug.
        let slow_retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 2),
                prop_str("initial_wait", "400ms"),
                prop_str("max_wait", "400ms"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[peer_block(&url), prop_int("batch_size", 1), slow_retry]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        // Kick off the first send. `batch_size=1` means the actor
        // flushes immediately; the server 500s, and the retry loop
        // enters `sleep(400ms)`.
        output
            .consume(&event_with("first"), event_ack())
            .await
            .unwrap();

        // Wait long enough for the first attempt to reach the server
        // and for the retry loop to be sitting in its backoff. The
        // 400 ms window here has to comfortably cover a request round
        // trip plus a few scheduler hops.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let after_first = post_count.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after_first, 1,
            "expected exactly the initial attempt to have hit the server before the notify race, got {after_first}"
        );

        // Fire the threshold-flush notify while the retry loop is
        // asleep. Pre-fix this cut the backoff short and re-fired the
        // send within milliseconds; post-fix it must not.
        output
            .consume(&event_with("second"), event_ack())
            .await
            .unwrap();

        // Give the actor plenty of time to react to the notify but
        // still stay under the 400 ms backoff floor.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mid_backoff = post_count.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            mid_backoff, 1,
            "threshold-flush notify must not wake the retry backoff — expected 1 attempt still, got {mid_backoff}",
        );

        // Now wait past the 400 ms floor and let the retry actually
        // fire. The exact count depends on how the two batches are
        // scheduled, but the retry MUST have fired by now.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let post_backoff = post_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            post_backoff >= 2,
            "retry must have fired after the backoff elapsed — got only {post_backoff}",
        );

        server.abort();
    }

    /// Regression pin: `shutdown()` fired while a retry backoff sleep
    /// is in progress must short-circuit that sleep promptly, even
    /// under the lost-wake window. The previous implementation
    /// awaited a bare `shutdown_notify.notified()` inside the retry
    /// `tokio::select!`. If `shutdown()` ran between the pre-sleep
    /// `is_shutting_down.load()` and the `notified()` registration
    /// (a real race — those steps aren't atomic), the wake was
    /// lost and the retry loop slept the full `max_wait`. With
    /// `max_wait` defaulting to 60 s and the runtime's shutdown
    /// budget at 10 s, that stranded actor state past the join and
    /// forced a task abort — the same class of leak that PR #84
    /// closed for the steady-state path.
    ///
    /// This test can't atomically script the "shutdown fires
    /// between the outer load and the inner notified()" ordering.
    /// It instead pins the observable outcome: with a long backoff
    /// (`max_wait=5s`) and shutdown fired ~150 ms into the sleep,
    /// `shutdown()` must return well under `max_wait`. Pre-fix this
    /// occasionally slept the full 5 s; post-fix it is always
    /// prompt because `wait_until_shutdown()`'s load-notified-
    /// recheck pattern catches either ordering.
    #[tokio::test]
    async fn shutdown_short_circuits_the_retry_backoff() {
        let (addr, _post_count, server) = run_counting_failing_collector().await;
        let url = format!("http://{}/", addr);
        // A very long floor makes it unambiguous: any prompt
        // shutdown return is thanks to the wake, not the sleep
        // elapsing on its own.
        let slow_retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 5),
                prop_str("initial_wait", "5s"),
                prop_str("max_wait", "5s"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[peer_block(&url), prop_int("batch_size", 1), slow_retry]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        output
            .consume(&event_with("first"), event_ack())
            .await
            .unwrap();

        // Let the actor reach the retry sleep.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Fire shutdown. Measure the wall time to return; it must
        // be far less than the 5 s max_wait floor.
        let started = std::time::Instant::now();
        output.shutdown(None).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "shutdown must short-circuit the retry sleep — took {elapsed:?} against a 5s floor"
        );

        server.abort();
    }

    /// Cheap ack handle that discards its disposition — the sleep-race
    /// regression above doesn't need to inspect it.
    fn event_ack() -> crate::queue::QueueAckHandle {
        let (h, _) = crate::queue::QueueAckHandle::for_test();
        h
    }

    /// Regression for the 0.7.8 shutdown panic / silent loss: at shutdown
    /// the batched flush MUST NOT consume the steady-state retry budget.
    /// We configure `max_attempts=5` with `200ms` waits — a budget that,
    /// if reused, would clearly visit the peer five times. The shutdown
    /// must POST exactly once, complete promptly, drain the buffer to
    /// the DLQ, and resolve every handle as `Recovered`.
    #[tokio::test]
    async fn shutdown_does_not_burn_steady_state_retry_budget() {
        let (addr, post_count, server) = run_counting_failing_collector().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 5),
                prop_str("initial_wait", "200ms"),
                prop_str("max_wait", "200ms"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = HttpOutput::from_properties(
            "myout",
            &mp(&[
                peer_block(&url),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                retry,
            ]),
            &ctx,
        )
        .unwrap();

        consume(&output, &event_with("ev1")).await.unwrap();
        consume(&output, &event_with("ev2")).await.unwrap();

        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(2), output.shutdown(Some(&writer)))
            .await
            .expect("shutdown must complete inside the runtime budget")
            .unwrap();
        let elapsed = started.elapsed();
        server.abort();

        let posts = post_count.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            posts, 1,
            "shutdown reused the steady-state retry budget ({} attempts); expected exactly 1",
            posts
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {:?} — must be bounded by SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT",
            elapsed
        );
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body.lines().count(),
            2,
            "both buffered events must land in the DLQ"
        );
    }

    /// Bind a TCP listener that completes the handshake but never reads
    /// the request body or sends a response — the closest in-process
    /// reproduction of the bug repro's unreachable peer. Held connections
    /// are kept alive in the spawned task until it is aborted.
    async fn run_stalled_listener() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        (addr, server)
    }

    /// Regression for the panic root cause: at shutdown the single send
    /// attempt MUST be wrapped in a bounded timeout, otherwise an
    /// unreachable peer (TCP accepts but never replies) outlasts the
    /// runtime's 10s shutdown deadline, the task is aborted, and the
    /// in-flight `shippable: Vec<(Event, QueueAckHandle)>` is dropped
    /// without resolving — firing `QueueAckHandle::Drop` and silently
    /// losing the events.
    ///
    /// Asserts that the elapsed deadline branch routes every event to
    /// the DLQ and resolves `Recovered`, and that shutdown returns
    /// inside `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` plus a small margin.
    #[tokio::test]
    async fn shutdown_bounded_by_attempt_timeout_against_stalled_peer() {
        let (addr, server) = run_stalled_listener().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
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

        consume(&output, &event_with("ev1")).await.unwrap();
        consume(&output, &event_with("ev2")).await.unwrap();

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT + Duration::from_secs(2),
            output.shutdown(Some(&writer)),
        )
        .await;
        let elapsed = started.elapsed();
        server.abort();

        result
            .expect("shutdown must return before runtime would have aborted us")
            .unwrap();
        assert!(
            elapsed < crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT + Duration::from_secs(1),
            "shutdown took {:?} — must be bounded by SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT",
            elapsed
        );
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "both events must reach the DLQ");
        let joined = lines.join("\n");
        assert!(
            joined.contains("deadline exceeded"),
            "DLQ records must carry the elapsed-deadline reason, got: {joined}"
        );
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
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
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
            .sink
            .inner
            .error_log
            .as_ref()
            .expect("error_log must be set");
        assert!(
            Arc::ptr_eq(stored, &writer),
            "constructor must store the exact Arc passed in"
        );
    }

    // -----------------------------------------------------------------
    // Regression tests for shutdown lifecycle handle ownership
    // (consume_shutdown trait dispatch + cooperative cancel + actor
    // model). These pin the contracts so future refactors don't
    // silently re-introduce the leak.
    // -----------------------------------------------------------------

    /// `Output::consume_shutdown` on a batched http output MUST park
    /// the (event, ack) into the buffer without touching the
    /// steady-state retry path. The follow-up `shutdown()` is the
    /// only place handles resolve.
    #[tokio::test]
    async fn consume_shutdown_buffers_for_batched_http() {
        // Unreachable peer: the goal is to verify *no* network call
        // happens on `consume_shutdown` itself; any accidental
        // routing to the steady-state send + retry loop would burn
        // attempts here.
        let dir = tempfile::TempDir::new().unwrap();
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(
            dir.path().join("errored.jsonl"),
        ));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block("http://127.0.0.1:1/"),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                fast_retry_block(),
            ]),
            &ctx,
        )
        .unwrap();

        let (ack, mut rx) = QueueAckHandle::for_test();
        let started = std::time::Instant::now();
        <HttpOutput as Output>::consume_shutdown(&output, &event_with("ev"), ack)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "consume_shutdown returned in {:?} — must be a buffer push, not the retry path",
            elapsed
        );
        assert_eq!(
            output.sink.inner.batch.lock().await.len(),
            1,
            "event must land in the buffer"
        );
        assert!(
            rx.try_recv().is_err(),
            "ack must NOT resolve in consume_shutdown — only in the post-loop shutdown drain"
        );

        // The follow-up shutdown drains it. Unreachable peer → DLQ + Recovered.
        let _ = tokio::time::timeout(Duration::from_secs(2), output.shutdown(Some(&writer))).await;
        let disposition = rx.try_recv().expect("ack must resolve after shutdown");
        assert!(
            matches!(disposition.1, crate::queue::AckDisposition::Recovered),
            "ack must resolve as Recovered (DLQ route), got {:?}",
            disposition.1
        );
    }

    /// In-flight `flush_events` retry MUST exit early when
    /// `is_shutting_down` is set — burning the full retry budget
    /// outlasts the runtime's 10s shutdown deadline and the abort
    /// drops the stack-local batch with handles unresolved (= the
    /// unresolved-handle regression). With `max_attempts=5` +
    /// `initial_wait=1s` (long enough to span the runtime budget),
    /// shutdown must still complete inside
    /// `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` and every ack must resolve.
    #[tokio::test]
    async fn cooperative_cancel_collapses_retry_during_shutdown() {
        let (addr, _post_count, server) = run_counting_failing_collector().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        // Long retry waits so a non-cancelled budget would clearly
        // exceed SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT (3s).
        let retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 5),
                prop_str("initial_wait", "5s"),
                prop_str("max_wait", "5s"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[
                    peer_block(&url),
                    prop_int("batch_size", 2),
                    prop_str("batch_timeout", "30s"),
                    retry,
                ]),
                &ctx,
            )
            .unwrap(),
        );

        // Threshold-trigger an actor flush. The peer 5xx-fails;
        // the actor's flush_events enters its retry loop with a 5s
        // sleep.
        let (ack1, _rx1) = QueueAckHandle::for_test();
        let (ack2, _rx2) = QueueAckHandle::for_test();
        let _ = output.consume(&event_with("a"), ack1).await;
        let output_clone = Arc::clone(&output);
        let event = event_with("b");
        let flush_task = tokio::spawn(async move {
            let _ = output_clone.consume(&event, ack2).await;
        });

        // Let the actor task enter the retry sleep.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Shutdown signals cooperative cancel; the retry sleep
        // bails out via is_shutting_down and the actor flush
        // collapses to DLQ + Recovered for both events.
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(4), output.shutdown(Some(&writer)))
            .await
            .expect("shutdown must complete inside SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT + budget")
            .unwrap();
        let elapsed = started.elapsed();
        let _ = tokio::time::timeout(Duration::from_secs(1), flush_task).await;
        server.abort();

        assert!(
            elapsed < Duration::from_secs(4),
            "shutdown took {:?} — cooperative cancel did not collapse the retry budget",
            elapsed
        );
        assert!(
            path.exists(),
            "DLQ must have been written after the cooperative cancel"
        );
    }

    /// The flusher actor is spawned exactly once at construction (for
    /// `batch_size > 1`) and is the same handle for the lifetime of
    /// the output — there is no per-flush respawn that would re-open
    /// the abort surface the timer task used to expose.
    #[tokio::test]
    async fn flusher_actor_spawned_once_at_construction() {
        let output = HttpOutput::from_properties(
            "test",
            &mp(&[
                peer_block("http://127.0.0.1:1/"),
                prop_int("batch_size", 100),
                prop_str("batch_timeout", "30s"),
                fast_retry_block(),
            ]),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let handle_id_before = {
            let guard = output.sink.actor_handle.lock().await;
            guard
                .as_ref()
                .map(|h| h.id())
                .expect("batched output must spawn the actor")
        };
        // Multiple consumes (sub-threshold) must not respawn the actor.
        consume(&output, &event_with("a")).await.unwrap();
        consume(&output, &event_with("b")).await.unwrap();
        consume(&output, &event_with("c")).await.unwrap();
        let handle_id_after = {
            let guard = output.sink.actor_handle.lock().await;
            guard
                .as_ref()
                .map(|h| h.id())
                .expect("actor must still be the same handle")
        };
        assert_eq!(
            handle_id_before, handle_id_after,
            "the flusher actor must be the same task across consumes — no per-flush respawn"
        );
        // Drain the buffer so the (a,b,c) handles do not leak unresolved
        // when the output drops at end-of-test. The peer is unreachable,
        // so all three route to the test-DLQ-recovery path.
        let _ = tokio::time::timeout(Duration::from_secs(2), output.shutdown(None)).await;
    }

    /// Audit-identified leak (2026-06-27 follow-up): the flusher actor
    /// was mid-`send().await` against a stalled peer when
    /// `shutdown()` signalled. The previous fix collapsed retry sleeps
    /// but not the transport call itself, so the actor sat on its
    /// stack-local `shippable` Vec until the runtime aborted it,
    /// dropping every parked `QueueAckHandle` unresolved (debug:
    /// panic; release: silent loss).
    ///
    /// This test sets up that exact race — actor wakes via
    /// `batch_timeout`, enters `send` against a TCP listener
    /// that accepts but never responds, `shutdown()` fires — and
    /// asserts every handle resolves to `Recovered` (via DLQ) inside
    /// the bounded shutdown budget.
    #[tokio::test]
    async fn actor_send_in_flight_cancels_on_shutdown_against_stalled_peer() {
        let (addr, server) = run_stalled_listener().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        // Short timer wake + large batch_size so the actor (not
        // consume itself) is the path that calls `send`.
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[
                    peer_block(&url),
                    prop_int("batch_size", 100),
                    prop_str("batch_timeout", "100ms"),
                    fast_retry_block(),
                ]),
                &ctx,
            )
            .unwrap(),
        );

        // One sub-threshold event — buffered, actor will pick it up
        // when the timer fires. We must drive consume via the trait
        // method (not the test shim) so the ack stays unresolved
        // until the actor's flush_events resolves it.
        let (ack, mut rx) = QueueAckHandle::for_test();
        <HttpOutput as Output>::consume(&output, &event_with("ev"), ack)
            .await
            .unwrap();

        // Give the actor time to wake, take the batch, and enter
        // `send` against the stalled peer. 200ms > batch_timeout
        // (100ms) plus a small margin for actor wake + transport
        // handshake. The reqwest client's 30s read timeout would be
        // the dominant wait without our cooperative cancel.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now call shutdown. With the cooperative transport cancel,
        // the actor's `send` future is dropped, the loop falls
        // through to the DLQ + Recovered path, the actor exits
        // cleanly, and shutdown joins it well within
        // SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT.
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), output.shutdown(Some(&writer)))
            .await
            .expect("shutdown must complete inside the bounded budget")
            .unwrap();
        let elapsed = started.elapsed();
        server.abort();

        // The runtime budget for the whole shutdown sequence is 10s;
        // we want this case to fit well within the per-attempt
        // bound (3s) plus a small post-actor drain margin.
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took {:?} — transport cancel did not collapse the in-flight send",
            elapsed
        );

        // The handle must resolve (not drop). Recovered (DLQ-routed)
        // is the expected disposition since the peer never responded.
        let (_pos, disposition) = rx
            .try_recv()
            .expect("ack must resolve, not drop unresolved");
        assert!(
            matches!(disposition, crate::queue::AckDisposition::Recovered),
            "expected Recovered (DLQ), got {:?}",
            disposition
        );

        // DLQ must have the event.
        assert!(
            path.exists(),
            "DLQ file must be written when transport cancel collapses the send"
        );
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body.lines().count(),
            1,
            "the single buffered event must land in the DLQ"
        );
    }

    /// Threshold-triggered actor flush + shutdown race. The previous
    /// drop-cancel test exercised the timer-driven path; this one
    /// exercises the `batch.len() >= batch_size` notify path. Both
    /// must resolve every handle inside the bounded shutdown budget.
    #[tokio::test]
    async fn actor_threshold_flush_cancels_on_shutdown_against_stalled_peer() {
        let (addr, server) = run_stalled_listener().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[
                    peer_block(&url),
                    prop_int("batch_size", 2),
                    prop_str("batch_timeout", "30s"),
                    fast_retry_block(),
                ]),
                &ctx,
            )
            .unwrap(),
        );

        // Push two events: the second hits the threshold, fires
        // flush_notify, the actor wakes and enters send
        // against the stalled peer.
        let (ack1, mut rx1) = QueueAckHandle::for_test();
        let (ack2, mut rx2) = QueueAckHandle::for_test();
        <HttpOutput as Output>::consume(&output, &event_with("a"), ack1)
            .await
            .unwrap();
        <HttpOutput as Output>::consume(&output, &event_with("b"), ack2)
            .await
            .unwrap();

        // Let the actor reach the in-flight send.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), output.shutdown(Some(&writer)))
            .await
            .expect("shutdown must complete inside the bounded budget")
            .unwrap();
        let elapsed = started.elapsed();
        server.abort();

        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took {:?} — threshold-flush in-flight send was not cancelled",
            elapsed
        );

        let (_, d1) = rx1.try_recv().expect("ack1 must resolve, not drop");
        let (_, d2) = rx2.try_recv().expect("ack2 must resolve, not drop");
        assert!(
            matches!(d1, crate::queue::AckDisposition::Recovered),
            "ack1 expected Recovered, got {:?}",
            d1
        );
        assert!(
            matches!(d2, crate::queue::AckDisposition::Recovered),
            "ack2 expected Recovered, got {:?}",
            d2
        );

        // Both events land in the DLQ.
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body.lines().count(),
            2,
            "both threshold-flushed events must land in the DLQ"
        );
    }

    /// Singleton (`batch_size <= 1`) actor + shutdown race. In
    /// the current implementation batch_size=1 no longer takes a
    /// separate inline retry path on the queue consumer's task —
    /// every event flows through the actor, so the same bounded-
    /// shutdown contract applies. consume() pushes one event
    /// (threshold reached at 1), the actor wakes, enters
    /// send, shutdown signals, the handle resolves to
    /// Recovered via DLQ.
    #[tokio::test]
    async fn singleton_actor_send_cancels_on_shutdown_against_stalled_peer() {
        let (addr, server) = run_stalled_listener().await;
        let url = format!("http://{}/", addr);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[
                    peer_block(&url),
                    prop_int("batch_size", 1),
                    fast_retry_block(),
                ]),
                &ctx,
            )
            .unwrap(),
        );

        let (ack, mut rx) = QueueAckHandle::for_test();
        <HttpOutput as Output>::consume(&output, &event_with("singleton"), ack)
            .await
            .unwrap();

        // Let the actor reach the in-flight send.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), output.shutdown(Some(&writer)))
            .await
            .expect("shutdown must complete inside the bounded budget")
            .unwrap();
        let elapsed = started.elapsed();
        server.abort();

        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took {:?} — singleton in-flight send was not cancelled",
            elapsed
        );

        let (_, disp) = rx.try_recv().expect("ack must resolve, not drop");
        assert!(
            matches!(disp, crate::queue::AckDisposition::Recovered),
            "singleton ack expected Recovered, got {:?}",
            disp
        );
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body.lines().count(),
            1,
            "the singleton event must land in the DLQ"
        );
    }

    /// Bounded ownership pin: `batch_size = 1` sets the permit
    /// capacity to `batch_size * 2 = 2`. That is the smallest bound
    /// that allows a threshold-driven flush to coexist with an
    /// in-flight refill without stalling — one permit is held by
    /// the event the actor moved into `shippable` for its in-flight
    /// send, one is held by the newly-parked event awaiting the
    /// next actor iteration. A third concurrent event must block
    /// on `permits.acquire_owned()` because both slots are taken.
    ///
    /// Also pins the shutdown-race bypass: while the third
    /// `consume()` is blocked, flipping the runtime shutdown watch
    /// wakes the permit acquire, bypasses the bound (permit =
    /// `None`), pushes the event, and returns `Ok` — otherwise
    /// graceful shutdown could not proceed past a saturated sink.
    #[tokio::test]
    async fn batched_permit_bound_is_two_for_batch_size_one_and_shutdown_bypasses() {
        let (addr, server) = run_stalled_listener().await;
        let url = format!("http://{}/", addr);
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let ctx = crate::modules::BuildContext {
            shutdown_signal: sd_rx,
            ..crate::modules::BuildContext::for_testing()
        };
        // Long backoff → the actor stays parked in-flight against
        // the stalled peer for the duration of the test rather than
        // churning through the retry budget and freeing the permit.
        let long_retry = Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 100),
                prop_str("initial_wait", "5s"),
                prop_str("max_wait", "5s"),
                Property::KeyValue {
                    key: "backoff".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::Ident(vec!["fixed".into()])),
                    value_span: None,
                },
            ],
        };
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[peer_block(&url), prop_int("batch_size", 1), long_retry]),
                &ctx,
            )
            .unwrap(),
        );

        let (ack1, _rx1) = QueueAckHandle::for_test();
        let (ack2, _rx2) = QueueAckHandle::for_test();
        let (ack3, _rx3) = QueueAckHandle::for_test();

        // First consume: pushes 1 event, hits threshold (batch_size=1),
        // fires flush_notify, actor picks up and enters the send.
        // Permit A now held inside `shippable`.
        <HttpOutput as Output>::consume(&output, &event_with("a"), ack1)
            .await
            .unwrap();
        // Give the actor time to drain the buffer and enter the
        // in-flight send against the stalled peer.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Second consume: buffer refill, permit B parked with the
        // new tuple. Now both permits are held.
        <HttpOutput as Output>::consume(&output, &event_with("b"), ack2)
            .await
            .unwrap();

        // Third consume: no permit available. Must block on
        // `permits.acquire_owned()`.
        let output_clone = Arc::clone(&output);
        let mut consume3 = tokio::spawn(async move {
            <HttpOutput as Output>::consume(&output_clone, &event_with("c"), ack3).await
        });
        tokio::select! {
            _ = &mut consume3 => panic!("3rd consume() must block awaiting a permit"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        // Flip runtime shutdown watch: the blocked permit acquire
        // wakes via `shutdown_rx.changed()`, bypasses the bound
        // (permit = None), pushes to buffer, returns Ok.
        sd_tx.send(true).unwrap();
        let consume3_result = tokio::time::timeout(Duration::from_secs(2), consume3)
            .await
            .expect("3rd consume() must unblock within 2s of shutdown flip")
            .expect("consume task must not panic");
        consume3_result.expect("consume must return Ok after shutdown bypass");

        // Clean shutdown so every parked handle resolves.
        tokio::time::timeout(Duration::from_secs(5), output.shutdown(None))
            .await
            .expect("shutdown must complete inside bounded budget")
            .unwrap();
        server.abort();
    }

    /// Byte-preserving pin: the HTTP output must forward non-UTF-8
    /// egress bytes to the receiver verbatim, not through a
    /// `String::from_utf8_lossy` that would substitute U+FFFD for
    /// every invalid byte sequence. The daemon does not know what
    /// downstream schema the operator is shipping (protobuf,
    /// FlatBuffers, raw binary, syslog with 8-bit MSGs), and
    /// silently rewriting the payload turns HTTP into a lossy
    /// transport — the exact bug the other sinks were audited free
    /// of. Encodes an event whose bytes contain `0xff 0x00 0x80`
    /// (canonical invalid UTF-8 shape: unpaired high byte + NUL +
    /// unpaired high byte) and asserts the receiver's request body
    /// matches byte-for-byte.
    #[tokio::test]
    async fn http_output_forwards_non_utf8_egress_verbatim() {
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        #[derive(Clone)]
        struct State {
            received: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
        }
        async fn handle(
            axum::extract::State(state): axum::extract::State<State>,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            state.received.lock().await.push(body.to_vec());
            (StatusCode::OK, "")
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let state = State {
            received: Arc::clone(&received),
        };
        let app = Router::new().route("/", post(handle)).with_state(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}/", addr);
        let output = Arc::new(
            HttpOutput::from_properties(
                "test",
                &mp(&[
                    peer_block(&url),
                    prop_int("batch_size", 1),
                    fast_retry_block(),
                ]),
                &crate::modules::BuildContext::for_testing(),
            )
            .unwrap(),
        );

        // Canonical non-UTF-8 payload: unpaired continuation bytes,
        // a NUL in the middle, and a lone high byte at the tail.
        let raw: &[u8] = &[0xff, 0x00, 0x80, b'x', 0xc3, 0x28];
        let ev = Event::new(
            bytes::Bytes::copy_from_slice(raw),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let (ack, mut rx) = QueueAckHandle::for_test();
        <HttpOutput as Output>::consume(&output, &ev, ack)
            .await
            .unwrap();

        // Give the flusher a moment to ship then poll for delivery.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let body = loop {
            {
                let guard = received.lock().await;
                if let Some(first) = guard.first() {
                    break first.clone();
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("receiver never observed the request within 3s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            body, raw,
            "HTTP output must forward non-UTF-8 egress bytes verbatim; got {:?}, expected {:?}",
            body, raw
        );
        // Sanity: single-event batch resolves as Delivered, no DLQ.
        let (_, disp) = rx.try_recv().expect("ack must resolve");
        assert!(
            matches!(disp, crate::queue::AckDisposition::Delivered),
            "expected Delivered, got {:?}",
            disp
        );

        tokio::time::timeout(Duration::from_secs(3), output.shutdown(None))
            .await
            .expect("shutdown must complete")
            .unwrap();
        server.abort();
    }
}

//! OTLP output: forwards Events to an OpenTelemetry collector / SaaS
//! backend via the OTLP/HTTP transport family (and OTLP/gRPC in a
//! follow-up commit).
//!
//! Each Event's `egress` is expected to be the singleton ResourceLogs
//! protobuf bytes produced by `otlp.encode_resourcelog_protobuf` —
//! this is the v0.5.0 hop contract for OTLP (see
//! `_DESIGN_V050_OTLP.md` §4.2). Output buffers the per-Event
//! ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the
//! batch in an `ExportLogsServiceRequest`, and ships it.
//!
//! ## Configuration
//!
//! ```text
//! def output otlp_out {
//!     type otlp
//!     endpoint "https://collector.example.com:4318"
//!     protocol "http_protobuf"   // http_json | http_protobuf [| grpc — TODO]
//!     batch_size 512
//!     batch_timeout "5s"
//!     headers {
//!         Authorization "Bearer ${env.OTLP_TOKEN}"
//!     }
//!     tls {
//!         ca "/etc/limpid/ca.crt"
//!     }
//! }
//! ```
//!
//! ### `batch_level`
//!
//! v0.5.0 only implements `none` (pure concat). The proto3 `repeated`
//! field guarantees that concatenating wire bytes is equivalent to a
//! merged message at the receiver, so a `none` batch is semantically
//! valid OTLP — what changes with `resource` / `scope` levels is wire
//! efficiency (fewer ResourceLogs / ScopeLogs entries), not the data
//! itself. The merge logic is queued for v0.5.x; see the design memo
//! §4.5.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest, logs::v1::ResourceLogs,
};
use prost::Message;
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};

#[derive(Debug, Clone, Copy)]
enum Protocol {
    HttpJson,
    HttpProtobuf,
    // Grpc deferred — see design memo §6.1 step ordering.
}

impl Protocol {
    fn parse(s: &str, output_name: &str) -> Result<Self> {
        match s {
            "http_json" => Ok(Protocol::HttpJson),
            "http_protobuf" => Ok(Protocol::HttpProtobuf),
            "grpc" => bail!(
                "output '{}': protocol 'grpc' is not implemented in v0.5.0 (use http_protobuf or http_json)",
                output_name
            ),
            other => bail!(
                "output '{}': unknown protocol '{}' (expected http_json or http_protobuf)",
                output_name,
                other
            ),
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Protocol::HttpJson => "application/json",
            Protocol::HttpProtobuf => "application/x-protobuf",
        }
    }
}

struct Inner {
    endpoint: String,
    protocol: Protocol,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    client: reqwest::Client,
    /// Buffered per-Event singleton ResourceLogs proto bytes. Each
    /// entry is exactly what `otlp.encode_resourcelog_protobuf`
    /// produced; flush concatenates them into one
    /// ExportLogsServiceRequest.
    batch: Mutex<Vec<Bytes>>,
}

pub struct OtlpOutput {
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for OtlpOutput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let endpoint = props::get_string(properties, "endpoint")
            .ok_or_else(|| anyhow!("output '{}': otlp requires 'endpoint'", name))?;
        let protocol_str = props::get_string(properties, "protocol")
            .or_else(|| props::get_ident(properties, "protocol"))
            .unwrap_or_else(|| "http_protobuf".to_string());
        let protocol = Protocol::parse(&protocol_str, name)?;
        let batch_size = props::get_positive_int(properties, "batch_size")?.unwrap_or(1) as usize;
        let batch_timeout = match props::get_string(properties, "batch_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };
        let batch_level = props::get_string(properties, "batch_level")
            .or_else(|| props::get_ident(properties, "batch_level"))
            .unwrap_or_else(|| "none".to_string());
        if batch_level != "none" {
            bail!(
                "output '{}': batch_level '{}' is not implemented in v0.5.0 (only 'none' supported)",
                name,
                batch_level
            );
        }

        let mut headers = Vec::new();
        if let Some(block) = props::get_block(properties, "headers") {
            for prop in block {
                if let Property::KeyValue {
                    key, value: expr, ..
                } = prop
                    && let Some(val) = match &expr.kind {
                        crate::dsl::ast::ExprKind::StringLit(s) => Some(s.clone()),
                        crate::dsl::ast::ExprKind::Ident(parts) => Some(parts.join(".")),
                        _ => None,
                    }
                {
                    headers.push((key.clone(), val));
                }
            }
        }

        let mut client_builder = reqwest::Client::builder();
        // TLS configuration — accept the same `tls { ca = "..." }` /
        // `verify` shape that the http output uses, so users don't
        // need to learn a second dialect.
        let verify = props::get_ident(properties, "verify")
            .map(|s| s != "false")
            .unwrap_or(true);
        if !verify {
            client_builder = client_builder.danger_accept_invalid_certs(true);
        }
        if let Some(block) = props::get_block(properties, "tls")
            && let Some(ca_path) = props::get_string(block, "ca")
        {
            let pem = std::fs::read(&ca_path)
                .with_context(|| format!("output '{}': cannot read CA cert {}", name, ca_path))?;
            let cert = reqwest::Certificate::from_pem(&pem).with_context(|| {
                format!("output '{}': invalid CA cert PEM at {}", name, ca_path)
            })?;
            client_builder = client_builder.add_root_certificate(cert);
        }
        let client = client_builder
            .build()
            .with_context(|| format!("output '{}': failed to build HTTP client", name))?;

        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                protocol,
                headers,
                batch_timeout,
                client,
                batch: Mutex::new(Vec::new()),
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for OtlpOutput {
    async fn write(&self, event: &Event) -> Result<()> {
        // Egress must be the singleton ResourceLogs proto bytes
        // produced by `otlp.encode_resourcelog_protobuf`. We do not
        // re-encode here — that's the process layer's job. If the user
        // wired the pipeline differently (e.g. forgot the encode), the
        // collector will see malformed wire and reject the batch.
        let proto = event.egress.clone();
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
}

impl OtlpOutput {
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
        let count = drained.len();

        // Decode each per-Event ResourceLogs and gather into the
        // request envelope. v0.5.0 batch_level=none → no merging.
        let mut req = ExportLogsServiceRequest::default();
        req.resource_logs.reserve(drained.len());
        for proto in &drained {
            let rl = ResourceLogs::decode(&**proto).with_context(|| {
                "output otlp: pipeline egress is not a valid ResourceLogs proto (wire it through `otlp.encode_resourcelog_protobuf`)"
            })?;
            req.resource_logs.push(rl);
        }

        let result = match self.inner.protocol {
            Protocol::HttpProtobuf => self.send_http_protobuf(&req).await,
            Protocol::HttpJson => self.send_http_json(&req).await,
        };

        match result {
            Ok(()) => {
                self.metrics.events_written.fetch_add(count as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.metrics.events_failed.fetch_add(count as u64, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    async fn send_http_protobuf(&self, req: &ExportLogsServiceRequest) -> Result<()> {
        let mut buf = Vec::with_capacity(req.encoded_len());
        req.encode(&mut buf)
            .map_err(|e| anyhow!("output otlp: encode failed: {e}"))?;
        self.send_http(buf).await
    }

    async fn send_http_json(&self, req: &ExportLogsServiceRequest) -> Result<()> {
        let body = serde_json::to_vec(req)
            .map_err(|e| anyhow!("output otlp: JSON encode failed: {e}"))?;
        self.send_http(body).await
    }

    async fn send_http(&self, body: Vec<u8>) -> Result<()> {
        let mut req = self
            .inner
            .client
            .post(&self.inner.endpoint)
            .header("Content-Type", self.inner.protocol.content_type())
            .body(body);
        for (k, v) in &self.inner.headers {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("output otlp: POST {} failed", self.inner.endpoint))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "output otlp: {} returned HTTP {} — {}",
                self.inner.endpoint,
                status.as_u16(),
                text.chars().take(500).collect::<String>()
            );
        }
        Ok(())
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
            let count = drained.len();
            let mut req = ExportLogsServiceRequest::default();
            req.resource_logs.reserve(drained.len());
            for proto in &drained {
                match ResourceLogs::decode(&**proto) {
                    Ok(rl) => req.resource_logs.push(rl),
                    Err(e) => {
                        tracing::warn!(
                            "otlp flush timer: dropping malformed ResourceLogs ({})",
                            e
                        );
                        metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }
            let body_result = match inner.protocol {
                Protocol::HttpProtobuf => {
                    let mut buf = Vec::with_capacity(req.encoded_len());
                    req.encode(&mut buf).ok();
                    Ok(buf)
                }
                Protocol::HttpJson => serde_json::to_vec(&req)
                    .map_err(|e| anyhow!("OTLP/JSON encode: {e}")),
            };
            let body = match body_result {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("otlp flush timer: encode failed ({})", e);
                    metrics
                        .events_failed
                        .fetch_add(count as u64, Ordering::Relaxed);
                    return;
                }
            };
            let mut request = inner
                .client
                .post(&inner.endpoint)
                .header("Content-Type", inner.protocol.content_type())
                .body(body);
            for (k, v) in &inner.headers {
                request = request.header(k, v);
            }
            match request.send().await {
                Ok(resp) if resp.status().is_success() => {
                    metrics
                        .events_written
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
                Ok(resp) => {
                    tracing::warn!(
                        "otlp flush timer: {} returned HTTP {}",
                        inner.endpoint,
                        resp.status().as_u16()
                    );
                    metrics
                        .events_failed
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!("otlp flush timer: POST failed ({})", e);
                    metrics
                        .events_failed
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
            }
        });
        *handle = Some(new_handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind};

    fn prop_str(key: &str, val: &str) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            value: Expr::spanless(ExprKind::StringLit(val.to_string())),
            value_span: None,
        }
    }

    fn prop_int(key: &str, val: i64) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            value: Expr::spanless(ExprKind::IntLit(val)),
            value_span: None,
        }
    }

    #[test]
    fn requires_endpoint() {
        let err = OtlpOutput::from_properties("o", &[]).err().unwrap();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn rejects_grpc_protocol_in_v050() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_str("protocol", "grpc"),
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(err.to_string().contains("not implemented"));
    }

    #[test]
    fn rejects_unknown_protocol() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_str("protocol", "carrier_pigeon"),
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(err.to_string().contains("unknown protocol"));
    }

    #[test]
    fn rejects_unsupported_batch_level() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_str("batch_level", "scope"),
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(err.to_string().contains("not implemented"));
    }

    #[test]
    fn defaults_protocol_to_http_protobuf() {
        let props = vec![prop_str("endpoint", "http://x")];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert!(matches!(output.inner.protocol, Protocol::HttpProtobuf));
    }

    #[test]
    fn batch_size_defaults_to_one() {
        let props = vec![prop_str("endpoint", "http://x")];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert_eq!(output.batch_size, 1);
    }

    #[test]
    fn batch_size_explicit() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_int("batch_size", 64),
        ];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert_eq!(output.batch_size, 64);
    }
}

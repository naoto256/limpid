//! HTTP output: sends events to an HTTP/HTTPS endpoint.
//!
//! Supports Elasticsearch Bulk API, Splunk HEC, Datadog, Loki,
//! and any generic HTTP endpoint.
//!
//! Properties:
//!   url          "https://es:9200/_bulk"        — required
//!   method       POST                            — optional (default: POST)
//!   content_type "application/json"              — optional (default: application/json)
//!   batch_size   100                             — optional (default: 1, no batching)
//!   batch_timeout "5s"                           — optional (flush interval, default: 5s)
//!   verify       false                           — optional (default: true, verify TLS certs)
//!   compress     gzip                            — optional (gzip compress request body)
//!   headers {                                    — optional extra headers
//!       Authorization "Bearer xxx"
//!   }
//!   tls {                                        — optional custom CA
//!       ca "/path/to/ca.crt"
//!   }

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, ModuleSchema, Output};

/// Shared state between write() and the flush timer task.
struct Inner {
    url: String,
    method: String,
    content_type: String,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    compress: bool,
    client: reqwest::Client,
    batch: Mutex<Vec<String>>,
}

pub struct HttpOutput {
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for HttpOutput {
    fn schema() -> ModuleSchema {
        ModuleSchema::default()
    }

    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let url = props::get_string(properties, "url")
            .ok_or_else(|| anyhow::anyhow!("output '{}': http requires 'url'", name))?;
        let method = props::get_ident(properties, "method")
            .unwrap_or_else(|| "POST".to_string())
            .to_uppercase();
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

        // Parse headers block
        let mut headers = Vec::new();
        if let Some(block) = props::get_block(properties, "headers") {
            for prop in block {
                if let Property::KeyValue(key, expr) = prop
                    && let Some(val) = match expr {
                        crate::dsl::ast::Expr::StringLit(s) => Some(s.clone()),
                        crate::dsl::ast::Expr::Ident(parts) => Some(parts.join(".")),
                        _ => None,
                    }
                {
                    headers.push((key.clone(), val));
                }
            }
        }

        // TLS / verify configuration
        let is_https = url.starts_with("https://");
        let verify = props::get_ident(properties, "verify")
            .map(|s| s != "false")
            .unwrap_or(true);
        let has_tls_block = props::get_block(properties, "tls").is_some();

        if !is_https {
            if !verify {
                tracing::warn!(
                    "output '{}': 'verify false' has no effect on non-HTTPS URL",
                    name
                );
            }
            if has_tls_block {
                tracing::warn!(
                    "output '{}': 'tls' block has no effect on non-HTTPS URL",
                    name
                );
            }
        }

        if !verify && has_tls_block {
            tracing::warn!(
                "output '{}': 'tls' block is ignored because 'verify false' disables certificate validation",
                name
            );
        }

        let mut client_builder = reqwest::Client::builder().timeout(Duration::from_secs(30));

        if !verify {
            client_builder = client_builder.danger_accept_invalid_certs(true);
        }

        if verify
            && let Some(tls_block) = props::get_block(properties, "tls")
            && let Some(ca_path) = props::get_string(tls_block, "ca")
        {
            let ca_pem = std::fs::read(&ca_path).with_context(|| {
                format!("output '{}': failed to read CA cert: {}", name, ca_path)
            })?;
            let ca_cert = reqwest::Certificate::from_pem(&ca_pem)
                .with_context(|| format!("output '{}': invalid CA cert: {}", name, ca_path))?;
            client_builder = client_builder.add_root_certificate(ca_cert);
        }

        let client = client_builder
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            inner: Arc::new(Inner {
                url,
                method,
                content_type,
                headers,
                batch_timeout,
                compress,
                client,
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
    async fn write(&self, event: &Event) -> Result<()> {
        let msg = String::from_utf8_lossy(&event.message).into_owned();

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

        let mut request = match self.method.as_str() {
            "PUT" => self.client.put(&self.url),
            _ => self.client.post(&self.url),
        };

        request = request.header("Content-Type", &self.content_type);

        if self.compress {
            request = request.header("Content-Encoding", "gzip");
        }

        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        let response = request
            .body(body)
            .send()
            .await
            .with_context(|| format!("http output: request to {} failed", self.url))?;

        let status = response.status();
        if !status.is_success() {
            let resp_body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "http output: {} returned {} — {}",
                self.url,
                status,
                resp_body.chars().take(200).collect::<String>()
            );
        }

        Ok(())
    }
}

impl Drop for HttpOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
        // Check buffer via try_lock (best-effort in Drop)
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

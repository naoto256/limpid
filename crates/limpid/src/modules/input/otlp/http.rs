//! OTLP/HTTP input: receive logs via the OTLP/HTTP transport.
//!
//! Listens for `POST /v1/logs` requests, accepts either
//! `application/x-protobuf` (canonical) or `application/json`
//! payloads, and delegates request splitting to
//! [`super::split_request`]. The shared helper is what every OTLP
//! input transport runs through, so the per-Event shape stays
//! identical across HTTP and gRPC.
//!
//! ## Configuration
//!
//! ```text
//! def input otlp_in {
//!     type otlp_http
//!     bind "0.0.0.0:4318"
//!     body_limit "16MB"      // optional, default 16 MiB
//! }
//! ```
//!
//! Per-Event shape (input writes only this much; payload semantics
//! belong to the process layer per Principle 2):
//!
//! - `ingress` = singleton ResourceLogs (1 Resource, 1 Scope, 1
//!   LogRecord) encoded as protobuf wire bytes
//! - `egress`  = `ingress.clone()` (default — process layer rewrites
//!   if needed)
//! - `source`  = TCP peer address
//! - `received_at` = `Utc::now()` at request handling time
//! - `workspace` = empty (decode is the process layer's job via
//!   `otlp.decode_resourcelog_protobuf(ingress)`)

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::split_request;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

/// Default body cap for OTLP/HTTP requests. axum 0.7 itself defaults
/// to 2 MiB, which is too small for typical collector → collector
/// batches (~5–20 MiB is common). 16 MiB is the convention adopted
/// by most production OTLP receivers.
const DEFAULT_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

pub struct OtlpHttpInput {
    bind_addr: String,
    body_limit: usize,
    metrics: Arc<InputMetrics>,
}

impl Module for OtlpHttpInput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let bind = props::get_string(properties, "bind")
            .unwrap_or_else(|| "0.0.0.0:4318".to_string());
        let body_limit = match props::get_string(properties, "body_limit") {
            Some(s) => {
                let bytes = props::parse_size(&s)?;
                if bytes == 0 {
                    anyhow::bail!(
                        "input '{}': body_limit must be greater than 0 (got '{}')",
                        name,
                        s
                    );
                }
                if bytes > usize::MAX as u64 {
                    anyhow::bail!(
                        "input '{}': body_limit '{}' exceeds platform addressable size",
                        name,
                        s
                    );
                }
                bytes as usize
            }
            None => DEFAULT_BODY_LIMIT_BYTES,
        };
        Ok(Self {
            bind_addr: bind,
            body_limit,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpHttpInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<Event>,
    metrics: Arc<InputMetrics>,
}

#[async_trait::async_trait]
impl Input for OtlpHttpInput {
    async fn run(
        self,
        tx: mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let state = AppState {
            tx,
            metrics: Arc::clone(&self.metrics),
        };
        let app = Router::new()
            .route("/v1/logs", post(handle_logs))
            .layer(DefaultBodyLimit::max(self.body_limit))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(&self.bind_addr).await?;
        info!(
            "otlp_http listening on {} (body_limit={} bytes)",
            self.bind_addr, self.body_limit
        );

        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        });

        if let Err(e) = server.await {
            warn!("otlp_http server error: {}", e);
        }
        info!("otlp_http: shutting down");
        Ok(())
    }
}

/// Handler for `POST /v1/logs`. Decodes the wire body per
/// `Content-Type`, then hands the parsed request off to
/// [`split_request`] which is shared with [`super::grpc`].
async fn handle_logs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    state.metrics.events_received.fetch_add(1, Ordering::Relaxed);

    // Default to protobuf when Content-Type is missing or unrecognised
    // — canonical OTLP form, matches what `opentelemetry-collector`
    // does when clients omit the header.
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_default();
    let req = match content_type.as_str() {
        "application/json" => match serde_json::from_slice::<ExportLogsServiceRequest>(&body) {
            Ok(r) => r,
            Err(e) => {
                warn!("otlp_http [{peer}]: JSON decode failed: {e}");
                state
                    .metrics
                    .events_invalid
                    .fetch_add(1, Ordering::Relaxed);
                return (StatusCode::BAD_REQUEST, "invalid OTLP/JSON payload").into_response();
            }
        },
        _ => match ExportLogsServiceRequest::decode(&*body) {
            Ok(r) => r,
            Err(e) => {
                warn!("otlp_http [{peer}]: protobuf decode failed: {e}");
                state
                    .metrics
                    .events_invalid
                    .fetch_add(1, Ordering::Relaxed);
                return (StatusCode::BAD_REQUEST, "invalid OTLP/protobuf payload")
                    .into_response();
            }
        },
    };

    let outcome = split_request(req, peer, &state.metrics, &state.tx, "otlp_http").await;
    if outcome.aborted {
        return (StatusCode::SERVICE_UNAVAILABLE, "pipeline closed").into_response();
    }
    // OTLP/HTTP does not return `partial_success` on the wire — the
    // gRPC variant does, but `application/x-protobuf` over HTTP would
    // need an `ExportLogsServiceResponse` body which we omit for
    // simplicity. `events_invalid` already counts the rejects.
    (StatusCode::OK, "").into_response()
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

    #[test]
    fn defaults_bind_address_and_body_limit() {
        let i = OtlpHttpInput::from_properties("o", &[]).unwrap();
        assert_eq!(i.bind_addr, "0.0.0.0:4318");
        assert_eq!(i.body_limit, DEFAULT_BODY_LIMIT_BYTES);
    }

    #[test]
    fn body_limit_accepts_size_suffix() {
        let i = OtlpHttpInput::from_properties("o", &[prop_str("body_limit", "1MB")]).unwrap();
        assert_eq!(i.body_limit, 1024 * 1024);
        let i = OtlpHttpInput::from_properties("o", &[prop_str("body_limit", "64MB")]).unwrap();
        assert_eq!(i.body_limit, 64 * 1024 * 1024);
    }

    #[test]
    fn body_limit_zero_is_rejected() {
        let err = OtlpHttpInput::from_properties("o", &[prop_str("body_limit", "0")])
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("body_limit must be greater than 0"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn body_limit_unrecognised_format_propagates_parse_error() {
        // parse_size's own error wording carries through.
        let err = OtlpHttpInput::from_properties("o", &[prop_str("body_limit", "huge")])
            .err()
            .unwrap();
        assert!(err.to_string().contains("invalid size"), "unexpected: {err}");
    }

    /// Wire-level: bring up the input on an ephemeral port with a
    /// 256-byte cap, then POST a 4 KiB body. axum's
    /// `DefaultBodyLimit` layer must reject with HTTP 413 *Payload
    /// Too Large* before the handler ever runs.
    ///
    /// We construct the body via the OTLP output's encode path
    /// (`batch_level=scope` collapsing many LogRecords into one
    /// request) rather than hand-rolling a payload — that way the
    /// test exercises the same wire shape a real OTLP relay would
    /// produce.
    #[tokio::test]
    async fn body_limit_rejects_oversize_request() {
        use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
        use opentelemetry_proto::tonic::common::v1::{
            AnyValue, InstrumentationScope, KeyValue, any_value,
        };
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release before the input rebinds

        let input = OtlpHttpInput::from_properties(
            "test",
            &[
                prop_str("bind", &addr.to_string()),
                prop_str("body_limit", "256"),
            ],
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            let _ = input.run(tx, shutdown_rx).await;
        });
        // Give the bind a moment.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Build a request whose serialised body comfortably exceeds
        // 256 bytes — 50 LogRecords with chatty bodies do it.
        let mut log_records = Vec::with_capacity(50);
        for i in 0..50u64 {
            log_records.push(LogRecord {
                time_unix_nano: i,
                severity_number: 9,
                severity_text: "INFO".into(),
                body: Some(AnyValue {
                    value: Some(any_value::Value::StringValue(format!(
                        "padding-record-{i:03}-with-enough-text-to-blow-the-256B-cap"
                    ))),
                }),
                ..Default::default()
            });
        }
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("body-limit-test".into())),
                        }),
                    }],
                    dropped_attributes_count: 0,
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "test".into(),
                        version: "0".into(),
                        attributes: vec![],
                        dropped_attributes_count: 0,
                    }),
                    log_records,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };
        let mut body = Vec::with_capacity(req.encoded_len());
        req.encode(&mut body).unwrap();
        assert!(
            body.len() > 256,
            "test fixture must exceed body_limit (got {} bytes)",
            body.len()
        );

        let resp = reqwest::Client::new()
            .post(format!("http://{}/v1/logs", addr))
            .header("Content-Type", "application/x-protobuf")
            .body(body)
            .send()
            .await
            .expect("request reaches the server");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "axum DefaultBodyLimit must reject the oversize body"
        );

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }

    /// Counterpoint: a request that fits under the cap is accepted
    /// (sanity check that the cap is not catching everything).
    #[tokio::test]
    async fn body_limit_accepts_request_under_cap() {
        use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let input = OtlpHttpInput::from_properties(
            "test",
            &[
                prop_str("bind", &addr.to_string()),
                prop_str("body_limit", "1KB"),
            ],
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            let _ = input.run(tx, shutdown_rx).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Empty request is well under any size cap.
        let req = ExportLogsServiceRequest {
            resource_logs: vec![],
        };
        let mut body = Vec::new();
        req.encode(&mut body).unwrap();
        let resp = reqwest::Client::new()
            .post(format!("http://{}/v1/logs", addr))
            .header("Content-Type", "application/x-protobuf")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // Empty payload → no Events emitted (events_received still
        // bumped per RPC, but the channel stays clean).
        assert!(rx.try_recv().is_err());

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }
}

//! OTLP/gRPC input: receive logs via the OTLP/gRPC transport.
//!
//! Hosts the `opentelemetry.proto.collector.logs.v1.LogsService`
//! gRPC service. Each `Export` RPC arrives as an
//! `ExportLogsServiceRequest`; request splitting is delegated to
//! [`super::split_request`], the same helper that drives the
//! [`super::http`] transport — so both inputs emit the identical
//! per-Event shape (1 Resource + 1 Scope + 1 LogRecord, encoded as
//! protobuf wire bytes).
//!
//! ## Configuration
//!
//! ```text
//! def input otlp_in {
//!     type otlp_grpc
//!     bind "0.0.0.0:4317"
//!     rate_limit 10000          // optional, events/sec budget
//!     tls {                      // optional; omit for plaintext
//!         cert "/etc/limpid/server.crt"
//!         key  "/etc/limpid/server.key"
//!         ca   "/etc/limpid/clients-ca.crt"   // optional → mTLS
//!     }
//! }
//! ```
//!
//! Reply: empty `ExportLogsServiceResponse` on success, or
//! `partial_success` with `rejected_log_records` populated when
//! re-encoding fails for some records (rare).
//!
//! With no `tls` block the input listens plaintext, suitable for
//! loopback or behind a TLS-terminating proxy. The `tls` block uses
//! the same shape as every other TLS-aware module (parsed once via
//! `crate::tls::TlsConfig::from_properties_block`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
    logs_service_server::{LogsService, LogsServiceServer},
};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};
use tracing::{info, warn};

use super::split_request;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::input::rate_limit::RateLimiter;
use crate::modules::{HasMetrics, Input, Module};
use crate::tls::TlsConfig;

pub struct OtlpGrpcInput {
    bind_addr: String,
    rate_limit: Option<u64>,
    tls: Option<TlsConfig>,
    metrics: Arc<InputMetrics>,
}

impl Module for OtlpGrpcInput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let bind = props::get_string(properties, "bind")
            .unwrap_or_else(|| "0.0.0.0:4317".to_string());
        let rate_limit = props::get_strictly_positive_int(properties, "rate_limit")?;
        let tls = TlsConfig::from_properties_block(&format!("input '{}'", name), properties)?;
        Ok(Self {
            bind_addr: bind,
            rate_limit,
            tls,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpGrpcInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for OtlpGrpcInput {
    async fn run(
        self,
        tx: mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let addr: SocketAddr = self.bind_addr.parse().map_err(|e| {
            anyhow::anyhow!("otlp_grpc: invalid bind address '{}': {e}", self.bind_addr)
        })?;
        let tls_config = match &self.tls {
            Some(t) => {
                crate::tls::install_default_crypto_provider();
                Some(load_tonic_tls_config(t).await?)
            }
            None => None,
        };
        let rate_limiter = self.rate_limit.map(|r| Arc::new(RateLimiter::new(r)));
        info!(
            "otlp_grpc listening on {} ({})",
            addr,
            if tls_config.is_some() { "TLS" } else { "plaintext" }
        );
        if let Some(rate) = self.rate_limit {
            info!("otlp_grpc rate_limit: {} events/sec", rate);
        }

        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::clone(&self.metrics),
            rate_limiter,
        };
        let mut builder = tonic::transport::Server::builder();
        if let Some(tls) = tls_config {
            builder = builder
                .tls_config(tls)
                .context("otlp_grpc: failed to install server TLS config")?;
        }
        let server = builder
            .add_service(LogsServiceServer::new(svc))
            .serve_with_shutdown(addr, async move {
                let _ = shutdown.changed().await;
            });

        if let Err(e) = server.await {
            warn!("otlp_grpc server error: {}", e);
        }
        info!("otlp_grpc: shutting down");
        Ok(())
    }
}

/// Read cert / key / CA PEM files and assemble tonic's
/// `ServerTlsConfig`. Parallels `crate::tls::build_server_config`
/// (rustls-native loader used by `syslog_tls`) — both follow the same
/// "spawn_blocking off the reactor" pattern, but the output types
/// differ (tonic's wrapper vs raw `Arc<rustls::ServerConfig>`), so
/// they cannot share the loader function. Code reuse stops at
/// `TlsConfig`, which is the same struct in both call sites.
async fn load_tonic_tls_config(material: &TlsConfig) -> Result<ServerTlsConfig> {
    let cert_path = material.cert_path.clone();
    let key_path = material.key_path.clone();
    let ca_path = material.ca_path.clone();
    tokio::task::spawn_blocking(move || -> Result<ServerTlsConfig> {
        let cert_pem = std::fs::read(&cert_path)
            .with_context(|| format!("tls: cannot read cert {}", cert_path))?;
        let key_pem = std::fs::read(&key_path)
            .with_context(|| format!("tls: cannot read key {}", key_path))?;
        let identity = Identity::from_pem(cert_pem, key_pem);
        let mut config = ServerTlsConfig::new().identity(identity);
        if let Some(p) = ca_path {
            let ca_pem = std::fs::read(&p)
                .with_context(|| format!("tls: cannot read CA {}", p))?;
            config = config.client_ca_root(Certificate::from_pem(ca_pem));
        }
        Ok(config)
    })
    .await
    .context("otlp_grpc: TLS loader task panicked")?
}

struct LogsServiceImpl {
    tx: mpsc::Sender<Event>,
    metrics: Arc<InputMetrics>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

#[tonic::async_trait]
impl LogsService for LogsServiceImpl {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> std::result::Result<Response<ExportLogsServiceResponse>, Status> {
        // Track every RPC as a "received" event for backpressure
        // visibility — even if the body is empty / malformed we want
        // to see the flow through input metrics.
        self.metrics.events_received.fetch_add(1, Ordering::Relaxed);

        let peer = request.remote_addr().unwrap_or_else(|| {
            // Fallback: tonic should always know the peer for a
            // TCP-served RPC, but never panic on this corner.
            "0.0.0.0:0".parse().unwrap()
        });
        let req = request.into_inner();

        let outcome = split_request(
            req,
            peer,
            &self.metrics,
            &self.tx,
            "otlp_grpc",
            self.rate_limiter.as_deref(),
        )
        .await;
        if outcome.aborted {
            return Err(Status::unavailable("pipeline closed"));
        }
        if outcome.rejected > 0 {
            // The collector spec encourages partial-success replies so
            // senders can decide whether to retry just the rejects.
            return Ok(Response::new(ExportLogsServiceResponse {
                partial_success: Some(ExportLogsPartialSuccess {
                    rejected_log_records: outcome.rejected as i64,
                    error_message: "limpid: failed to re-encode some log records".into(),
                }),
            }));
        }
        Ok(Response::new(empty_response()))
    }
}

fn empty_response() -> ExportLogsServiceResponse {
    ExportLogsServiceResponse {
        partial_success: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{
        common::v1::InstrumentationScope,
        logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
        resource::v1::Resource,
    };
    use prost::Message;

    #[test]
    fn defaults_bind_address_no_rate_limit_no_tls() {
        let i = OtlpGrpcInput::from_properties("o", &[]).unwrap();
        assert_eq!(i.bind_addr, "0.0.0.0:4317");
        assert_eq!(i.rate_limit, None);
        assert!(i.tls.is_none());
    }

    #[test]
    fn rate_limit_property_round_trip() {
        let prop = Property::KeyValue {
            key: "rate_limit".into(),
            value: crate::dsl::ast::Expr::spanless(crate::dsl::ast::ExprKind::IntLit(2500)),
            value_span: None,
        };
        let i = OtlpGrpcInput::from_properties("o", &[prop]).unwrap();
        assert_eq!(i.rate_limit, Some(2500));
    }

    fn tls_block(cert: &str, key: &str, ca: Option<&str>) -> Property {
        let mut props = vec![
            Property::KeyValue {
                key: "cert".into(),
                value: crate::dsl::ast::Expr::spanless(crate::dsl::ast::ExprKind::StringLit(
                    cert.into(),
                )),
                value_span: None,
            },
            Property::KeyValue {
                key: "key".into(),
                value: crate::dsl::ast::Expr::spanless(crate::dsl::ast::ExprKind::StringLit(
                    key.into(),
                )),
                value_span: None,
            },
        ];
        if let Some(c) = ca {
            props.push(Property::KeyValue {
                key: "ca".into(),
                value: crate::dsl::ast::Expr::spanless(crate::dsl::ast::ExprKind::StringLit(
                    c.into(),
                )),
                value_span: None,
            });
        }
        Property::Block {
            key: "tls".into(),
            properties: props,
        }
    }

    #[test]
    fn tls_block_records_cert_key() {
        let props = vec![tls_block("/c.pem", "/k.pem", None)];
        let i = OtlpGrpcInput::from_properties("o", &props).unwrap();
        let tls = i.tls.expect("tls present");
        assert_eq!(tls.cert_path, "/c.pem");
        assert_eq!(tls.key_path, "/k.pem");
        assert!(tls.ca_path.is_none());
    }

    #[test]
    fn tls_block_records_ca_for_mtls() {
        let props = vec![tls_block("/c.pem", "/k.pem", Some("/ca.pem"))];
        let i = OtlpGrpcInput::from_properties("o", &props).unwrap();
        let tls = i.tls.expect("tls present");
        assert_eq!(tls.ca_path.as_deref(), Some("/ca.pem"));
    }

    #[test]
    fn tls_block_without_cert_is_rejected() {
        let props = vec![Property::Block {
            key: "tls".into(),
            properties: vec![Property::KeyValue {
                key: "key".into(),
                value: crate::dsl::ast::Expr::spanless(crate::dsl::ast::ExprKind::StringLit(
                    "/k.pem".into(),
                )),
                value_span: None,
            }],
        }];
        let err = OtlpGrpcInput::from_properties("o", &props).err().unwrap();
        assert!(
            err.to_string().contains("tls block requires 'cert'"),
            "unexpected: {err}"
        );
    }

    #[tokio::test]
    async fn export_splits_one_record_into_one_event() {
        // Drive the service trait directly (no socket) so the test
        // exercises the splitting logic without taking up a port.
        let (tx, mut rx) = mpsc::channel(8);
        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::new(InputMetrics::default()),
            rate_limiter: None,
        };
        let req = Request::new(ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![],
                    dropped_attributes_count: 0,
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "test".into(),
                        version: "0".into(),
                        attributes: vec![],
                        dropped_attributes_count: 0,
                    }),
                    log_records: vec![LogRecord {
                        time_unix_nano: 42,
                        severity_number: 9,
                        severity_text: "INFO".into(),
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        });
        let resp = svc.export(req).await.unwrap();
        assert!(resp.into_inner().partial_success.is_none());
        let event = rx.recv().await.expect("event must have been emitted");
        let decoded = ResourceLogs::decode(&*event.ingress).unwrap();
        assert_eq!(decoded.scope_logs.len(), 1);
        assert_eq!(decoded.scope_logs[0].log_records.len(), 1);
        assert_eq!(decoded.scope_logs[0].log_records[0].time_unix_nano, 42);
    }

    #[tokio::test]
    async fn empty_request_is_a_noop() {
        let (tx, mut rx) = mpsc::channel(8);
        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::new(InputMetrics::default()),
            rate_limiter: None,
        };
        let req = Request::new(ExportLogsServiceRequest {
            resource_logs: vec![],
        });
        let resp = svc.export(req).await.unwrap();
        assert!(resp.into_inner().partial_success.is_none());
        // No event should have been emitted.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn multiple_records_split_into_multiple_events() {
        let (tx, mut rx) = mpsc::channel(8);
        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::new(InputMetrics::default()),
            rate_limiter: None,
        };
        let lr = |t: u64| LogRecord {
            time_unix_nano: t,
            ..Default::default()
        };
        let req = Request::new(ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: None,
                scope_logs: vec![ScopeLogs {
                    scope: None,
                    log_records: vec![lr(1), lr(2), lr(3)],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        });
        svc.export(req).await.unwrap();
        for expected in [1u64, 2, 3] {
            let event = rx.recv().await.expect("event must be emitted");
            let decoded = ResourceLogs::decode(&*event.ingress).unwrap();
            assert_eq!(
                decoded.scope_logs[0].log_records[0].time_unix_nano,
                expected
            );
        }
    }
}

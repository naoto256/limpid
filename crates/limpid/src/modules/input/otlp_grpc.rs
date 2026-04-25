//! OTLP/gRPC input: receive logs via the OTLP/gRPC transport.
//!
//! Hosts the `opentelemetry.proto.collector.logs.v1.LogsService`
//! gRPC service. Each `Export` RPC arrives as an
//! `ExportLogsServiceRequest`; the handler splits it into one Event
//! per LogRecord and emits each as a singleton ResourceLogs
//! (1 Resource + 1 Scope + 1 LogRecord — the v0.5.0 OTLP hop contract,
//! identical to `otlp_http`). The reply is an empty
//! `ExportLogsServiceResponse` on success, or a `partial_success` with
//! `rejected_log_records` populated when re-encoding fails for some
//! records (rare).
//!
//! ## Configuration
//!
//! ```text
//! def input otlp_in {
//!     type otlp_grpc
//!     bind "0.0.0.0:4317"
//! }
//! ```
//!
//! TLS / mTLS for the server is queued for v0.5.x — currently the
//! input runs plaintext. For TLS termination today, front it with a
//! reverse proxy (nginx, envoy, traefik). The HTTP input has the same
//! shape, so the topology is symmetric.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::{
        ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
        logs_service_server::{LogsService, LogsServiceServer},
    },
    common::v1::InstrumentationScope,
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

pub struct OtlpGrpcInput {
    bind_addr: String,
    metrics: Arc<InputMetrics>,
}

impl Module for OtlpGrpcInput {
    fn from_properties(_name: &str, properties: &[Property]) -> Result<Self> {
        let bind = props::get_string(properties, "bind")
            .unwrap_or_else(|| "0.0.0.0:4317".to_string());
        Ok(Self {
            bind_addr: bind,
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
        info!("otlp_grpc listening on {}", addr);

        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::clone(&self.metrics),
        };
        let server = tonic::transport::Server::builder()
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

struct LogsServiceImpl {
    tx: mpsc::Sender<Event>,
    metrics: Arc<InputMetrics>,
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

        let total: usize = req
            .resource_logs
            .iter()
            .map(|rl| {
                rl.scope_logs
                    .iter()
                    .map(|sl| sl.log_records.len())
                    .sum::<usize>()
            })
            .sum();
        if total == 0 {
            return Ok(Response::new(empty_response()));
        }

        let mut sent = 0usize;
        for rl in &req.resource_logs {
            for sl in &rl.scope_logs {
                for lr in &sl.log_records {
                    let singleton = build_singleton(
                        rl.resource.clone(),
                        sl.scope.clone(),
                        lr.clone(),
                        rl.schema_url.clone(),
                        sl.schema_url.clone(),
                    );
                    let mut buf = Vec::with_capacity(singleton.encoded_len());
                    if let Err(e) = singleton.encode(&mut buf) {
                        warn!("otlp_grpc [{peer}]: re-encode failed: {e}");
                        self.metrics
                            .events_invalid
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let event = Event::new(Bytes::from(buf), peer);
                    if self.tx.send(event).await.is_err() {
                        // Receiver dropped — pipeline shutting down.
                        return Err(Status::unavailable("pipeline closed"));
                    }
                    sent += 1;
                }
            }
        }

        let rejected = total.saturating_sub(sent);
        if rejected > 0 {
            // The collector spec encourages partial-success replies so
            // senders can decide whether to retry just the rejects.
            // We only reject on encode failure (very rare); surface
            // it.
            return Ok(Response::new(ExportLogsServiceResponse {
                partial_success: Some(ExportLogsPartialSuccess {
                    rejected_log_records: rejected as i64,
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

/// Same shape as the HTTP input's helper: collapse `(R, S, L)` into a
/// singleton ResourceLogs (1 Resource + 1 Scope + 1 LogRecord).
fn build_singleton(
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    log_record: LogRecord,
    resource_schema_url: String,
    scope_schema_url: String,
) -> ResourceLogs {
    ResourceLogs {
        resource,
        scope_logs: vec![ScopeLogs {
            scope,
            log_records: vec![log_record],
            schema_url: scope_schema_url,
        }],
        schema_url: resource_schema_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_address() {
        let i = OtlpGrpcInput::from_properties("o", &[]).unwrap();
        assert_eq!(i.bind_addr, "0.0.0.0:4317");
    }

    #[tokio::test]
    async fn export_splits_one_record_into_one_event() {
        // Drive the service trait directly (no socket) so the test
        // exercises the splitting logic without taking up a port.
        let (tx, mut rx) = mpsc::channel(8);
        let svc = LogsServiceImpl {
            tx,
            metrics: Arc::new(InputMetrics::default()),
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

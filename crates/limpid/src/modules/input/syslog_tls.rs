//! Syslog TCP+TLS input: TLS-encrypted syslog reception.
//! Reuses TCP framing logic after TLS handshake.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::modules::{HasMetrics, Input, Module, ModuleSchema};
use crate::tls::TlsConfig;

use super::rate_limit::RateLimiter;
use super::syslog_tcp::TcpFraming;

/// Syslog TCP+TLS input: listens on a TCP socket with TLS encryption.
///
/// After TLS handshake, uses the same framing logic as plain TCP
/// (octet counting or non-transparent framing per RFC 6587).
pub struct SyslogTlsInput {
    pub bind_addr: String,
    pub framing: TcpFraming,
    pub tls_config: TlsConfig,
    pub rate_limit: Option<u64>,
    pub max_connections: usize,
    metrics: Arc<crate::metrics::InputMetrics>,
}

impl Module for SyslogTlsInput {
    fn schema() -> ModuleSchema {
        ModuleSchema::default()
    }

    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let bind =
            props::get_string(properties, "bind").unwrap_or_else(|| "0.0.0.0:6514".to_string());
        let framing = match props::get_ident(properties, "framing").as_deref() {
            Some("octet_counting") => TcpFraming::OctetCounting,
            Some("non_transparent") => TcpFraming::NonTransparent,
            _ => TcpFraming::Auto,
        };
        let tls_block = props::get_block(properties, "tls")
            .ok_or_else(|| anyhow::anyhow!("input '{}': syslog_tls requires 'tls' block", name))?;
        let cert = props::get_string(tls_block, "cert")
            .ok_or_else(|| anyhow::anyhow!("input '{}': tls requires 'cert'", name))?;
        let key = props::get_string(tls_block, "key")
            .ok_or_else(|| anyhow::anyhow!("input '{}': tls requires 'key'", name))?;
        let ca = props::get_string(tls_block, "ca");
        let rate_limit = props::get_strictly_positive_int(properties, "rate_limit")?;
        let max_connections = props::get_positive_int(properties, "max_connections")?
            .unwrap_or(super::syslog_tcp::DEFAULT_MAX_CONNECTIONS)
            as usize;

        Ok(Self {
            bind_addr: bind,
            framing,
            tls_config: TlsConfig {
                cert_path: cert,
                key_path: key,
                ca_path: ca,
            },
            rate_limit,
            max_connections,
            metrics: Arc::new(crate::metrics::InputMetrics::default()),
        })
    }
}

impl HasMetrics for SyslogTlsInput {
    type Stats = crate::metrics::InputMetrics;
    fn metrics(&self) -> Arc<crate::metrics::InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for SyslogTlsInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let server_config = crate::tls::build_server_config(&self.tls_config).await?;
        let acceptor = TlsAcceptor::from(server_config);

        let listener = TcpListener::bind(&self.bind_addr).await?;
        info!("syslog_tls listening on {}", self.bind_addr);

        let limiter: Option<Arc<RateLimiter>> = self.rate_limit.map(|r| {
            info!("syslog_tls rate_limit: {} events/sec", r);
            Arc::new(RateLimiter::new(r))
        });

        let mut conn_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        loop {
            conn_handles.retain(|h| !h.is_finished());

            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("syslog_tls: shutting down ({} active connections)", conn_handles.len());
                        for h in &conn_handles {
                            h.abort();
                        }
                        break;
                    }
                }

                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            if conn_handles.len() >= self.max_connections {
                                tracing::warn!("syslog_tls: max connections ({}) reached, rejecting {}", self.max_connections, addr);
                                drop(stream);
                                continue;
                            }
                            let acceptor = acceptor.clone();
                            let tx = tx.clone();
                            let framing = self.framing;
                            let limiter = limiter.clone();
                            let metrics = Arc::clone(&self.metrics);
                            debug!("syslog_tls: new connection from {}", addr);

                            conn_handles.push(tokio::spawn(async move {
                                let tls_stream = match acceptor.accept(stream).await {
                                    Ok(s) => s,
                                    Err(e) => {
                                        warn!("syslog_tls [{}]: TLS handshake failed: {}", addr, e);
                                        return;
                                    }
                                };

                                debug!("syslog_tls [{}]: TLS handshake complete", addr);

                                let mut reader = BufReader::new(tls_stream);

                                let effective_framing = if framing == TcpFraming::Auto {
                                    match super::syslog_tcp::detect_framing(&mut reader, addr).await {
                                        Some(f) => f,
                                        None => return,
                                    }
                                } else {
                                    framing
                                };

                                match effective_framing {
                                    TcpFraming::OctetCounting => {
                                        super::syslog_tcp::read_octet_counting(&mut reader, addr, &tx, limiter.as_deref(), Some(&metrics)).await;
                                    }
                                    TcpFraming::NonTransparent => {
                                        super::syslog_tcp::read_non_transparent(&mut reader, addr, &tx, limiter.as_deref(), Some(&metrics)).await;
                                    }
                                    TcpFraming::Auto => unreachable!(),
                                };
                            }));
                        }
                        Err(e) => {
                            error!("syslog_tls accept error: {}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

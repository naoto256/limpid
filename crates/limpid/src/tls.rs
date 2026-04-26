//! TLS configuration for TCP-based inputs and outputs.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ServerConfig};

use crate::dsl::ast::Property;
use crate::dsl::props;

/// TLS settings parsed from DSL `tls { ... }` block.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
    /// CA cert for client verification. None = no client auth.
    pub ca_path: Option<String>,
}

impl TlsConfig {
    /// Parse the optional `tls { cert key ca }` block off a module's
    /// property list. Returns `Ok(None)` when no block is present so
    /// callers can branch on plaintext vs TLS, and a clear error when
    /// the block exists but is missing required fields. The single
    /// implementation keeps error wording consistent across every
    /// module that accepts the same block (syslog_tls, otlp_grpc, …).
    pub fn from_properties_block(
        module_name: &str,
        properties: &[Property],
    ) -> Result<Option<Self>> {
        let Some(block) = props::get_block(properties, "tls") else {
            return Ok(None);
        };
        let cert_path = props::get_string(block, "cert")
            .ok_or_else(|| anyhow::anyhow!("'{}': tls block requires 'cert'", module_name))?;
        let key_path = props::get_string(block, "key")
            .ok_or_else(|| anyhow::anyhow!("'{}': tls block requires 'key'", module_name))?;
        let ca_path = props::get_string(block, "ca");
        Ok(Some(TlsConfig {
            cert_path,
            key_path,
            ca_path,
        }))
    }
}

/// Install the default rustls `CryptoProvider` (aws-lc-rs) once per
/// process. rustls 0.23 forces explicit selection; both the OTLP gRPC
/// input (server-side TLS) and output (client-side TLS) need it before
/// the first handshake. Idempotent — gated by a `Once`, and
/// `install_default` itself silently no-ops when a provider is already
/// installed (e.g. by reqwest), so multiple call sites are safe.
pub fn install_default_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Build a rustls ServerConfig for a TLS-enabled TCP input.
///
/// File I/O is offloaded to `spawn_blocking` so we don't stall the tokio
/// reactor thread on slow disks (NFS, EBS, etc.) during startup.
pub async fn build_server_config(tls: &TlsConfig) -> Result<Arc<ServerConfig>> {
    let cert_path = tls.cert_path.clone();
    let key_path = tls.key_path.clone();
    let ca_path = tls.ca_path.clone();

    tokio::task::spawn_blocking(move || build_server_config_sync(&cert_path, &key_path, ca_path))
        .await
        .context("tls: cert/key loader task panicked")?
}

fn build_server_config_sync(
    cert_path: &str,
    key_path: &str,
    ca_path: Option<String>,
) -> Result<Arc<ServerConfig>> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let config = if let Some(ref ca_path) = ca_path {
        // Client certificate verification enabled
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store.add(cert).context("failed to add CA cert")?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .context("failed to build client verifier")?;
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("failed to build TLS server config with client auth")?
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("failed to build TLS server config")?
    };

    Ok(Arc::new(config))
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read cert file: {}", path))?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certs from: {}", path))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in: {}", path);
    }
    Ok(certs)
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read key file: {}", path))?;
    let key = PrivateKeyDer::from_pem_slice(&bytes)
        .with_context(|| format!("failed to parse key from: {}", path))?;
    Ok(key)
}

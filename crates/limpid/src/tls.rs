//! TLS configuration for TCP-based inputs and outputs.

use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ServerConfig};

/// TLS settings parsed from DSL `tls { ... }` block.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
    /// CA cert for client verification. None = no client auth.
    pub ca_path: Option<String>,
}

/// Build a rustls ServerConfig for a TLS-enabled TCP input.
pub fn build_server_config(tls: &TlsConfig) -> Result<Arc<ServerConfig>> {
    let certs = load_certs(&tls.cert_path)?;
    let key = load_private_key(&tls.key_path)?;

    let config = if let Some(ref ca_path) = tls.ca_path {
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
    let file = std::fs::File::open(Path::new(path))
        .with_context(|| format!("failed to open cert file: {}", path))?;
    let reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_reader_iter(reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certs from: {}", path))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in: {}", path);
    }
    Ok(certs)
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(Path::new(path))
        .with_context(|| format!("failed to open key file: {}", path))?;
    let reader = BufReader::new(file);
    let key = PrivateKeyDer::from_pem_reader(reader)
        .with_context(|| format!("failed to parse key from: {}", path))?;
    Ok(key)
}

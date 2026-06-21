//! TLS configuration for TCP-based inputs and outputs.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};

// ---------------------------------------------------------------------------
// Declarative TLS sub-block schemas
// ---------------------------------------------------------------------------
//
// Every Module that exposes a `tls { ... }` block shares the same
// inner key set: `cert`, `key`, `ca`. The only thing that differs is
// which of those are required, and that depends on the role:
//
//   - **Server role** (TLS-terminating listeners like `syslog_tcp`
//     (with optional `tls` block), `otlp_grpc`, `otlp_http`,
//     the future `input limpid`): the server presents a
//     certificate, so `cert` and `key` are required. `ca` is optional
//     and used for client-cert verification (mTLS).
//
//   - **Client role** (sinks like `http`, `otlp` output, the future
//     `output limpid`): in the pre-mTLS world only `ca` is meaningful,
//     and `cert` / `key` aren't accepted by today's HTTP / gRPC
//     transports. The v0.8.0 `output limpid` will lift that and accept
//     all three (mTLS client auth); the schema already allows them as
//     optional fields, so adding mTLS to clients won't require touching
//     the schema layer.
//
// Both schemas live here next to `TlsConfig::from_properties_block` so
// the parser and the schema description can't drift. Per-Module
// required-ness for the *outer* `tls` property (whole block required
// vs optional) stays with each Module — that's a transport-level
// decision, not a TLS-block-internal one.

const TLS_BLOCK_CERT_KEY_CA_REQUIRED: &[PropertySpec] = &[
    PropertySpec {
        name: "cert",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "key",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "ca",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

const TLS_BLOCK_CA_ONLY: &[PropertySpec] = &[PropertySpec {
    name: "ca",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::String,
}];

/// Shared schema for the `tls { cert | key | ca }` block used by
/// TLS-terminating server Modules (`syslog_tcp`, `otlp_grpc`,
/// `otlp_http`, the future `input limpid`). `cert` and `key` are
/// mandatory; `ca` enables mTLS client-certificate verification.
pub const TLS_SERVER_BLOCK_PROPERTIES: &[PropertySpec] = TLS_BLOCK_CERT_KEY_CA_REQUIRED;

/// Shared schema for the `tls { ca }` block used by client Modules
/// (`output http`, `output otlp`) that only need to add a custom CA
/// to their trust store. When v0.8.0 lands `output limpid` with mTLS
/// the schema can switch to [`TLS_SERVER_BLOCK_PROPERTIES`] (same
/// shape, with `cert` / `key` becoming optional) without forcing a
/// breaking config change for the simpler clients.
pub const TLS_CLIENT_BLOCK_PROPERTIES: &[PropertySpec] = TLS_BLOCK_CA_ONLY;

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
    /// module that accepts the same block (syslog_tcp, otlp_grpc, …).
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

/// TLS settings for client-side transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTlsConfig {
    pub ca_path: Option<String>,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

impl ClientTlsConfig {
    pub fn validate(&self, label: &str) -> Result<()> {
        if self.cert_path.is_some() != self.key_path.is_some() {
            anyhow::bail!("{}: tls cert and key must be specified together", label);
        }
        Ok(())
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

/// Build a rustls ClientConfig for a TLS-enabled output.
pub fn build_client_config_sync(tls: &ClientTlsConfig) -> Result<Arc<ClientConfig>> {
    install_default_crypto_provider();

    let root_store = if let Some(ref ca_path) = tls.ca_path {
        let ca_certs = load_certs(ca_path)?;
        let mut root_store = RootCertStore::empty();
        for cert in ca_certs {
            root_store.add(cert).context("failed to add CA cert")?;
        }
        root_store
    } else {
        RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
    };

    let builder = ClientConfig::builder().with_root_certificates(root_store);
    let config = match (&tls.cert_path, &tls.key_path) {
        (Some(cert_path), Some(key_path)) => builder
            .with_client_auth_cert(load_certs(cert_path)?, load_private_key(key_path)?)
            .context("failed to build TLS client config with client auth")?,
        (None, None) => builder.with_no_client_auth(),
        _ => anyhow::bail!("tls cert and key must be specified together"),
    };

    Ok(Arc::new(config))
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

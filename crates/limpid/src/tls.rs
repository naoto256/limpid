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

const TLS_BLOCK_CERT_KEY_CA_OPTIONAL: &[PropertySpec] = &[
    PropertySpec {
        name: "ca",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "cert",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "key",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

/// Shared schema for the `tls { cert | key | ca }` block used by
/// TLS-terminating server Modules (`syslog_tcp`, `otlp_grpc`,
/// `otlp_http`, the future `input limpid`). `cert` and `key` are
/// mandatory; `ca` enables mTLS client-certificate verification.
pub const TLS_SERVER_BLOCK_PROPERTIES: &[PropertySpec] = TLS_BLOCK_CERT_KEY_CA_REQUIRED;

/// Shared schema for the `tls { ca | cert | key }` block used by
/// client Modules (`output syslog_tcp` per-peer, `output kafka`,
/// `output http`, `output otlp_http`, `output otlp_grpc`). All three
/// keys are optional at the schema layer; the cert↔key paired
/// invariant is enforced at parse time by
/// [`ClientTlsConfig::validate`]. `ca` alone is a custom CA, `ca` +
/// `cert` + `key` is mTLS; empty-block handling is module-specific
/// (most callers accept it as "use system CA roots, no client identity",
/// but a module is free to layer additional rejection rules on top —
/// e.g. `output otlp_http` rejects any `tls` block on a plaintext
/// endpoint, regardless of contents).
pub const TLS_CLIENT_BLOCK_PROPERTIES: &[PropertySpec] = TLS_BLOCK_CERT_KEY_CA_OPTIONAL;

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

/// TLS protocol versions accepted by every rustls config limpid
/// builds (server listeners and client sinks alike).
///
/// This is an *explicit pin* of what rustls 0.23 already defaults to:
/// the crate only implements TLS 1.2 and 1.3, and `DEFAULT_VERSIONS`
/// enables both — pinning changes no behaviour today. It exists so the
/// "≥ TLS 1.2" floor is a stated property of limpid's configuration
/// rather than an inherited library default: rustls documents that a
/// future release may change `DEFAULT_VERSIONS`, and any such change
/// should be a deliberate edit here, not a silent inheritance.
/// The reqwest-based sinks (`output http`, `output otlp_http`) don't
/// go through this module's builders; they pin the same floor via
/// `ClientBuilder::min_tls_version`. (Security audit Low 4-3.)
static TLS_PROTOCOL_VERSIONS: &[&rustls::SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

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

    let builder = ClientConfig::builder_with_protocol_versions(TLS_PROTOCOL_VERSIONS)
        .with_root_certificates(root_store);
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
    // The client path installs the provider before building; do the
    // same here so `builder_with_protocol_versions` (which reads the
    // process-default provider) can't race a bare startup sequence.
    install_default_crypto_provider();

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
        ServerConfig::builder_with_protocol_versions(TLS_PROTOCOL_VERSIONS)
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("failed to build TLS server config with client auth")?
    } else {
        ServerConfig::builder_with_protocol_versions(TLS_PROTOCOL_VERSIONS)
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
    // Warn (don't refuse) on a group/other-readable key file: refusing
    // would brick existing deployments on upgrade, but a private key
    // readable by other local users defeats the point of TLS/mTLS.
    // (Security audit Low 4-3.)
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(path)
            && key_mode_is_overshared(meta.mode())
        {
            tracing::warn!(
                "tls: private key {} is readable by group/other (mode {:o}) — \
                 restrict with `chmod 600 {}`",
                path,
                meta.mode() & 0o777,
                path
            );
        }
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read key file: {}", path))?;
    let key = PrivateKeyDer::from_pem_slice(&bytes)
        .with_context(|| format!("failed to parse key from: {}", path))?;
    Ok(key)
}

/// True when a private-key file's mode grants read access beyond its
/// owner (group-read 0o040 or other-read 0o004). Split from
/// `load_private_key` so the predicate is unit-testable without
/// capturing tracing output.
#[cfg(unix)]
fn key_mode_is_overshared(mode: u32) -> bool {
    mode & 0o044 != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct PemSet {
        _dir: TempDir,
        cert: String,
        key: String,
        ca_cert: String,
        // Independent cert that does NOT match `key` — used to force
        // rustls's cert↔key consistency check to reject.
        mismatched_cert: String,
    }

    fn gen_pems() -> PemSet {
        let dir = TempDir::new().unwrap();
        // Primary cert + key + CA.
        let key_pair = rcgen::KeyPair::generate().expect("key gen");
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

        // Separate self-signed cert used both as the CA root and as a
        // mismatched-cert source for the consistency-check test. The
        // path is the same file; consumers pick whichever role.
        let ca_kp = rcgen::KeyPair::generate().unwrap();
        let ca_params = rcgen::CertificateParams::new(vec!["alt".to_string()]).unwrap();
        let ca_cert = ca_params.self_signed(&ca_kp).unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();

        PemSet {
            cert: cert_path.display().to_string(),
            key: key_path.display().to_string(),
            ca_cert: ca_path.display().to_string(),
            mismatched_cert: ca_path.display().to_string(),
            _dir: dir,
        }
    }

    // ---------- build_server_config ----------

    #[tokio::test]
    async fn server_config_without_ca_skips_client_auth() {
        let p = gen_pems();
        let cfg = TlsConfig {
            cert_path: p.cert.clone(),
            key_path: p.key.clone(),
            ca_path: None,
        };
        let server_cfg = build_server_config(&cfg).await.expect("build ok");
        assert!(Arc::strong_count(&server_cfg) >= 1);
    }

    #[tokio::test]
    async fn server_config_with_ca_enables_client_auth() {
        let p = gen_pems();
        let cfg = TlsConfig {
            cert_path: p.cert.clone(),
            key_path: p.key.clone(),
            ca_path: Some(p.ca_cert.clone()),
        };
        let server_cfg = build_server_config(&cfg).await.expect("build ok");
        assert!(Arc::strong_count(&server_cfg) >= 1);
    }

    #[tokio::test]
    async fn server_config_rejects_missing_cert_file() {
        let p = gen_pems();
        let cfg = TlsConfig {
            cert_path: "/nonexistent/cert.pem".into(),
            key_path: p.key.clone(),
            ca_path: None,
        };
        let err = build_server_config(&cfg).await.expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("cert") || msg.contains("read"),
            "expected cert-read error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn server_config_rejects_missing_key_file() {
        let p = gen_pems();
        let cfg = TlsConfig {
            cert_path: p.cert.clone(),
            key_path: "/nonexistent/key.pem".into(),
            ca_path: None,
        };
        let err = build_server_config(&cfg).await.expect_err("must fail");
        assert!(err.to_string().contains("key") || err.to_string().contains("read"));
    }

    #[tokio::test]
    async fn server_config_rejects_mismatched_cert_and_key() {
        // Hand rustls a cert whose subject public key does NOT match
        // the configured private key. rustls's `with_single_cert` is
        // documented to verify this pairing; a regression that swapped
        // it for a non-checking variant would let TLS handshake fail
        // at runtime instead of config-load.
        let p = gen_pems();
        let cfg = TlsConfig {
            cert_path: p.mismatched_cert.clone(),
            key_path: p.key.clone(),
            ca_path: None,
        };
        let err = build_server_config(&cfg).await.expect_err("must fail");
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("server config")
                || err.to_string().to_ascii_lowercase().contains("key"),
            "expected pairing-rejection error, got: {err}"
        );
    }

    // ---------- build_client_config_sync ----------

    #[test]
    fn client_config_no_ca_no_identity_uses_webpki_roots() {
        let cfg = ClientTlsConfig {
            ca_path: None,
            cert_path: None,
            key_path: None,
        };
        let _ = build_client_config_sync(&cfg).expect("build ok");
    }

    #[test]
    fn client_config_with_custom_ca() {
        let p = gen_pems();
        let cfg = ClientTlsConfig {
            ca_path: Some(p.ca_cert.clone()),
            cert_path: None,
            key_path: None,
        };
        let _ = build_client_config_sync(&cfg).expect("build ok");
    }

    #[test]
    fn client_config_with_mtls_identity() {
        let p = gen_pems();
        let cfg = ClientTlsConfig {
            ca_path: Some(p.ca_cert.clone()),
            cert_path: Some(p.cert.clone()),
            key_path: Some(p.key.clone()),
        };
        let _ = build_client_config_sync(&cfg).expect("build ok");
    }

    #[test]
    fn client_config_rejects_cert_without_key() {
        let p = gen_pems();
        let cfg = ClientTlsConfig {
            ca_path: None,
            cert_path: Some(p.cert.clone()),
            key_path: None,
        };
        let err = build_client_config_sync(&cfg).expect_err("must fail");
        assert!(err.to_string().contains("cert and key"), "got: {err}");
    }

    #[test]
    fn client_config_rejects_key_without_cert() {
        let p = gen_pems();
        let cfg = ClientTlsConfig {
            ca_path: None,
            cert_path: None,
            key_path: Some(p.key.clone()),
        };
        let err = build_client_config_sync(&cfg).expect_err("must fail");
        assert!(err.to_string().contains("cert and key"));
    }

    #[test]
    fn client_config_rejects_unreadable_ca() {
        let cfg = ClientTlsConfig {
            ca_path: Some("/nonexistent/ca.pem".into()),
            cert_path: None,
            key_path: None,
        };
        let err = build_client_config_sync(&cfg).expect_err("must fail");
        assert!(
            err.to_string().contains("read") || err.to_string().contains("cert"),
            "got: {err}"
        );
    }

    // ---------- key file permission check ----------

    #[test]
    #[cfg(unix)]
    fn key_mode_predicate_flags_group_or_other_read() {
        // Overshared: any read bit beyond the owner.
        assert!(key_mode_is_overshared(0o644)); // group+other read
        assert!(key_mode_is_overshared(0o640)); // group read
        assert!(key_mode_is_overshared(0o604)); // other read
        assert!(key_mode_is_overshared(0o444));
        // Owner-only shapes are fine, including the write bit.
        assert!(!key_mode_is_overshared(0o600));
        assert!(!key_mode_is_overshared(0o400));
        // Group/other *write without read* is a different (weirder)
        // misconfiguration; the read-exposure warning stays scoped to
        // confidentiality of the key material.
        assert!(!key_mode_is_overshared(0o622));
    }

    #[test]
    #[cfg(unix)]
    fn load_private_key_still_loads_overshared_key() {
        // The permission check must warn, never refuse — refusing
        // would brick existing deployments on upgrade. Pin the
        // load-succeeds behaviour for a 0644 key.
        use std::os::unix::fs::PermissionsExt;
        let p = gen_pems();
        std::fs::set_permissions(&p.key, std::fs::Permissions::from_mode(0o644)).unwrap();
        load_private_key(&p.key).expect("overshared key must still load (warn-only check)");
    }

    // ---------- protocol version pin ----------

    #[test]
    fn protocol_version_pin_is_tls12_floor() {
        // Compile-visible statement of the audit requirement: the pin
        // must contain TLS 1.2 and 1.3 and nothing older. rustls 0.23
        // cannot even express < 1.2, so this is a tripwire against a
        // future edit that drops 1.2 (breaking older peers silently)
        // or a rustls upgrade that changes what the constant means.
        let versions: Vec<_> = TLS_PROTOCOL_VERSIONS.iter().map(|v| v.version).collect();
        assert!(versions.contains(&rustls::ProtocolVersion::TLSv1_2));
        assert!(versions.contains(&rustls::ProtocolVersion::TLSv1_3));
        assert_eq!(versions.len(), 2);
    }
}

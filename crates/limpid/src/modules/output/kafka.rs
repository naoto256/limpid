//! Kafka output: produces event messages to an Apache Kafka topic.
//!
//! Uses librdkafka via the rdkafka crate. The producer handles batching,
//! compression, retries, and connection management internally — so unlike
//! the syslog / http / otlp outputs there is no per-peer rotation layer
//! on top: `brokers` is the bootstrap list and librdkafka resolves the
//! cluster from there.
//!
//! Properties:
//!   brokers   "kafka1:9092,kafka2:9092"   — required
//!   topic     "syslog-events"             — required
//!   compression  snappy                   — optional (none, gzip, snappy, lz4, zstd)
//!   acks      all                         — optional (0, 1, all; default: all)
//!   key       source                      — optional (event field to use as partition key)
//!   queue_timeout "5s"                    — optional
//!   tls  { ca; cert; key }                — optional (TLS to brokers; mTLS if cert+key)
//!   sasl { mechanism; username; password_file }
//!                                          — optional (SASL/PLAIN or SCRAM)
//!
//! `security.protocol` is derived from the blocks present:
//! - neither      → plaintext (librdkafka default)
//! - `tls` only   → ssl
//! - `sasl` only  → sasl_plaintext
//! - both         → sasl_ssl
//!
//! Secrets handling: `key` (PEM private key path) and `password_file`
//! both point to **separate files** rather than inline strings, matching
//! the mTLS convention used elsewhere in limpid. Use chmod 600 on those
//! files; the daemon refuses to run as root, so the config user must
//! own them.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::tls::ClientTlsConfig;

/// All three fields are optional at the schema layer; the invariant
/// (cert ↔ key paired) is enforced at parse time via
/// [`ClientTlsConfig::validate`].
const KAFKA_TLS_BLOCK_PROPERTIES: &[PropertySpec] = &[
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

/// Supported SASL mechanisms. The DSL spelling is underscore-separated
/// (`scram_sha_256`) to fit the DSL's ident grammar — `-` would
/// tokenise as subtraction. They're mapped to librdkafka's canonical
/// hyphen-separated spelling (`SCRAM-SHA-256`) at parse time.
const KAFKA_SASL_MECHANISMS: &[&str] = &["plain", "scram_sha_256", "scram_sha_512"];

/// SASL block schema. All three keys are required when the block is
/// present so a typo can't silently downgrade auth.
///
/// `password_file` (not `password`) is the only supported shape —
/// inline `password` would put cleartext credentials in the config
/// itself, which gets committed / backed up / logged. The file at
/// `password_file` is read once at startup; rotate it and restart
/// the daemon to refresh.
const KAFKA_SASL_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "mechanism",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(KAFKA_SASL_MECHANISMS),
    },
    PropertySpec {
        name: "username",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "password_file",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

const KAFKA_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "brokers",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "topic",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "compression",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["none", "gzip", "snappy", "lz4", "zstd"]),
    },
    PropertySpec {
        name: "acks",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["0", "1", "all"]),
    },
    // `key` accepts the magic value `source` or any user-chosen
    // field name (`workspace.tenant` etc.). String, not Enum, since
    // the field-name half is open.
    PropertySpec {
        name: "key",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "queue_timeout",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(KAFKA_TLS_BLOCK_PROPERTIES),
    },
    PropertySpec {
        name: "sasl",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(KAFKA_SASL_BLOCK_PROPERTIES),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

struct SaslConfig {
    /// rdkafka-spelled mechanism (`PLAIN` / `SCRAM-SHA-256` / …).
    mechanism: String,
    username: String,
    password: String,
}

fn parse_tls_block(name: &str, properties: &[Property]) -> Result<Option<ClientTlsConfig>> {
    let Some(block) = props::get_block(properties, "tls") else {
        return Ok(None);
    };
    let tls = ClientTlsConfig {
        ca_path: props::get_string(block, "ca"),
        cert_path: props::get_string(block, "cert"),
        key_path: props::get_string(block, "key"),
    };
    tls.validate(&format!("output '{}'", name))?;
    Ok(Some(tls))
}

fn parse_sasl_block(name: &str, properties: &[Property]) -> Result<Option<SaslConfig>> {
    let Some(block) = props::get_block(properties, "sasl") else {
        return Ok(None);
    };
    let mechanism = props::get_ident(block, "mechanism").ok_or_else(|| {
        anyhow::anyhow!("output '{}': sasl block requires 'mechanism'", name)
    })?;
    let username = props::get_string(block, "username").ok_or_else(|| {
        anyhow::anyhow!("output '{}': sasl block requires 'username'", name)
    })?;
    let password_file = props::get_string(block, "password_file").ok_or_else(|| {
        anyhow::anyhow!("output '{}': sasl block requires 'password_file'", name)
    })?;

    let raw = std::fs::read_to_string(&password_file).with_context(|| {
        format!(
            "output '{}': failed to read sasl password_file '{}'",
            name, password_file
        )
    })?;
    // Strip a single trailing newline (the common case for
    // `echo "secret" > pw`). Empty file or whitespace-only file is
    // probably a misconfigured secret, not a deliberate empty
    // password, so reject it.
    let password = raw.strip_suffix('\n').unwrap_or(&raw).to_string();
    if password.trim().is_empty() {
        anyhow::bail!(
            "output '{}': sasl password_file '{}' is empty",
            name,
            password_file
        );
    }

    // Map DSL underscore-spelling → librdkafka hyphen-spelling.
    let mechanism_upper = match mechanism.as_str() {
        "plain" => "PLAIN",
        "scram_sha_256" => "SCRAM-SHA-256",
        "scram_sha_512" => "SCRAM-SHA-512",
        other => {
            anyhow::bail!(
                "output '{}': unsupported sasl mechanism '{}' (expected one of {:?})",
                name,
                other,
                KAFKA_SASL_MECHANISMS
            );
        }
    };

    Ok(Some(SaslConfig {
        mechanism: mechanism_upper.to_string(),
        username,
        password,
    }))
}

/// Pick librdkafka's `security.protocol` from which of (tls, sasl)
/// are configured. Returns `None` for the "no extra protocol layer"
/// case so the caller can simply skip the setting (librdkafka
/// defaults to plaintext).
fn security_protocol(has_tls: bool, has_sasl: bool) -> Option<&'static str> {
    match (has_tls, has_sasl) {
        (false, false) => None,
        (true, false) => Some("ssl"),
        (false, true) => Some("sasl_plaintext"),
        (true, true) => Some("sasl_ssl"),
    }
}

struct KafkaPayload {
    egress: Bytes,
    key: Option<String>,
}

pub struct KafkaOutput {
    producer: FutureProducer,
    topic: String,
    key_field: Option<KeyField>,
    queue_timeout: Duration,
    metrics: Arc<OutputMetrics>,
}

/// Which event field to use as the Kafka partition key.
#[derive(Debug, Clone)]
enum KeyField {
    Source,
    Field(String),
}

impl Module for KafkaOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(KAFKA_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let brokers = props::get_string(properties, "brokers")
            .ok_or_else(|| anyhow::anyhow!("output '{}': kafka requires 'brokers'", name))?;
        let topic = props::get_string(properties, "topic")
            .ok_or_else(|| anyhow::anyhow!("output '{}': kafka requires 'topic'", name))?;

        let compression =
            props::get_ident(properties, "compression").unwrap_or_else(|| "none".to_string());
        if !matches!(
            compression.as_str(),
            "none" | "gzip" | "snappy" | "lz4" | "zstd"
        ) {
            anyhow::bail!(
                "output '{}': invalid compression '{}' (expected: none, gzip, snappy, lz4, zstd)",
                name,
                compression
            );
        }

        let acks = props::get_ident(properties, "acks").unwrap_or_else(|| "all".to_string());
        if !matches!(acks.as_str(), "0" | "1" | "all") {
            anyhow::bail!(
                "output '{}': invalid acks '{}' (expected: 0, 1, all)",
                name,
                acks
            );
        }

        let queue_timeout = match props::get_string(properties, "queue_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };

        let key_field = props::get_ident(properties, "key").map(|k| match k.as_str() {
            "source" => KeyField::Source,
            other => KeyField::Field(other.to_string()),
        });

        let tls = parse_tls_block(name, properties)?;
        let sasl = parse_sasl_block(name, properties)?;

        // message.timeout.ms: rdkafka's internal delivery timeout (includes retries to broker).
        // Separate from queue_timeout which is the wait time when the internal queue is full.
        // If delivery fails after this timeout, limpid's queue retry mechanism handles re-delivery.
        let mut client = ClientConfig::new();
        client
            .set("bootstrap.servers", &brokers)
            .set("compression.type", &compression)
            .set("acks", &acks)
            .set("message.timeout.ms", "30000");

        if let Some(protocol) = security_protocol(tls.is_some(), sasl.is_some()) {
            client.set("security.protocol", protocol);
        }
        if let Some(ref tls) = tls {
            if let Some(ref ca) = tls.ca_path {
                client.set("ssl.ca.location", ca);
            }
            if let (Some(cert), Some(key)) = (&tls.cert_path, &tls.key_path) {
                client.set("ssl.certificate.location", cert);
                client.set("ssl.key.location", key);
            }
        }
        if let Some(ref sasl) = sasl {
            client
                .set("sasl.mechanism", &sasl.mechanism)
                .set("sasl.username", &sasl.username)
                .set("sasl.password", &sasl.password);
        }

        let producer: FutureProducer = client
            .create()
            .with_context(|| format!("output '{}': failed to create Kafka producer", name))?;

        Ok(Self {
            producer,
            topic,
            key_field,
            queue_timeout,
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for KafkaOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for KafkaOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        let key = self.resolve_key_borrowed(event);
        Ok(RenderedPayload::new(KafkaPayload {
            egress: event.egress.clone(),
            key,
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: KafkaPayload = payload.downcast()?;
        let mut record = FutureRecord::to(&self.topic).payload(payload.egress.as_ref());
        if let Some(ref k) = payload.key {
            record = record.key(k);
        }

        self.producer
            .send(record, self.queue_timeout)
            .await
            .map_err(|(e, _)| anyhow::anyhow!("kafka produce failed: {}", e))?;

        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for KafkaOutput {
    fn drop(&mut self) {
        if let Err(e) = self.producer.flush(Duration::from_secs(5)) {
            tracing::warn!("kafka output: flush on shutdown failed: {}", e);
        }
    }
}

impl KafkaOutput {
    fn resolve_key_borrowed(&self, event: &BorrowedEvent<'_>) -> Option<String> {
        let kf = self.key_field.as_ref()?;
        let value = match kf {
            KeyField::Source => event.source.ip().to_string(),
            KeyField::Field(name) => event
                .workspace_get(name)
                .and_then(|v| v.as_str().map(|s| s.to_string()))?,
        };
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use tempfile::TempDir;

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties,
        }
    }

    fn str_prop(key: &str, val: &str) -> Property {
        kv(key, ExprKind::StringLit(val.into()))
    }

    fn ident_prop(key: &str, val: &str) -> Property {
        kv(key, ExprKind::Ident(vec![val.into()]))
    }

    // ---- security_protocol selector ----

    #[test]
    fn security_protocol_matrix() {
        assert_eq!(security_protocol(false, false), None);
        assert_eq!(security_protocol(true, false), Some("ssl"));
        assert_eq!(security_protocol(false, true), Some("sasl_plaintext"));
        assert_eq!(security_protocol(true, true), Some("sasl_ssl"));
    }

    // ---- parse_tls_block ----

    #[test]
    fn parse_tls_absent_returns_none() {
        let result = parse_tls_block("k", &[]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_tls_ca_only() {
        let props = vec![block("tls", vec![str_prop("ca", "/etc/ca.pem")])];
        let tls = parse_tls_block("k", &props).unwrap().unwrap();
        assert_eq!(tls.ca_path.as_deref(), Some("/etc/ca.pem"));
        assert!(tls.cert_path.is_none() && tls.key_path.is_none());
    }

    #[test]
    fn parse_tls_mtls_full() {
        let props = vec![block(
            "tls",
            vec![
                str_prop("ca", "/ca.pem"),
                str_prop("cert", "/c.pem"),
                str_prop("key", "/k.pem"),
            ],
        )];
        let tls = parse_tls_block("k", &props).unwrap().unwrap();
        assert_eq!(tls.ca_path.as_deref(), Some("/ca.pem"));
        assert_eq!(tls.cert_path.as_deref(), Some("/c.pem"));
        assert_eq!(tls.key_path.as_deref(), Some("/k.pem"));
    }

    #[test]
    fn parse_tls_cert_without_key_rejected() {
        let props = vec![block("tls", vec![str_prop("cert", "/c.pem")])];
        let err = parse_tls_block("k", &props).err().unwrap();
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    #[test]
    fn parse_tls_key_without_cert_rejected() {
        let props = vec![block("tls", vec![str_prop("key", "/k.pem")])];
        let err = parse_tls_block("k", &props).err().unwrap();
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    // ---- parse_sasl_block ----

    struct PasswordFile {
        _dir: TempDir,
        path: String,
    }

    fn make_password_file(contents: &[u8]) -> PasswordFile {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kafka.pw");
        std::fs::write(&path, contents).unwrap();
        PasswordFile {
            _dir: dir,
            path: path.display().to_string(),
        }
    }

    #[test]
    fn parse_sasl_absent_returns_none() {
        assert!(parse_sasl_block("k", &[]).unwrap().is_none());
    }

    #[test]
    fn parse_sasl_plain_reads_password_file() {
        let pw = make_password_file(b"hunter2\n");
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "limpid"),
                str_prop("password_file", &pw.path),
            ],
        )];
        let sasl = parse_sasl_block("k", &props).unwrap().unwrap();
        assert_eq!(sasl.mechanism, "PLAIN");
        assert_eq!(sasl.username, "limpid");
        assert_eq!(sasl.password, "hunter2"); // trailing newline stripped
    }

    #[test]
    fn parse_sasl_scram_uppercases_mechanism() {
        let pw = make_password_file(b"secret");
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "scram_sha_512"),
                str_prop("username", "u"),
                str_prop("password_file", &pw.path),
            ],
        )];
        let sasl = parse_sasl_block("k", &props).unwrap().unwrap();
        // DSL underscore form → librdkafka hyphen form.
        assert_eq!(sasl.mechanism, "SCRAM-SHA-512");
    }

    #[test]
    fn parse_sasl_missing_password_file_errors() {
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "u"),
                str_prop("password_file", "/nonexistent/path/to/pw"),
            ],
        )];
        let err = parse_sasl_block("k", &props).err().unwrap();
        assert!(err.to_string().contains("password_file"), "{}", err);
    }

    #[test]
    fn parse_sasl_empty_password_file_rejected() {
        let pw = make_password_file(b"\n");
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "u"),
                str_prop("password_file", &pw.path),
            ],
        )];
        let err = parse_sasl_block("k", &props).err().unwrap();
        assert!(err.to_string().contains("empty"), "{}", err);
    }
}

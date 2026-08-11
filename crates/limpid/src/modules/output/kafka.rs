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
use std::time::Duration;

use anyhow::{Context, Result};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};
use crate::tls::ClientTlsConfig;

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
    // `key` accepts only the magic value `source` (= partition by
    // event source IP). Pipeline-mutable selectors like
    // `workspace.tenant` are rejected — see the bail in
    // `from_properties`. Kept as String rather than Enum so the
    // schema-level error message can match the runtime parse error
    // (`must be 'source'`) rather than splitting it across two
    // diagnostic categories.
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
        kind: PropertyValueKind::Block(crate::tls::TLS_CLIENT_BLOCK_PROPERTIES),
    },
    PropertySpec {
        name: "sasl",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(KAFKA_SASL_BLOCK_PROPERTIES),
    },
    crate::queue::RETRY_PROPERTY_SPEC,
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
    let mechanism = props::get_ident(block, "mechanism")
        .ok_or_else(|| anyhow::anyhow!("output '{}': sasl block requires 'mechanism'", name))?;
    let username = props::get_string(block, "username")
        .ok_or_else(|| anyhow::anyhow!("output '{}': sasl block requires 'username'", name))?;
    let password_file = props::get_string(block, "password_file")
        .ok_or_else(|| anyhow::anyhow!("output '{}': sasl block requires 'password_file'", name))?;

    let raw = std::fs::read_to_string(&password_file).with_context(|| {
        format!(
            "output '{}': failed to read sasl password_file '{}'",
            name, password_file
        )
    })?;
    // Strip a single trailing newline (the common case for
    // `echo "secret" > pw`), handling CRLF as well as bare LF so
    // password files written on Windows hosts authenticate correctly.
    // Empty file or whitespace-only file is probably a misconfigured
    // secret, not a deliberate empty password, so reject it.
    let password = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .or_else(|| raw.strip_suffix('\r'))
        .unwrap_or(&raw)
        .to_string();
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

fn plain_without_tls_error(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "output '{}': sasl mechanism 'plain' requires a tls {{ ... }} block — PLAIN puts credentials in clear text on the wire and must run over TLS. Use scram_sha_256 or scram_sha_512 if TLS is not available.",
        name
    )
}

/// Cheap pre-check that fires BEFORE `parse_sasl_block` reads the
/// password file from disk. If the operator has both a broken
/// `password_file` path and `mechanism plain` without TLS, the file
/// I/O error masks the more important diagnostic (the
/// credentials-on-the-wire problem), and fixing the path only
/// exposes the second error after the fact. Surfacing the TLS
/// requirement first lets the operator see the deeper issue without
/// guessing which fix to apply first.
fn pre_check_plain_requires_tls(
    name: &str,
    properties: &[Property],
    tls: Option<&ClientTlsConfig>,
) -> Result<()> {
    let Some(sasl_block) = props::get_block(properties, "sasl") else {
        return Ok(());
    };
    let Some(mechanism) = props::get_ident(sasl_block, "mechanism") else {
        return Ok(());
    };
    if mechanism == "plain" && tls.is_none() {
        return Err(plain_without_tls_error(name));
    }
    Ok(())
}

/// Post-parse guard: reject `mechanism plain` without a `tls { ... }`
/// block. SASL/PLAIN transmits the username and password in clear text
/// on the wire, so the only safe transport is TLS (Kafka and Confluent
/// both document this requirement: "TLS/SSL encryption should always be
/// used if SASL mechanism is PLAIN"). If the operator wants plain-text
/// Kafka, the supported path is `scram_sha_256` / `scram_sha_512`,
/// which use a challenge-response and never put the password on the
/// wire. Kept as a belt-and-braces check after `pre_check_plain_requires_tls`
/// in case future refactors reorder or skip the pre-check.
///
/// Note on semantics: this checks `tls.is_none()` — i.e. *presence*
/// of any `tls { ... }` block — not its validity. A tls block whose
/// `ca` / `cert` / `key` paths point at non-existent files satisfies
/// the guard at config-load time and is only rejected later when
/// rdkafka tries to load the PEM bytes. Today that's fine because
/// the kafka schema doesn't expose a `verify` toggle, so the only
/// way to "have a tls block but no real TLS" is via a path typo,
/// and the operator sees the file error a moment later. If a future
/// `verify false` knob lands here (matching `output http` /
/// `output otlp_http`), this guard would need to additionally
/// reject `mechanism plain` + `verify false` since that combination
/// would also put PLAIN credentials on a non-verified wire.
fn require_tls_for_plain(
    name: &str,
    sasl: Option<&SaslConfig>,
    tls: Option<&ClientTlsConfig>,
) -> Result<()> {
    if let Some(s) = sasl
        && s.mechanism == "PLAIN"
        && tls.is_none()
    {
        return Err(plain_without_tls_error(name));
    }
    Ok(())
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

pub struct KafkaOutput {
    name: String,
    producer: FutureProducer,
    topic: String,
    key_field: Option<KeyField>,
    queue_timeout: Duration,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

/// Which event-intrinsic field to use as the Kafka partition key.
///
/// Only event-intrinsic fields are accepted: the partition key cannot
/// depend on pipeline-internal workspace state. Operators who need
/// per-tenant partition ordering must split the traffic into separate
/// outputs at the pipeline body level, each with its own static topic.
#[derive(Debug, Clone)]
enum KeyField {
    Source,
}

impl Module for KafkaOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(KAFKA_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let error_log = ctx.error_log.as_ref().map(Arc::clone);
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
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

        let key_field = match props::get_ident(properties, "key") {
            Some(k) if k == "source" => Some(KeyField::Source),
            Some(other) => anyhow::bail!(
                "output '{}': kafka `key` must be `source` (got `{}`). \
                 Per-tenant or per-field partitioning by pipeline-internal \
                 workspace state is no longer supported; split the traffic \
                 into separate kafka outputs from the pipeline body and \
                 give each a static topic.",
                name,
                other,
            ),
            None => None,
        };

        let tls = parse_tls_block(name, properties)?;
        // Pre-check before reading the password file so a broken
        // `password_file` path doesn't mask the more important
        // PLAIN-without-TLS error.
        pre_check_plain_requires_tls(name, properties, tls.as_ref())?;
        let sasl = parse_sasl_block(name, properties)?;
        require_tls_for_plain(name, sasl.as_ref(), tls.as_ref())?;

        // message.timeout.ms: rdkafka's internal delivery timeout (includes retries to broker).
        // Separate from queue_timeout which is the wait time when the internal queue is full.
        // If delivery fails after this timeout, the Kafka output's own `consume`
        // path (driven by `retry { ... }`) handles re-delivery; the queue layer
        // only advances its cursor once each ack handle resolves.
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
            name: name.to_string(),
            producer,
            topic,
            key_field,
            queue_timeout,
            retry,
            error_log,
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
            shutdown_signal: ctx.shutdown_signal.clone(),
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
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            // Kafka's side-effect boundary is the
            // `producer.send(...)` call inside `try_send`:
            // librdkafka's `FutureProducer::send` synchronously
            // enqueues the record into the internal producer queue,
            // and dropping the returned future waits-side only — the
            // record still ships to the broker.
            //
            // No `pre_send_or_shutdown` wrapper here. Wrapping
            // `try_send` in one would let a `tokio::select!`
            // cancel the `producer.send(...).await` mid-flight, drop
            // the delivery future, and return `None` — while the
            // record is still on its way to the broker. The caller
            // would then route the event to DLQ as `Recovered`, so
            // one send attempt would produce both a broker delivery
            // AND a replayable DLQ record (duplicate delivery).
            //
            // Instead, `try_send` is entirely synchronous up to a
            // final `shutdown.borrow_and_update()` recheck
            // immediately before `producer.send(...)`. That recheck
            // is the sole load-bearing shutdown boundary for
            // steady-state consume: if shutdown was already observed
            // when the prep finishes, `try_send` returns
            // `PreSendShutdown` and no send is started; if shutdown
            // flips *after* `producer.send(...)` is entered, the
            // send runs to completion under librdkafka's own
            // `queue_timeout` / `message.timeout.ms` — the runtime
            // shutdown budget does not truncate it here. A
            // shutdown that fires between the pre-send prep and the
            // recheck is a legitimate race that resolves in favour
            // of "send happens" and is caught by the next iteration
            // or by the runtime SIGTERM budget.
            match self.try_send(event, shutdown.clone()).await {
                Ok(KafkaTrySendOutcome::Delivered) => {
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                Ok(KafkaTrySendOutcome::PreSendShutdown) => {
                    let reason = format!(
                        "output '{}': write attempt abandoned on shutdown (pre-send)",
                        self.name
                    );
                    let __dlq_outcome = crate::modules::route_event_to_dlq(
                        self.error_log.as_ref(),
                        self.error_log_fallback,
                        &self.metrics,
                        &self.name,
                        event,
                        ack.position(),
                        &reason,
                    )
                    .await;
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.inc();
                    if attempt >= self.retry.max_attempts {
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    // Race the backoff sleep against shutdown. If the runtime
                    // signals shutdown mid-sleep, do NOT keep retrying — the
                    // retry budget (default 1+2+4+8 = 15 s) can outlast the
                    // runtime's 10 s shutdown budget, and if we don't return
                    // the queue consumer's select! never gets back to its
                    // shutdown arm. Route the pending event to DLQ, resolve
                    // `Recovered`, and return.
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        // `consume_shutdown` is called *because* the runtime
        // shutdown signal has fired — passing the process-wide
        // shutdown receiver into `try_send` would cause its
        // pre-send recheck to short-circuit every event as
        // `PreSendShutdown` and no drain event would ship. Build a
        // fresh receiver that never observes shutdown so the
        // recheck is a no-op. The `_no_shutdown_tx` binding keeps
        // the sender alive for the whole call.
        //
        // No outer `timeout(...)` around `try_send` here — the wait
        // envelope is librdkafka's own `queue_timeout` /
        // `message.timeout.ms`. Wrapping the future in an outer
        // timeout would cancel it after `producer.send(...)` had
        // already handed the record to librdkafka; the record would
        // still ship to the broker (drop only cancels observation),
        // and the shutdown-drain path would then route the same
        // event through the DLQ as `Recovered`, producing a
        // duplicate delivery + replayable DLQ record. The same
        // reasoning applies here as the steady-state `consume` path:
        // once the side-effect boundary is crossed, an outer
        // timeout is unsafe.
        //
        // `try_send`'s `Err` is itself ambiguous with respect to
        // the wire boundary: `queue_timeout` / `message.timeout.ms`
        // fires *after* `producer.send(...)` has synchronously
        // enqueued the record into librdkafka's internal queue, and
        // the record may or may not have been shipped to the broker
        // by the time the awaited delivery future returns Err.
        // Route through `finalize_shutdown_singleton_disposition_ambiguous`
        // so shutdown-time failure is forced to `Dropped`: disk
        // queues wedge for next-start reconciliation, memory queues
        // fall back to `Recovered` inside the disposition helper.
        // Recovered would fabricate an at-least-once guarantee kafka
        // does not support at this boundary.
        let (_no_shutdown_tx, no_shutdown_rx) = tokio::sync::watch::channel(false);
        let result = match self.try_send(event, no_shutdown_rx).await {
            Ok(KafkaTrySendOutcome::Delivered) => Ok(()),
            Ok(KafkaTrySendOutcome::PreSendShutdown) => {
                // The receiver we passed can never observe
                // shutdown — this branch is a defensive
                // assertion, not a live code path.
                unreachable!(
                    "kafka consume_shutdown must not observe PreSendShutdown from a never-fire receiver"
                )
            }
            Err(e) => Err(e),
        };
        crate::modules::finalize_shutdown_singleton_disposition_ambiguous(
            result,
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

impl KafkaOutput {
    /// Steady-state single-attempt send.
    ///
    /// The pre-send phase is everything up to (but not including)
    /// `producer.send(...)`: key resolution, payload cloning, and
    /// `FutureRecord` construction — all in-process, no wire-side
    /// effect. Once `producer.send(...)` is called, librdkafka has
    /// **synchronously** enqueued the record into its internal
    /// producer queue and there is no supported way to remove it
    /// (dropping the returned delivery future cancels the wait
    /// only; the record still ships to the broker, subject to
    /// `message.timeout.ms`). This helper therefore takes a
    /// shutdown receiver and rechecks it once more immediately
    /// before the `producer.send(...)` call — that recheck is
    /// where the side-effect boundary sits.
    ///
    /// If shutdown flips *between* this recheck and the
    /// `producer.send(...)` call the record ships. There is no
    /// outer race in `Output::consume` that could catch this: the
    /// caller `await`s `try_send` directly, deliberately, to avoid
    /// letting a `tokio::select!` shutdown arm cancel the
    /// `producer.send(...)` future mid-flight (that would produce
    /// duplicate delivery — record on the broker AND in the DLQ).
    /// The remaining unavoidable gap is covered by two operator-
    /// visible mechanisms: the delivery future's own
    /// `queue_timeout` / `message.timeout.ms` bounds the wait, and
    /// on a disk queue a shutdown-driven Dropped disposition
    /// triggers the fail-stop wedge so the cursor holds for
    /// replay on next start.
    async fn try_send(
        &self,
        event: &Event,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<KafkaTrySendOutcome> {
        let key = self.resolve_key(event);
        let egress = event.egress.clone();

        let mut record = FutureRecord::to(&self.topic).payload(egress.as_ref());
        if let Some(ref k) = key {
            record = record.key(k);
        }

        // Load-bearing shutdown recheck: the point of no return is
        // the `producer.send(...)` call below. Everything above is
        // in-process prep; everything below waits for the delivery
        // report of a record that has already been queued to
        // librdkafka. Return `PreSendShutdown` here rather than
        // starting a send we cannot cancel.
        if *shutdown.borrow_and_update() {
            return Ok(KafkaTrySendOutcome::PreSendShutdown);
        }

        // Side-effect boundary crossed. Await the delivery future
        // to completion (bounded by `queue_timeout` /
        // `message.timeout.ms`). A shutdown flip past this line
        // does **not** cancel the send: dropping the future would
        // only stop us from observing the delivery report, not
        // stop the ship.
        self.producer
            .send(record, self.queue_timeout)
            .await
            .map_err(|(e, _)| anyhow::anyhow!("kafka produce failed: {}", e))?;
        Ok(KafkaTrySendOutcome::Delivered)
    }
}

/// Outcome of a single kafka `try_send` attempt.
enum KafkaTrySendOutcome {
    Delivered,
    PreSendShutdown,
}

impl Drop for KafkaOutput {
    fn drop(&mut self) {
        if let Err(e) = self.producer.flush(Duration::from_secs(5)) {
            tracing::warn!("kafka output: flush on shutdown failed: {}", e);
        }
    }
}

impl KafkaOutput {
    /// Compute the optional Kafka message key from `event`. Returns
    /// `None` when no `key_field` is configured. Reads only
    /// event-intrinsic fields (never pipeline-internal workspace).
    fn resolve_key(&self, event: &Event) -> Option<String> {
        let kf = self.key_field.as_ref()?;
        let value = match kf {
            KeyField::Source => event.source.ip().to_string(),
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

    fn mp(props: &[Property]) -> crate::dsl::module_props::ModuleProperties {
        crate::dsl::module_props::ModuleProperties::from_parts("kafka", props.to_vec())
    }

    // ---- key field migration -----------------------------------------

    #[test]
    fn from_properties_rejects_arbitrary_key_field_with_migration_hint() {
        // Previously the kafka output accepted any ident as `key`,
        // looking up `event.workspace.<ident>` at send time. That
        // capability is gone; only the event-intrinsic `source`
        // selector remains, and any other value bails with a
        // migration message.
        let props = vec![
            str_prop("brokers", "localhost:9092"),
            str_prop("topic", "t"),
            ident_prop("key", "user_id"),
        ];
        let err = KafkaOutput::from_properties(
            "k",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("constructor must reject pipeline-mutable key field");
        let msg = err.to_string();
        assert!(msg.contains("`source`"), "msg: {}", msg);
        assert!(
            msg.contains("separate kafka outputs from the pipeline body"),
            "msg: {}",
            msg,
        );
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

    #[test]
    fn parse_sasl_plain_strips_crlf_password_file() {
        // Windows hosts (or any editor that uses CRLF) write the
        // trailing newline as `\r\n`; stripping only `\n` leaves a
        // bare `\r` on the password and Kafka authentication fails
        // with a "bad credentials" error that looks like a wrong
        // password.
        let pw = make_password_file(b"hunter2\r\n");
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "limpid"),
                str_prop("password_file", &pw.path),
            ],
        )];
        let sasl = parse_sasl_block("k", &props).unwrap().unwrap();
        assert_eq!(sasl.password, "hunter2");
    }

    #[test]
    fn parse_sasl_plain_strips_bare_cr_password_file() {
        let pw = make_password_file(b"hunter2\r");
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "u"),
                str_prop("password_file", &pw.path),
            ],
        )];
        let sasl = parse_sasl_block("k", &props).unwrap().unwrap();
        assert_eq!(sasl.password, "hunter2");
    }

    // ---- require_tls_for_plain ----

    fn sasl(mechanism: &str) -> SaslConfig {
        SaslConfig {
            mechanism: mechanism.to_string(),
            username: "u".into(),
            password: "p".into(),
        }
    }

    fn empty_tls() -> ClientTlsConfig {
        ClientTlsConfig {
            ca_path: None,
            cert_path: None,
            key_path: None,
        }
    }

    #[test]
    fn require_tls_for_plain_rejects_plain_without_tls() {
        let s = sasl("PLAIN");
        let err = require_tls_for_plain("k", Some(&s), None).err().unwrap();
        assert!(err.to_string().contains("tls"), "{}", err);
        assert!(err.to_string().contains("plain"), "{}", err);
    }

    #[test]
    fn require_tls_for_plain_accepts_plain_with_tls() {
        let s = sasl("PLAIN");
        let t = empty_tls();
        require_tls_for_plain("k", Some(&s), Some(&t)).unwrap();
    }

    #[test]
    fn require_tls_for_plain_accepts_scram_without_tls() {
        // SCRAM uses challenge-response; the password never goes on
        // the wire, so plaintext transport is acceptable (though
        // typically still paired with TLS in production).
        let s = sasl("SCRAM-SHA-512");
        require_tls_for_plain("k", Some(&s), None).unwrap();
    }

    #[test]
    fn require_tls_for_plain_accepts_no_sasl() {
        require_tls_for_plain("k", None, None).unwrap();
    }

    // ---- pre_check_plain_requires_tls (ordering guard) ----

    #[test]
    fn pre_check_plain_requires_tls_fires_before_password_file_read() {
        // Regression guard for the audit finding A2: if both the
        // password_file path is broken AND the operator wrote
        // `mechanism plain` without a tls block, the pre-check
        // surfaces the TLS requirement *first*. Without the pre-
        // check the file-read error from parse_sasl_block would mask
        // the more important credentials-on-the-wire problem.
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "u"),
                str_prop("password_file", "/nonexistent/does/not/exist.txt"),
            ],
        )];
        // No TLS block, plain mechanism: pre-check must fire here
        // and we must NEVER hit the file-read in parse_sasl_block.
        let err = pre_check_plain_requires_tls("k", &props, None)
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("plain") && msg.contains("tls"), "{msg}");
        assert!(
            !msg.contains("password_file"),
            "must not leak file-path: {msg}"
        );
    }

    #[test]
    fn pre_check_plain_passes_when_tls_block_present() {
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "plain"),
                str_prop("username", "u"),
                str_prop("password_file", "/nonexistent/does/not/exist.txt"),
            ],
        )];
        let tls = empty_tls();
        pre_check_plain_requires_tls("k", &props, Some(&tls)).unwrap();
    }

    #[test]
    fn pre_check_plain_passes_for_scram_without_tls() {
        let props = vec![block(
            "sasl",
            vec![
                ident_prop("mechanism", "scram_sha_512"),
                str_prop("username", "u"),
                str_prop("password_file", "/nonexistent/does/not/exist.txt"),
            ],
        )];
        pre_check_plain_requires_tls("k", &props, None).unwrap();
    }

    /// Structural regression pin for the duplicate-delivery bug: the
    /// steady-state `consume` MUST NOT wrap `try_send` in a
    /// `pre_send_or_shutdown` (or any other `select!`-shaped
    /// construct). Doing so lets a mid-`producer.send(...).await`
    /// shutdown drop the delivery future while librdkafka has
    /// already accepted the record, producing both a broker
    /// delivery AND a DLQ-replayable record. The load-bearing
    /// shutdown boundary is the `borrow_and_update()` recheck
    /// immediately before `producer.send(...)` inside `try_send`
    /// — this is the single, atomic pre-send guard.
    ///
    /// Detection strategy: read this source file at test time and
    /// assert that no `consume`-scoped `pre_send_or_shutdown` call
    /// exists. A grep-shaped structural test is unusual, but the
    /// alternative — wiring up a real `FutureProducer` to observe
    /// a mid-send cancellation — requires a Kafka broker in the
    /// test harness. The regression this pins is a shape, not a
    /// value, so a source-level pin is honest.
    #[test]
    fn consume_does_not_wrap_try_send_in_pre_send_or_shutdown() {
        let src = include_str!("kafka.rs");
        // Extract the body of `Output::consume` (up to
        // `consume_shutdown` which is the next `async fn`).
        let consume_start = src
            .find("async fn consume(")
            .expect("consume fn must exist");
        let consume_end = src[consume_start..]
            .find("async fn consume_shutdown(")
            .expect("consume_shutdown fn must follow");
        let consume_body = &src[consume_start..consume_start + consume_end];
        // Match a call, not a bare mention: an in-body reference in
        // a doc comment (e.g. "No pre_send_or_shutdown wrapper here")
        // must not trip the pin, but the actual function call
        // `crate::modules::pre_send_or_shutdown(...)` must.
        assert!(
            !consume_body.contains("pre_send_or_shutdown("),
            "Output::consume must not wrap try_send in pre_send_or_shutdown; that lets a \
             mid-producer.send shutdown drop the delivery future while librdkafka has \
             already accepted the record, producing duplicate delivery. The load-bearing \
             pre-send guard lives inside try_send at the borrow_and_update recheck."
        );
    }

    /// `consume_shutdown` routes its result through
    /// `finalize_shutdown_singleton_disposition_ambiguous`, not the
    /// plain variant. Rationale: `try_send`'s `Err` fires after
    /// `producer.send(...)` has synchronously enqueued the record
    /// into librdkafka; the delivery future's failure (queue_timeout /
    /// message.timeout.ms) does not distinguish "queue full, never
    /// touched broker" from "record was picked up and broker ack
    /// never came". Routing `Err` as honest `Recovered` on shutdown
    /// therefore risks a duplicate — force `Dropped` so the disk
    /// queue wedges for next-start reconciliation instead.
    #[test]
    fn consume_shutdown_uses_ambiguous_finalizer() {
        let src = include_str!("kafka.rs");
        let start = src
            .find("async fn consume_shutdown(")
            .expect("consume_shutdown fn must exist");
        let end_offset = src[start..]
            .find("impl KafkaOutput {")
            .expect("consume_shutdown body must end before next impl block");
        let body = &src[start..start + end_offset];
        assert!(
            body.contains("finalize_shutdown_singleton_disposition_ambiguous("),
            "kafka consume_shutdown must use the _ambiguous finalizer — try_send Err after \
             producer.send is ambiguous with respect to the wire boundary, and honest Recovered \
             would risk a duplicate on next-start replay of the DLQ record."
        );
        assert!(
            !body.contains("crate::modules::finalize_shutdown_singleton_disposition("),
            "kafka consume_shutdown must not fall back to the plain finalizer."
        );
    }

    /// Same shape pin, on the shutdown-drain path: `consume_shutdown`
    /// must not wrap `try_send` in an outer `tokio::time::timeout`.
    /// The steady-state `consume` had this rule from the beginning;
    /// the shutdown path used to carry a `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT`
    /// wrapper that reintroduced exactly the same duplicate-delivery
    /// failure mode: a timeout that fires after `producer.send(...)`
    /// has already handed the record to librdkafka cancels the
    /// delivery future but not the ship — the record still goes to
    /// the broker, and the shutdown-drain path routes the same
    /// event through the DLQ as `Recovered`. The wait envelope is
    /// librdkafka's `queue_timeout` / `message.timeout.ms`; a
    /// disk-queue Dropped on failure triggers the fail-stop wedge
    /// for next-start replay.
    #[test]
    fn consume_shutdown_does_not_wrap_try_send_in_outer_timeout() {
        let src = include_str!("kafka.rs");
        let start = src
            .find("async fn consume_shutdown(")
            .expect("consume_shutdown fn must exist");
        // Take the body up to the next top-level `impl` or the end
        // of the `impl Output for KafkaOutput` block. The next
        // `impl KafkaOutput` marks the boundary.
        let end_offset = src[start..]
            .find("impl KafkaOutput {")
            .expect("consume_shutdown body must end before next impl block");
        let body = &src[start..start + end_offset];
        assert!(
            !body.contains("tokio::time::timeout("),
            "consume_shutdown must not wrap try_send in tokio::time::timeout; that lets a \
             post-producer.send timeout cancel the delivery future while librdkafka has \
             already accepted the record — same duplicate-delivery gap as the steady-state \
             consume. Rely on queue_timeout / message.timeout.ms for the wait envelope."
        );
    }

    /// Functional pin for the pre-send load-bearing guard: with the
    /// shutdown receiver already set to `true`, `try_send` must
    /// return `PreSendShutdown` without touching `producer.send`.
    /// This is the shutdown boundary the outer-wrapper removal
    /// relies on — losing it would let a shutdown observed before
    /// the send start still result in a broker delivery.
    #[tokio::test]
    async fn try_send_short_circuits_when_shutdown_already_set() {
        use crate::event::Event;
        use bytes::Bytes;

        let (tx, rx) = tokio::sync::watch::channel(true); // pre-set
        Box::leak(Box::new(tx)); // keep sender alive for the test

        // Build a minimal KafkaOutput. `client.create()` synchronously
        // returns a `FutureProducer` handle; librdkafka defers all
        // broker connection to the first produce call, which we
        // never reach because the shutdown recheck short-circuits.
        let mut client = ClientConfig::new();
        client
            .set("bootstrap.servers", "127.0.0.1:1") // unreachable, unused
            .set("message.timeout.ms", "5000");
        let producer: FutureProducer = client.create().expect("build producer");
        let output = KafkaOutput {
            name: "k".into(),
            producer,
            topic: "t".into(),
            key_field: None,
            queue_timeout: Duration::from_secs(1),
            retry: RetryConfig::default(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal: rx.clone(),
        };
        let event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let outcome = output.try_send(&event, rx).await.expect("try_send Ok arm");
        assert!(
            matches!(outcome, KafkaTrySendOutcome::PreSendShutdown),
            "try_send with pre-set shutdown must return PreSendShutdown, got {:?}",
            match outcome {
                KafkaTrySendOutcome::Delivered => "Delivered",
                KafkaTrySendOutcome::PreSendShutdown => "PreSendShutdown",
            }
        );
    }
}

//! Kafka output: produces event messages to an Apache Kafka topic.
//!
//! Uses librdkafka via the rdkafka crate. The producer handles batching,
//! compression, retries, and connection management internally.
//!
//! Properties:
//!   brokers   "kafka1:9092,kafka2:9092"   — required
//!   topic     "syslog-events"             — required
//!   compression  snappy                   — optional (none, gzip, snappy, lz4, zstd)
//!   acks      all                         — optional (0, 1, all; default: all)
//!   key       source                      — optional (event field to use as partition key)
//!   queue_timeout "5s"                    — optional (max wait when internal queue is full; default: 5s)

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, ModuleSchema, Output};

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
    Facility,
    Severity,
    Field(String),
}

impl Module for KafkaOutput {
    fn schema() -> ModuleSchema {
        ModuleSchema::default()
    }

    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
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
            "facility" => KeyField::Facility,
            "severity" => KeyField::Severity,
            other => KeyField::Field(other.to_string()),
        });

        // message.timeout.ms: rdkafka's internal delivery timeout (includes retries to broker).
        // Separate from queue_timeout which is the wait time when the internal queue is full.
        // If delivery fails after this timeout, limpid's queue retry mechanism handles re-delivery.
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("compression.type", &compression)
            .set("acks", &acks)
            .set("message.timeout.ms", "30000")
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
    async fn write(&self, event: &Event) -> Result<()> {
        let payload = &event.message;

        let key = self.resolve_key(event);

        let mut record = FutureRecord::to(&self.topic).payload(payload.as_ref());
        if let Some(ref k) = key {
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
    fn resolve_key(&self, event: &Event) -> Option<String> {
        let kf = self.key_field.as_ref()?;
        let value = match kf {
            KeyField::Source => event.source.ip().to_string(),
            KeyField::Facility => event.facility.map(|f| f.to_string())?,
            KeyField::Severity => event.severity.map(|s| s.to_string())?,
            KeyField::Field(name) => event
                .fields
                .get(name)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())?,
        };
        Some(value)
    }
}

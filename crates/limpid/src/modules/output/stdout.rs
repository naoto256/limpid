//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;

use crate::dsl::schema::PropertySpec;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};

/// `output stdout` has no module-specific properties; only the
/// common `retry { ... } / queue { ... }` sub-blocks apply.
const STDOUT_OUTPUT_SCHEMA: &[PropertySpec] = &[
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct StdoutOutput {
    metrics: Arc<OutputMetrics>,
}

impl Module for StdoutOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(STDOUT_OUTPUT_SCHEMA)
    }

    fn from_properties(
        _name: &str,
        _properties: &crate::modules::ModuleProperties,
    ) -> Result<Self> {
        Ok(Self {
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for StdoutOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for StdoutOutput {
    async fn consume(&self, event: &Event) -> Result<()> {
        // Trivial sink: no template rendering, no transport batching —
        // print the egress bytes directly. `String::from_utf8_lossy`
        // is the same egress→stdout step the previous `write` path
        // ran; only the `RenderedPayload` plumbing is gone.
        let msg = String::from_utf8_lossy(&event.egress);
        println!("{}", msg);
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

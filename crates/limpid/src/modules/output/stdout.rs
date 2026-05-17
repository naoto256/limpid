//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use bytes::Bytes;

use crate::dsl::arena::EventArena;
use crate::dsl::schema::PropertySpec;
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

/// `output stdout` has no module-specific properties; only the common
/// `queue { ... }` sub-block applies.
const STDOUT_OUTPUT_SCHEMA: &[PropertySpec] = &[crate::queue::QUEUE_PROPERTY_SPEC];

pub struct StdoutOutput {
    metrics: Arc<OutputMetrics>,
}

/// Per-event payload: just the egress bytes (refcounted clone).
struct StdoutPayload {
    egress: Bytes,
}

impl Module for StdoutOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(STDOUT_OUTPUT_SCHEMA)
    }

    fn from_properties(_name: &str, _properties: &crate::modules::ModuleProperties) -> Result<Self> {
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
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(StdoutPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: StdoutPayload = payload.downcast()?;
        let msg = String::from_utf8_lossy(&payload.egress);
        println!("{}", msg);
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

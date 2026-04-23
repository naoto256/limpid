//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;

use crate::dsl::ast::Property;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{FromProperties, HasMetrics, Output};

pub struct StdoutOutput {
    metrics: Arc<OutputMetrics>,
}

impl FromProperties for StdoutOutput {
    fn from_properties(_name: &str, _properties: &[Property]) -> Result<Self> {
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
    async fn write(&self, event: &Event) -> Result<()> {
        let msg = String::from_utf8_lossy(&event.message);
        println!("{}", msg);
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

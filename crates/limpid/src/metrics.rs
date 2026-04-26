//! Shared metrics counters.
//!
//! Each component owns its own `Arc<XxxMetrics>` and counts internally.
//! `MetricsRegistry` holds references for aggregated access (stats command).
//! Runtime never counts — it only distributes handles.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct InputMetrics {
    /// Events actually received by the input module (network, socket, file, etc).
    /// Injected events are NOT counted here — see `events_injected`.
    pub events_received: AtomicU64,
    pub events_invalid: AtomicU64,
    /// Events pushed into this input's channel via `limpidctl inject`.
    pub events_injected: AtomicU64,
}

impl Default for InputMetrics {
    fn default() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_invalid: AtomicU64::new(0),
            events_injected: AtomicU64::new(0),
        }
    }
}

pub struct PipelineMetrics {
    pub events_received: AtomicU64,
    pub events_finished: AtomicU64,
    pub events_dropped: AtomicU64,
    pub events_discarded: AtomicU64,
    /// Events for which a `process` statement raised a runtime error
    /// (unknown identifier, type mismatch, regex compile failure, …).
    /// The event is discarded rather than forwarded with the original
    /// ingress unchanged. Distinct from `events_discarded` so operators
    /// can tell a config-bug-shaped routing miss apart from a logic-bug-
    /// shaped runtime failure.
    pub events_errored: AtomicU64,
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_finished: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
            events_discarded: AtomicU64::new(0),
            events_errored: AtomicU64::new(0),
        }
    }
}

pub struct OutputMetrics {
    /// Total events that entered this output's queue (from pipelines + injects).
    /// `events_received - events_injected` = events delivered via pipelines.
    pub events_received: AtomicU64,
    /// Events pushed into this output's queue via `limpidctl inject`.
    pub events_injected: AtomicU64,
    pub events_written: AtomicU64,
    pub events_failed: AtomicU64,
    pub retries: AtomicU64,
}

impl Default for OutputMetrics {
    fn default() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_injected: AtomicU64::new(0),
            events_written: AtomicU64::new(0),
            events_failed: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        }
    }
}

/// Central registry holding Arc references to all metrics counters.
pub struct MetricsRegistry {
    inputs: HashMap<String, Arc<InputMetrics>>,
    pipelines: HashMap<String, Arc<PipelineMetrics>>,
    outputs: HashMap<String, Arc<OutputMetrics>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            pipelines: HashMap::new(),
            outputs: HashMap::new(),
        }
    }

    /// Collect a metrics handle from a module that owns it.
    pub fn register_input(&mut self, name: &str, metrics: Arc<InputMetrics>) {
        self.inputs.insert(name.to_string(), metrics);
    }

    /// Collect a metrics handle from a pipeline worker that owns it.
    pub fn register_pipeline(&mut self, name: &str, metrics: Arc<PipelineMetrics>) {
        self.pipelines.insert(name.to_string(), metrics);
    }

    /// Collect a metrics handle from an output module that owns it.
    pub fn register_output(&mut self, name: &str, metrics: Arc<OutputMetrics>) {
        self.outputs.insert(name.to_string(), metrics);
    }

    pub fn to_json(&self) -> String {
        let mut map = serde_json::Map::new();

        // Pipelines first — they're the main concept.
        let mut pipelines = serde_json::Map::new();
        for (name, m) in &self.pipelines {
            let mut p = serde_json::Map::new();
            p.insert(
                "events_received".into(),
                m.events_received.load(Ordering::Relaxed).into(),
            );
            p.insert(
                "events_finished".into(),
                m.events_finished.load(Ordering::Relaxed).into(),
            );
            p.insert(
                "events_dropped".into(),
                m.events_dropped.load(Ordering::Relaxed).into(),
            );
            p.insert(
                "events_discarded".into(),
                m.events_discarded.load(Ordering::Relaxed).into(),
            );
            p.insert(
                "events_errored".into(),
                m.events_errored.load(Ordering::Relaxed).into(),
            );
            pipelines.insert(name.clone(), serde_json::Value::Object(p));
        }
        map.insert("pipelines".into(), serde_json::Value::Object(pipelines));

        let mut inputs = serde_json::Map::new();
        for (name, m) in &self.inputs {
            let mut i = serde_json::Map::new();
            i.insert(
                "events_received".into(),
                m.events_received.load(Ordering::Relaxed).into(),
            );
            i.insert(
                "events_invalid".into(),
                m.events_invalid.load(Ordering::Relaxed).into(),
            );
            i.insert(
                "events_injected".into(),
                m.events_injected.load(Ordering::Relaxed).into(),
            );
            inputs.insert(name.clone(), serde_json::Value::Object(i));
        }
        map.insert("inputs".into(), serde_json::Value::Object(inputs));

        let mut outputs = serde_json::Map::new();
        for (name, m) in &self.outputs {
            let mut o = serde_json::Map::new();
            o.insert(
                "events_received".into(),
                m.events_received.load(Ordering::Relaxed).into(),
            );
            o.insert(
                "events_injected".into(),
                m.events_injected.load(Ordering::Relaxed).into(),
            );
            o.insert(
                "events_written".into(),
                m.events_written.load(Ordering::Relaxed).into(),
            );
            o.insert(
                "events_failed".into(),
                m.events_failed.load(Ordering::Relaxed).into(),
            );
            o.insert("retries".into(), m.retries.load(Ordering::Relaxed).into());
            outputs.insert(name.clone(), serde_json::Value::Object(o));
        }
        map.insert("outputs".into(), serde_json::Value::Object(outputs));

        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
    }
}

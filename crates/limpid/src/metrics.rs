//! Shared metrics counters.
//!
//! Each component owns its own `Arc<XxxMetrics>` and counts internally.
//! `MetricsRegistry` holds references for aggregated access (stats command).
//! Runtime never counts — it only distributes handles.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

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
    /// The event is routed to the dead-letter queue: the configured
    /// `error_log` JSONL file when set, or — when unset — a
    /// payload-free tracing summary via the `error_log_fallback`
    /// ladder (`emit_dlq_tracing_fallback` helper; `Meta` / `Full`
    /// upgrade the line only on explicit operator opt-in). Distinct
    /// from `events_discarded` so operators can tell a config-bug-
    /// shaped routing miss apart from a logic-bug-shaped runtime
    /// failure.
    pub events_errored: AtomicU64,
    /// Subset of `events_errored` for which the configured
    /// `error_log` write itself failed (disk full, permissions,
    /// rotation race). The runtime falls back to a structured
    /// `tracing::error!` line, but operators should alarm on this
    /// counter — it means the replay path may be incomplete.
    pub events_errored_unwritable: AtomicU64,
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_finished: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
            events_discarded: AtomicU64::new(0),
            events_errored: AtomicU64::new(0),
            events_errored_unwritable: AtomicU64::new(0),
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
    /// Disk queue consumers that stopped accepting new events after
    /// observing an `AckDisposition::Dropped` — `Dropped` cannot
    /// advance a disk cursor without hiding data loss, and
    /// continuing to accept would grow the in-flight bookkeeping
    /// unboundedly. Bumped once per fail-stop event on a disk
    /// consumer; memory-queue consumers never bump this counter
    /// (they cannot replay on restart, so continuing is the only
    /// available policy). A non-zero value here is an
    /// operator-facing signal to investigate the output for the
    /// underlying bug / panic / DLQ-write failure and restart the
    /// daemon so the disk queue can replay from the wedge point.
    /// See `docs/src/operations/error-log.md` for the manual
    /// intervention runbook.
    pub events_wedged: AtomicU64,
    /// Sink-side counterpart to `PipelineMetrics::events_errored_unwritable`:
    /// bumped when an output's own `route_event_to_dlq` call
    /// failed to write the DLQ record (`error_log` was configured
    /// but the disk write itself errored — full disk, permission
    /// drop, rotation race). Distinct from the pipeline-side
    /// counter because operator alarms need to know which failure
    /// path is misbehaving. When this bumps on a disk queue the
    /// consumer routes the event as `Dropped` (disk-queue fail-stop wedge)
    /// so the cursor holds and a subsequent daemon start replays
    /// the event through a hopefully-healthy DLQ. On memory
    /// queues the event is `Recovered` regardless — there is no
    /// replay path so the event is actually lost, and this counter
    /// is the operator alarm signal for that loss rather than a
    /// durable trace of it.
    pub events_errored_unwritable: AtomicU64,
}

impl Default for OutputMetrics {
    fn default() -> Self {
        Self {
            events_received: AtomicU64::new(0),
            events_injected: AtomicU64::new(0),
            events_written: AtomicU64::new(0),
            events_failed: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            events_wedged: AtomicU64::new(0),
            events_errored_unwritable: AtomicU64::new(0),
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
            p.insert(
                "events_errored_unwritable".into(),
                m.events_errored_unwritable.load(Ordering::Relaxed).into(),
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
            o.insert(
                "events_wedged".into(),
                m.events_wedged.load(Ordering::Relaxed).into(),
            );
            o.insert(
                "events_errored_unwritable".into(),
                m.events_errored_unwritable.load(Ordering::Relaxed).into(),
            );
            outputs.insert(name.clone(), serde_json::Value::Object(o));
        }
        map.insert("outputs".into(), serde_json::Value::Object(outputs));

        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
    }
}

/// Self-describing metric registry that produces a schema v1 snapshot.
///
/// Label values are fixed at registration time, so the hot path never
/// performs label lookup, lock acquisition, or allocation.
/// `MetricsSnapshot` is the sole source of schema v1 serialization.
/// `.build()` returns `Result<_, MetricsError>` — registration errors
/// must propagate to daemon startup and must not be swallowed at the
/// emit site.
///
/// The module intentionally coexists unwired with the active legacy
/// `MetricsRegistry` above; the `#[allow(dead_code)]` is temporary and
/// applies only while no in-tree consumer of this module exists.
#[allow(dead_code)]
mod registry_core {
    use super::*;

    #[derive(Debug)]
    pub(crate) enum MetricsError {
        InvalidName {
            name: String,
            labelset: Vec<(String, String)>,
        },
        /// Help text is part of the self-describing schema's public
        /// contract, so empty or missing help is rejected at build time
        /// rather than allowed to slip through as silent schema drift.
        MissingHelp {
            name: String,
            labelset: Vec<(String, String)>,
        },
        DuplicateSeries {
            name: String,
            labelset: Vec<(String, String)>,
        },
        /// Metric name identifies a family: type, help, and the set of
        /// label names must match across every series that shares a
        /// name. The strict match is stronger than Prometheus's
        /// exposition rules — deliberately, so a declaration drift
        /// cannot fork the wire schema at export time.
        MetadataConflict {
            name: String,
            existing: String,
            requested: String,
        },
    }

    impl fmt::Display for MetricsError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::InvalidName { name, labelset } => write!(
                    formatter,
                    "invalid Prometheus metric name: name={name:?}, labelset={labelset:?}"
                ),
                Self::MissingHelp { name, labelset } => write!(
                    formatter,
                    "metric help is required: name={name:?}, labelset={labelset:?}"
                ),
                Self::DuplicateSeries { name, labelset } => write!(
                    formatter,
                    "duplicate metric series: name={name:?}, labelset={labelset:?}"
                ),
                Self::MetadataConflict {
                    name,
                    existing,
                    requested,
                } => write!(
                    formatter,
                    "metric family metadata conflict: name={name:?}, existing={existing}, requested={requested}"
                ),
            }
        }
    }

    impl std::error::Error for MetricsError {}

    pub(crate) struct Counter {
        value: AtomicU64,
    }

    impl Counter {
        pub(crate) fn inc(&self) {
            self.value.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) struct Gauge {
        value: AtomicU64,
    }

    impl Gauge {
        pub(crate) fn set(&self, value: u64) {
            self.value.store(value, Ordering::Relaxed);
        }
    }

    /// Prometheus-style histogram whose per-boundary `bucket_counts`
    /// hold the **raw** hit count for their bucket range. The
    /// cumulative shape Prometheus expects is assembled at export time
    /// by `snapshot_series` — keeping storage raw lets `observe` stay a
    /// single `fetch_add` per bucket instead of a cascade. `sum_bits`
    /// holds an `f64` bit pattern in an `AtomicU64` because Rust has no
    /// atomic add over `f64`; `observe` uses a CAS loop to keep the
    /// running sum lock-free.
    pub(crate) struct Histogram {
        boundaries: Vec<f64>,
        bucket_counts: Vec<AtomicU64>,
        count: AtomicU64,
        sum_bits: AtomicU64,
    }

    impl Histogram {
        pub(crate) fn observe(&self, value: f64) {
            if let Some(index) = self
                .boundaries
                .iter()
                .position(|boundary| value <= *boundary)
            {
                self.bucket_counts[index].fetch_add(1, Ordering::Relaxed);
            }
            self.count.fetch_add(1, Ordering::Relaxed);

            let mut current = self.sum_bits.load(Ordering::Relaxed);
            loop {
                let next = (f64::from_bits(current) + value).to_bits();
                match self.sum_bits.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "lowercase")]
    enum MetricType {
        Counter,
        Gauge,
        Histogram,
    }

    #[derive(Serialize)]
    pub(crate) struct MetricsSnapshot {
        schema: u32,
        metrics: Vec<MetricSnapshot>,
    }

    #[derive(Serialize)]
    struct MetricSnapshot {
        name: String,
        #[serde(rename = "type")]
        metric_type: MetricType,
        help: String,
        series: Vec<SeriesSnapshot>,
    }

    #[derive(Serialize)]
    #[serde(untagged)]
    enum SeriesSnapshot {
        Value {
            labels: BTreeMap<String, String>,
            value: u64,
        },
        Histogram {
            labels: BTreeMap<String, String>,
            buckets: Vec<(f64, u64)>,
            sum: f64,
            count: u64,
        },
    }

    #[derive(Clone, PartialEq, Eq, Hash)]
    struct SeriesKey {
        name: String,
        labelset: Vec<(String, String)>,
    }

    type Labels = Vec<(String, String)>;

    struct ValueSeries<T> {
        labels: Labels,
        handle: Arc<T>,
    }

    enum FamilyKind {
        Counter(Vec<ValueSeries<Counter>>),
        Gauge(Vec<ValueSeries<Gauge>>),
        Histogram(Vec<ValueSeries<Histogram>>),
    }

    impl FamilyKind {
        fn metric_type(&self) -> MetricType {
            match self {
                Self::Counter(_) => MetricType::Counter,
                Self::Gauge(_) => MetricType::Gauge,
                Self::Histogram(_) => MetricType::Histogram,
            }
        }
    }

    struct MetricFamily {
        name: String,
        help: String,
        label_names: Vec<String>,
        kind: FamilyKind,
    }

    #[derive(Default)]
    struct RegistryInner {
        series_keys: HashSet<SeriesKey>,
        families: Vec<MetricFamily>,
    }

    pub(crate) struct Registry {
        inner: Mutex<RegistryInner>,
    }

    impl Registry {
        pub(crate) fn new() -> Self {
            Self {
                inner: Mutex::new(RegistryInner::default()),
            }
        }

        pub(crate) fn counter(&self, name: &str) -> CounterBuilder<'_> {
            CounterBuilder {
                core: BuilderCore::new(self, name),
            }
        }

        pub(crate) fn gauge(&self, name: &str) -> GaugeBuilder<'_> {
            GaugeBuilder {
                core: BuilderCore::new(self, name),
            }
        }

        pub(crate) fn histogram(&self, name: &str) -> HistogramBuilder<'_> {
            HistogramBuilder {
                core: BuilderCore::new(self, name),
                buckets: Vec::new(),
            }
        }

        pub(crate) fn snapshot(&self) -> MetricsSnapshot {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let metrics = inner
                .families
                .iter()
                .map(|family| MetricSnapshot {
                    name: family.name.clone(),
                    metric_type: family.kind.metric_type(),
                    help: family.help.clone(),
                    series: snapshot_series(&family.kind),
                })
                .collect();
            MetricsSnapshot { schema: 1, metrics }
        }

        fn register_counter(
            &self,
            name: String,
            help: Option<String>,
            labels: Labels,
        ) -> Result<Arc<Counter>, MetricsError> {
            let labels = canonical_labels(labels);
            validate_metric_name(&name, &labels)?;
            let help = require_help(&name, &labels, help)?;
            let handle = Arc::new(Counter {
                value: AtomicU64::new(0),
            });
            let mut inner = self.lock_for_registration();
            let family_index = validate_family_metadata(
                &inner,
                &name,
                MetricType::Counter,
                &help,
                &label_names(&labels),
            )?;
            insert_series_key(&mut inner, &name, &labels)?;
            if let Some(index) = family_index {
                let FamilyKind::Counter(series) = &mut inner.families[index].kind else {
                    unreachable!("validated counter family changed while registry lock was held")
                };
                series.push(ValueSeries {
                    labels,
                    handle: handle.clone(),
                });
            } else {
                let label_names = label_names(&labels);
                inner.families.push(MetricFamily {
                    name,
                    help,
                    label_names,
                    kind: FamilyKind::Counter(vec![ValueSeries {
                        labels,
                        handle: handle.clone(),
                    }]),
                });
            }
            Ok(handle)
        }

        fn register_gauge(
            &self,
            name: String,
            help: Option<String>,
            labels: Labels,
        ) -> Result<Arc<Gauge>, MetricsError> {
            let labels = canonical_labels(labels);
            validate_metric_name(&name, &labels)?;
            let help = require_help(&name, &labels, help)?;
            let handle = Arc::new(Gauge {
                value: AtomicU64::new(0),
            });
            let mut inner = self.lock_for_registration();
            let family_index = validate_family_metadata(
                &inner,
                &name,
                MetricType::Gauge,
                &help,
                &label_names(&labels),
            )?;
            insert_series_key(&mut inner, &name, &labels)?;
            if let Some(index) = family_index {
                let FamilyKind::Gauge(series) = &mut inner.families[index].kind else {
                    unreachable!("validated gauge family changed while registry lock was held")
                };
                series.push(ValueSeries {
                    labels,
                    handle: handle.clone(),
                });
            } else {
                let label_names = label_names(&labels);
                inner.families.push(MetricFamily {
                    name,
                    help,
                    label_names,
                    kind: FamilyKind::Gauge(vec![ValueSeries {
                        labels,
                        handle: handle.clone(),
                    }]),
                });
            }
            Ok(handle)
        }

        fn register_histogram(
            &self,
            name: String,
            help: Option<String>,
            labels: Labels,
            boundaries: Vec<f64>,
        ) -> Result<Arc<Histogram>, MetricsError> {
            let labels = canonical_labels(labels);
            validate_metric_name(&name, &labels)?;
            let help = require_help(&name, &labels, help)?;
            let handle = Arc::new(Histogram {
                bucket_counts: boundaries.iter().map(|_| AtomicU64::new(0)).collect(),
                boundaries,
                count: AtomicU64::new(0),
                sum_bits: AtomicU64::new(0.0_f64.to_bits()),
            });
            let mut inner = self.lock_for_registration();
            let family_index = validate_family_metadata(
                &inner,
                &name,
                MetricType::Histogram,
                &help,
                &label_names(&labels),
            )?;
            insert_series_key(&mut inner, &name, &labels)?;
            if let Some(index) = family_index {
                let FamilyKind::Histogram(series) = &mut inner.families[index].kind else {
                    unreachable!("validated histogram family changed while registry lock was held")
                };
                series.push(ValueSeries {
                    labels,
                    handle: handle.clone(),
                });
            } else {
                let label_names = label_names(&labels);
                inner.families.push(MetricFamily {
                    name,
                    help,
                    label_names,
                    kind: FamilyKind::Histogram(vec![ValueSeries {
                        labels,
                        handle: handle.clone(),
                    }]),
                });
            }
            Ok(handle)
        }

        fn lock_for_registration(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    struct BuilderCore<'a> {
        registry: &'a Registry,
        name: String,
        help: Option<String>,
        labels: Labels,
    }

    impl<'a> BuilderCore<'a> {
        fn new(registry: &'a Registry, name: &str) -> Self {
            Self {
                registry,
                name: name.to_owned(),
                help: None,
                labels: Vec::new(),
            }
        }

        fn with_help(mut self, help: &str) -> Self {
            self.help = Some(help.to_owned());
            self
        }

        fn with_label(mut self, key: &str, value: &str) -> Self {
            self.labels.push((key.to_owned(), value.to_owned()));
            self
        }
    }

    pub(crate) struct CounterBuilder<'a> {
        core: BuilderCore<'a>,
    }

    impl CounterBuilder<'_> {
        pub(crate) fn help(self, help: &str) -> Self {
            Self {
                core: self.core.with_help(help),
            }
        }

        pub(crate) fn label(self, key: &str, value: &str) -> Self {
            Self {
                core: self.core.with_label(key, value),
            }
        }

        pub(crate) fn build(self) -> Result<Arc<Counter>, MetricsError> {
            self.core
                .registry
                .register_counter(self.core.name, self.core.help, self.core.labels)
        }
    }

    pub(crate) struct GaugeBuilder<'a> {
        core: BuilderCore<'a>,
    }

    impl GaugeBuilder<'_> {
        pub(crate) fn help(self, help: &str) -> Self {
            Self {
                core: self.core.with_help(help),
            }
        }

        pub(crate) fn label(self, key: &str, value: &str) -> Self {
            Self {
                core: self.core.with_label(key, value),
            }
        }

        pub(crate) fn build(self) -> Result<Arc<Gauge>, MetricsError> {
            self.core
                .registry
                .register_gauge(self.core.name, self.core.help, self.core.labels)
        }
    }

    pub(crate) struct HistogramBuilder<'a> {
        core: BuilderCore<'a>,
        buckets: Vec<f64>,
    }

    impl HistogramBuilder<'_> {
        pub(crate) fn help(self, help: &str) -> Self {
            Self {
                core: self.core.with_help(help),
                buckets: self.buckets,
            }
        }

        pub(crate) fn label(self, key: &str, value: &str) -> Self {
            Self {
                core: self.core.with_label(key, value),
                buckets: self.buckets,
            }
        }

        pub(crate) fn buckets(mut self, boundaries: &[f64]) -> Self {
            self.buckets = boundaries.to_vec();
            self
        }

        pub(crate) fn build(self) -> Result<Arc<Histogram>, MetricsError> {
            self.core.registry.register_histogram(
                self.core.name,
                self.core.help,
                self.core.labels,
                self.buckets,
            )
        }
    }

    fn canonical_labels(mut labels: Labels) -> Labels {
        labels.sort_unstable();
        labels
    }

    fn label_names(labels: &Labels) -> Vec<String> {
        let mut names: Vec<_> = labels.iter().map(|(name, _)| name.clone()).collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn validate_metric_name(name: &str, labels: &Labels) -> Result<(), MetricsError> {
        if is_valid_metric_name(name) {
            Ok(())
        } else {
            Err(MetricsError::InvalidName {
                name: name.to_owned(),
                labelset: labels.clone(),
            })
        }
    }

    fn require_help(
        name: &str,
        labels: &Labels,
        help: Option<String>,
    ) -> Result<String, MetricsError> {
        match help {
            Some(help) if !help.is_empty() => Ok(help),
            _ => Err(MetricsError::MissingHelp {
                name: name.to_owned(),
                labelset: labels.clone(),
            }),
        }
    }

    fn validate_family_metadata(
        inner: &RegistryInner,
        name: &str,
        metric_type: MetricType,
        help: &str,
        requested_label_names: &[String],
    ) -> Result<Option<usize>, MetricsError> {
        let Some((index, family)) = inner
            .families
            .iter()
            .enumerate()
            .find(|(_, family)| family.name == name)
        else {
            return Ok(None);
        };
        if family.kind.metric_type() == metric_type
            && family.help == help
            && family.label_names == requested_label_names
        {
            return Ok(Some(index));
        }

        Err(MetricsError::MetadataConflict {
            name: name.to_owned(),
            existing: metadata_diagnostic(
                family.kind.metric_type(),
                &family.help,
                &family.label_names,
            ),
            requested: metadata_diagnostic(metric_type, help, requested_label_names),
        })
    }

    fn metadata_diagnostic(metric_type: MetricType, help: &str, label_names: &[String]) -> String {
        format!("type={metric_type:?}, help={help:?}, label_names={label_names:?}")
    }

    fn insert_series_key(
        inner: &mut RegistryInner,
        name: &str,
        labels: &Labels,
    ) -> Result<(), MetricsError> {
        let key = SeriesKey {
            name: name.to_owned(),
            labelset: labels.clone(),
        };
        if inner.series_keys.insert(key) {
            Ok(())
        } else {
            Err(MetricsError::DuplicateSeries {
                name: name.to_owned(),
                labelset: labels.clone(),
            })
        }
    }

    fn is_valid_metric_name(name: &str) -> bool {
        let mut bytes = name.bytes();
        let Some(first) = bytes.next() else {
            return false;
        };
        matches!(first, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b':')
            && bytes.all(|byte| {
                matches!(
                    byte,
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b':'
                )
            })
    }

    fn snapshot_series(kind: &FamilyKind) -> Vec<SeriesSnapshot> {
        match kind {
            FamilyKind::Counter(series) => series
                .iter()
                .map(|series| SeriesSnapshot::Value {
                    labels: labels_map(&series.labels),
                    value: series.handle.value.load(Ordering::Relaxed),
                })
                .collect(),
            FamilyKind::Gauge(series) => series
                .iter()
                .map(|series| SeriesSnapshot::Value {
                    labels: labels_map(&series.labels),
                    value: series.handle.value.load(Ordering::Relaxed),
                })
                .collect(),
            FamilyKind::Histogram(series) => series
                .iter()
                .map(|series| {
                    // Fold the raw per-bucket counts into the
                    // cumulative shape Prometheus expects (see the
                    // `Histogram` doc for why storage stays raw).
                    let mut cumulative = 0;
                    let buckets = series
                        .handle
                        .boundaries
                        .iter()
                        .zip(&series.handle.bucket_counts)
                        .map(|(boundary, count)| {
                            cumulative += count.load(Ordering::Relaxed);
                            (*boundary, cumulative)
                        })
                        .collect();
                    SeriesSnapshot::Histogram {
                        labels: labels_map(&series.labels),
                        buckets,
                        sum: f64::from_bits(series.handle.sum_bits.load(Ordering::Relaxed)),
                        count: series.handle.count.load(Ordering::Relaxed),
                    }
                })
                .collect(),
        }
    }

    fn labels_map(labels: &Labels) -> BTreeMap<String, String> {
        labels.iter().cloned().collect()
    }
}

#[allow(unused_imports)]
pub(crate) use registry_core::{MetricsError, MetricsSnapshot, Registry};

#[cfg(test)]
mod registry_tests {
    use super::{MetricsError, MetricsSnapshot, Registry};
    use serde_json::Value;

    fn build_ok<T>(result: Result<T, MetricsError>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("metric registration failed: {error}"),
        }
    }

    fn registration_error<T>(result: Result<T, MetricsError>) -> String {
        match result {
            Ok(_) => panic!("metric registration unexpectedly succeeded"),
            Err(error) => error.to_string(),
        }
    }

    fn assert_missing_help<T>(
        result: Result<T, MetricsError>,
        expected_name: &str,
        expected_label: (&str, &str),
    ) {
        let error = match result {
            Ok(_) => panic!("metric registration unexpectedly succeeded without help"),
            Err(error) => error,
        };
        let diagnostic = error.to_string();
        match error {
            MetricsError::MissingHelp { name, labelset } => {
                assert_eq!(name, expected_name);
                assert!(
                    labelset.contains(&(expected_label.0.to_owned(), expected_label.1.to_owned()))
                );
            }
            other => panic!("expected MissingHelp, got {other:?}"),
        }
        assert!(diagnostic.contains(expected_name));
        assert!(diagnostic.contains(expected_label.0));
        assert!(diagnostic.contains(expected_label.1));
    }

    fn metadata_error<T>(result: Result<T, MetricsError>) -> String {
        match result {
            Ok(_) => panic!("metric registration unexpectedly accepted conflicting metadata"),
            Err(error @ MetricsError::MetadataConflict { .. }) => error.to_string(),
            Err(other) => panic!("expected MetadataConflict, got {other:?}"),
        }
    }

    fn snapshot_json(registry: &Registry) -> Value {
        let snapshot: MetricsSnapshot = registry.snapshot();
        serde_json::to_value(snapshot).expect("snapshot must serialize")
    }

    fn metric<'a>(snapshot: &'a Value, name: &str) -> &'a Value {
        snapshot["metrics"]
            .as_array()
            .expect("metrics must be an array")
            .iter()
            .find(|metric| metric["name"] == name)
            .unwrap_or_else(|| panic!("missing metric {name}"))
    }

    #[test]
    fn counter_gauge_and_histogram_handles_update_snapshot_values() {
        let registry = Registry::new();
        let counter = build_ok(
            registry
                .counter("limpid_test_events_total")
                .help("Test events.")
                .label("source", "unit")
                .build(),
        );
        let gauge = build_ok(
            registry
                .gauge("limpid_test_queue_depth")
                .help("Test queue depth.")
                .label("output", "unit")
                .build(),
        );
        let histogram = build_ok(
            registry
                .histogram("limpid_test_latency_seconds")
                .help("Test latency.")
                .buckets(&[0.1, 1.0])
                .label("peer", "unit")
                .build(),
        );

        counter.inc();
        counter.inc();
        gauge.set(7);
        histogram.observe(0.05);
        histogram.observe(0.1);
        histogram.observe(0.5);
        histogram.observe(1.0);
        histogram.observe(2.0);

        let snapshot = snapshot_json(&registry);
        assert_eq!(snapshot["schema"], 1);

        let counter = metric(&snapshot, "limpid_test_events_total");
        assert_eq!(counter["type"], "counter");
        assert_eq!(counter["help"], "Test events.");
        assert_eq!(counter["series"][0]["labels"]["source"], "unit");
        assert_eq!(counter["series"][0]["value"], 2);

        let gauge = metric(&snapshot, "limpid_test_queue_depth");
        assert_eq!(gauge["type"], "gauge");
        assert_eq!(gauge["help"], "Test queue depth.");
        assert_eq!(gauge["series"][0]["labels"]["output"], "unit");
        assert_eq!(gauge["series"][0]["value"], 7);

        let histogram = metric(&snapshot, "limpid_test_latency_seconds");
        assert_eq!(histogram["type"], "histogram");
        assert_eq!(histogram["help"], "Test latency.");
        assert_eq!(histogram["series"][0]["labels"]["peer"], "unit");
        assert_eq!(
            histogram["series"][0]["buckets"],
            serde_json::json!([[0.1, 2], [1.0, 4]])
        );
        assert_eq!(histogram["series"][0]["count"], 5);
        let sum = histogram["series"][0]["sum"]
            .as_f64()
            .expect("histogram sum must be a number");
        assert!((sum - 3.65).abs() < 1e-12);
    }

    #[test]
    fn counter_updates_are_not_lost_across_threads() {
        const THREADS: usize = 4;
        const INCREMENTS_PER_THREAD: usize = 1_000;

        let registry = Registry::new();
        let counter = build_ok(
            registry
                .counter("limpid_test_concurrent_events_total")
                .help("Concurrent test events.")
                .label("source", "unit")
                .build(),
        );
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    for _ in 0..INCREMENTS_PER_THREAD {
                        counter.inc();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("counter worker must not panic");
        }

        let snapshot = snapshot_json(&registry);
        let counter = metric(&snapshot, "limpid_test_concurrent_events_total");
        assert_eq!(
            counter["series"][0]["value"],
            (THREADS * INCREMENTS_PER_THREAD) as u64
        );
    }

    #[test]
    fn registration_rejects_invalid_metric_name_with_diagnostic_context() {
        let registry = Registry::new();
        let error = registration_error(
            registry
                .counter("9invalid_metric_name")
                .help("Invalid-name test metric.")
                .label("source", "unit")
                .build(),
        );

        assert!(error.contains("9invalid_metric_name"));
        assert!(error.contains("source"));
        assert!(error.contains("unit"));
    }

    #[test]
    fn every_builder_rejects_missing_help() {
        let registry = Registry::new();
        assert_missing_help(
            registry
                .counter("limpid_test_missing_counter_help_total")
                .label("source", "counter")
                .build(),
            "limpid_test_missing_counter_help_total",
            ("source", "counter"),
        );
        assert_missing_help(
            registry
                .gauge("limpid_test_missing_gauge_help")
                .label("source", "gauge")
                .build(),
            "limpid_test_missing_gauge_help",
            ("source", "gauge"),
        );
        assert_missing_help(
            registry
                .histogram("limpid_test_missing_histogram_help_seconds")
                .buckets(&[0.1, 1.0])
                .label("source", "histogram")
                .build(),
            "limpid_test_missing_histogram_help_seconds",
            ("source", "histogram"),
        );
    }

    #[test]
    fn counter_builder_rejects_empty_help() {
        let registry = Registry::new();
        assert_missing_help(
            registry
                .counter("limpid_test_empty_counter_help_total")
                .help("")
                .label("source", "counter")
                .build(),
            "limpid_test_empty_counter_help_total",
            ("source", "counter"),
        );
    }

    #[test]
    fn same_name_rejects_type_help_and_label_name_conflicts() {
        let registry = Registry::new();

        build_ok(
            registry
                .counter("limpid_test_type_conflict_total")
                .help("Type conflict metric.")
                .label("source", "one")
                .build(),
        );
        let type_error = metadata_error(
            registry
                .gauge("limpid_test_type_conflict_total")
                .help("Type conflict metric.")
                .label("source", "two")
                .build(),
        );
        assert!(type_error.contains("limpid_test_type_conflict_total"));
        assert!(type_error.contains("Counter"));
        assert!(type_error.contains("Gauge"));

        build_ok(
            registry
                .counter("limpid_test_help_conflict_total")
                .help("Original help.")
                .label("source", "one")
                .build(),
        );
        let help_error = metadata_error(
            registry
                .counter("limpid_test_help_conflict_total")
                .help("Conflicting help.")
                .label("source", "two")
                .build(),
        );
        assert!(help_error.contains("limpid_test_help_conflict_total"));
        assert!(help_error.contains("Original help."));
        assert!(help_error.contains("Conflicting help."));

        build_ok(
            registry
                .counter("limpid_test_label_conflict_total")
                .help("Label conflict metric.")
                .label("source", "one")
                .build(),
        );
        let label_error = metadata_error(
            registry
                .counter("limpid_test_label_conflict_total")
                .help("Label conflict metric.")
                .label("peer", "two")
                .build(),
        );
        assert!(label_error.contains("limpid_test_label_conflict_total"));
        assert!(label_error.contains("source"));
        assert!(label_error.contains("peer"));
    }

    #[test]
    fn label_name_order_does_not_split_a_metric_family() {
        let registry = Registry::new();
        build_ok(
            registry
                .counter("limpid_test_label_name_order_total")
                .help("Label-name order metric.")
                .label("region", "west")
                .label("source", "one")
                .build(),
        );
        build_ok(
            registry
                .counter("limpid_test_label_name_order_total")
                .help("Label-name order metric.")
                .label("source", "two")
                .label("region", "east")
                .build(),
        );

        let snapshot = snapshot_json(&registry);
        let matching_metrics: Vec<_> = snapshot["metrics"]
            .as_array()
            .expect("metrics must be an array")
            .iter()
            .filter(|metric| metric["name"] == "limpid_test_label_name_order_total")
            .collect();
        assert_eq!(matching_metrics.len(), 1);
        assert_eq!(
            matching_metrics[0]["series"]
                .as_array()
                .expect("series must be an array")
                .len(),
            2
        );
    }

    #[test]
    fn registration_rejects_duplicate_name_and_labelset() {
        let registry = Registry::new();
        build_ok(
            registry
                .counter("limpid_test_duplicate_total")
                .help("Duplicate test metric.")
                .label("source", "unit")
                .build(),
        );
        let error = registration_error(
            registry
                .counter("limpid_test_duplicate_total")
                .help("Duplicate test metric.")
                .label("source", "unit")
                .build(),
        );

        assert!(error.contains("limpid_test_duplicate_total"));
        assert!(error.contains("source"));
        assert!(error.contains("unit"));
    }

    #[test]
    fn duplicate_detection_is_independent_of_label_order() {
        let registry = Registry::new();
        build_ok(
            registry
                .counter("limpid_test_order_total")
                .help("Label-order test metric.")
                .label("pipeline", "main")
                .label("step", "parse")
                .build(),
        );
        let error = registration_error(
            registry
                .counter("limpid_test_order_total")
                .help("Label-order test metric.")
                .label("step", "parse")
                .label("pipeline", "main")
                .build(),
        );

        assert!(error.contains("limpid_test_order_total"));
        assert!(error.contains("pipeline"));
        assert!(error.contains("main"));
        assert!(error.contains("step"));
        assert!(error.contains("parse"));
    }

    #[test]
    fn same_name_with_a_different_labelset_registers_a_distinct_series() {
        let registry = Registry::new();
        let one = build_ok(
            registry
                .counter("limpid_test_partitioned_total")
                .help("Partitioned test metric.")
                .label("source", "one")
                .build(),
        );
        let two = build_ok(
            registry
                .counter("limpid_test_partitioned_total")
                .help("Partitioned test metric.")
                .label("source", "two")
                .build(),
        );
        one.inc();
        two.inc();
        two.inc();

        let snapshot = snapshot_json(&registry);
        let series = metric(&snapshot, "limpid_test_partitioned_total")["series"]
            .as_array()
            .expect("series must be an array");
        assert_eq!(series.len(), 2);
        let values_by_source: std::collections::HashMap<_, _> = series
            .iter()
            .map(|series| {
                (
                    series["labels"]["source"]
                        .as_str()
                        .expect("source label must be a string"),
                    series["value"]
                        .as_u64()
                        .expect("counter value must be an integer"),
                )
            })
            .collect();
        assert_eq!(values_by_source.get("one"), Some(&1));
        assert_eq!(values_by_source.get("two"), Some(&2));
    }
}

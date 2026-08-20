//! Shared self-describing metrics registry and fully-labelled handles.
//!
//! Metric-emitting inputs, the pipeline worker, and outputs hold
//! their corresponding per-role bundle (`InputMetrics`,
//! `PipelineMetrics`, `OutputMetrics`); each bundle's `register`
//! helper materialises its canonical fully-labelled counter series
//! in the shared registry.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) use limpid_metrics_schema::MetricsSnapshot;
use limpid_metrics_schema::{
    DROPPED_ROOT_PROCESS_NAME, DROPPED_ROOT_STEP, EVENTS_DROPPED_TOTAL, HistogramSeries,
    MetricFamily as SnapshotFamily, PROCESS_EVENTS_ERRORED_TOTAL, PROCESS_EVENTS_IN_TOTAL,
    PROCESS_EVENTS_OUT_TOTAL, PROCESS_LABEL_NAME, PROCESS_LABEL_PATH, PROCESS_LABEL_PIPELINE,
    PROCESS_LABEL_STEP, PROCESS_PATH_ROOT, ValueSeries as SnapshotValueSeries,
};

pub(crate) fn register_build_info(
    registry: &Registry,
    version: &str,
    node_id: &str,
) -> Result<(), MetricsError> {
    let build_info = registry
        .gauge("limpid_build_info")
        .label("version", version)
        .label("node_id", node_id)
        .help("Build information for the running limpid node.")
        .build()?;
    build_info.set(1);
    Ok(())
}

pub(crate) const LTP_HOP_LATENCY_BUCKETS: [f64; 8] =
    [0.0001, 0.001, 0.005, 0.025, 0.1, 0.5, 2.5, 10.0];

#[derive(Clone)]
pub(crate) struct LtpPeerMetrics {
    pub(crate) network_latency: Arc<registry_core::Histogram>,
    pub(crate) intra_latency: Arc<registry_core::Histogram>,
    pub(crate) negative_delta: Arc<registry_core::Counter>,
    pub(crate) loop_dropped: Arc<registry_core::Counter>,
}

pub(crate) struct LtpMetrics {
    peers: BTreeMap<String, LtpPeerMetrics>,
    pub(crate) rejected_unknown_peer: Arc<registry_core::Counter>,
}

impl LtpMetrics {
    pub(crate) fn register(
        registry: &Registry,
        peers: &BTreeSet<String>,
    ) -> Result<Arc<Self>, MetricsError> {
        let rejected_unknown_peer = registry
            .counter("limpid_ltp_rejected_unknown_peer_total")
            .help("Total LTP peer connection attempts rejected for an undeclared key or mismatched node identity.")
            .build()?;
        let mut handles = BTreeMap::new();
        for peer in peers {
            let histogram = |segment| {
                registry
                    .histogram("limpid_ltp_hop_latency_seconds")
                    .label("peer", peer)
                    .label("segment", segment)
                    .help("LTP hop latency between authenticated peers.")
                    .buckets(&LTP_HOP_LATENCY_BUCKETS)
                    .build()
            };
            let counter = |name, help| {
                registry
                    .counter(name)
                    .label("peer", peer)
                    .help(help)
                    .build()
            };
            handles.insert(
                peer.clone(),
                LtpPeerMetrics {
                    network_latency: histogram("network")?,
                    intra_latency: histogram("intra")?,
                    negative_delta: counter(
                        "limpid_ltp_negative_delta_total",
                        "Total negative cross-host LTP latency deltas clamped to zero.",
                    )?,
                    loop_dropped: counter(
                        "limpid_ltp_loop_dropped_total",
                        "Total LTP events dropped because of a cycle or hop limit.",
                    )?,
                },
            );
        }
        Ok(Arc::new(Self {
            peers: handles,
            rejected_unknown_peer,
        }))
    }

    pub(crate) fn peer(&self, peer: &str) -> Option<LtpPeerMetrics> {
        self.peers.get(peer).cloned()
    }
}

pub struct InputMetrics {
    /// Events actually received by the input module (network, socket, file, etc).
    /// Injected events are NOT counted here — see `events_injected`.
    pub(crate) events_received: Arc<registry_core::Counter>,
    pub(crate) events_invalid: Arc<registry_core::Counter>,
    /// Events pushed into this input's channel via `limpidctl inject`.
    pub(crate) events_injected: Arc<registry_core::Counter>,
    pub(crate) bytes_received: Arc<registry_core::Counter>,
}

impl InputMetrics {
    pub(crate) fn register(registry: &Registry, input: &str) -> Result<Arc<Self>, MetricsError> {
        macro_rules! counter {
            ($name:literal, $help:literal) => {
                registry
                    .counter($name)
                    .label("input", input)
                    .help($help)
                    .build()?
            };
        }
        Ok(Arc::new(Self {
            events_received: counter!(
                "limpid_input_events_received_total",
                "Total events received by the input."
            ),
            events_invalid: counter!(
                "limpid_input_events_invalid_total",
                "Total invalid events rejected by the input."
            ),
            events_injected: counter!(
                "limpid_input_events_injected_total",
                "Total events injected into the input through the control socket."
            ),
            bytes_received: counter!(
                "limpid_input_bytes_received_total",
                "Total logical bytes received by the input adapter before validation."
            ),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_testing() -> Arc<Self> {
        Self::register(&Registry::new(), "test-input").expect("test input metrics must register")
    }
}

pub struct PipelineMetrics {
    pub(crate) events_received: Arc<registry_core::Counter>,
    pub(crate) events_finished: Arc<registry_core::Counter>,
    pub(crate) events_dropped: Arc<registry_core::Counter>,
    pub(crate) events_discarded: Arc<registry_core::Counter>,
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
    pub(crate) events_errored: Arc<registry_core::Counter>,
    /// Subset of `events_errored` for which the configured
    /// `error_log` write itself failed (disk full, permissions,
    /// rotation race). The runtime falls back to a structured
    /// `tracing::error!` line, but operators should alarm on this
    /// counter — it means the replay path may be incomplete.
    pub(crate) events_errored_unwritable: Arc<registry_core::Counter>,
    pub(crate) inflight: Arc<registry_core::Gauge>,
}

impl PipelineMetrics {
    pub(crate) fn register(registry: &Registry, pipeline: &str) -> Result<Arc<Self>, MetricsError> {
        macro_rules! counter {
            ($name:literal, $help:literal) => {
                registry
                    .counter($name)
                    .label("pipeline", pipeline)
                    .help($help)
                    .build()?
            };
        }
        macro_rules! gauge {
            ($name:literal, $help:literal) => {
                registry
                    .gauge($name)
                    .label("pipeline", pipeline)
                    .help($help)
                    .build()?
            };
        }
        Ok(Arc::new(Self {
            events_received: counter!(
                "limpid_pipeline_events_received_total",
                "Total events received by the pipeline."
            ),
            events_finished: counter!(
                "limpid_pipeline_events_finished_total",
                "Total events that finished pipeline processing."
            ),
            events_dropped: registry
                .counter(EVENTS_DROPPED_TOTAL)
                .label(PROCESS_LABEL_PIPELINE, pipeline)
                .label(PROCESS_LABEL_STEP, DROPPED_ROOT_STEP)
                .label(PROCESS_LABEL_PATH, PROCESS_PATH_ROOT)
                .label(PROCESS_LABEL_NAME, DROPPED_ROOT_PROCESS_NAME)
                .help("Total events whose drop propagated through this processing node.")
                .build()?,
            events_discarded: counter!(
                "limpid_pipeline_events_discarded_total",
                "Total events discarded by pipeline routing."
            ),
            events_errored: counter!(
                "limpid_pipeline_events_errored_total",
                "Total events that encountered a pipeline processing error."
            ),
            events_errored_unwritable: counter!(
                "limpid_pipeline_events_errored_unwritable_total",
                "Total pipeline errors whose recovery record could not be written."
            ),
            inflight: gauge!(
                "limpid_pipeline_inflight",
                "Pipeline executions currently in progress, including terminal bookkeeping."
            ),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_testing() -> Arc<Self> {
        Self::register(&Registry::new(), "test-pipeline")
            .expect("test pipeline metrics must register")
    }
}

#[derive(Clone)]
pub(crate) struct ProcessCounters {
    incoming: Arc<registry_core::Counter>,
    outgoing: Arc<registry_core::Counter>,
    dropped: Arc<registry_core::Counter>,
    errored: Arc<registry_core::Counter>,
}

impl ProcessCounters {
    pub(crate) fn register(
        registry: &Registry,
        pipeline: &str,
        step: usize,
        process_path: &str,
        process_name: &str,
    ) -> Result<Self, MetricsError> {
        let step = step.to_string();
        let counter = |name, help| {
            registry
                .counter(name)
                .label(PROCESS_LABEL_PIPELINE, pipeline)
                .label(PROCESS_LABEL_STEP, &step)
                .label(PROCESS_LABEL_PATH, process_path)
                .label(PROCESS_LABEL_NAME, process_name)
                .help(help)
                .build()
        };
        Ok(Self {
            incoming: counter(
                PROCESS_EVENTS_IN_TOTAL,
                "Total process invocation frames started.",
            )?,
            outgoing: counter(
                PROCESS_EVENTS_OUT_TOTAL,
                "Total process invocation frames that continued successfully.",
            )?,
            dropped: counter(
                EVENTS_DROPPED_TOTAL,
                "Total events whose drop propagated through this processing node.",
            )?,
            errored: counter(
                PROCESS_EVENTS_ERRORED_TOTAL,
                "Total process invocation frames terminated by an error.",
            )?,
        })
    }

    pub(crate) fn start(&self) {
        self.incoming.inc();
    }

    pub(crate) fn continued(&self) {
        self.outgoing.inc();
    }

    pub(crate) fn dropped(&self) {
        self.dropped.inc();
    }

    pub(crate) fn errored(&self) {
        self.errored.inc();
    }
}

pub struct OutputMetrics {
    /// Total events that entered this output's queue (from pipelines + injects).
    /// `events_received - events_injected` = events delivered via pipelines.
    pub(crate) events_received: Arc<registry_core::Counter>,
    /// Events pushed into this output's queue via `limpidctl inject`.
    pub(crate) events_injected: Arc<registry_core::Counter>,
    pub(crate) events_written: Arc<registry_core::Counter>,
    pub(crate) events_failed: Arc<registry_core::Counter>,
    pub(crate) retries: Arc<registry_core::Counter>,
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
    pub(crate) events_wedged: Arc<registry_core::Counter>,
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
    pub(crate) events_errored_unwritable: Arc<registry_core::Counter>,
    pub(crate) bytes_written: Arc<registry_core::Counter>,
    pub(crate) queue_depth: Arc<registry_core::Gauge>,
    pub(crate) in_retry: Arc<registry_core::Gauge>,
}

impl OutputMetrics {
    pub(crate) fn register(registry: &Registry, output: &str) -> Result<Arc<Self>, MetricsError> {
        macro_rules! counter {
            ($name:literal, $help:literal) => {
                registry
                    .counter($name)
                    .label("output", output)
                    .help($help)
                    .build()?
            };
        }
        macro_rules! gauge {
            ($name:literal, $help:literal) => {
                registry
                    .gauge($name)
                    .label("output", output)
                    .help($help)
                    .build()?
            };
        }
        Ok(Arc::new(Self {
            events_received: counter!(
                "limpid_output_events_received_total",
                "Total events received by the output queue."
            ),
            events_injected: counter!(
                "limpid_output_events_injected_total",
                "Total events injected into the output through the control socket."
            ),
            events_written: counter!(
                "limpid_output_events_written_total",
                "Total events successfully written by the output."
            ),
            events_failed: counter!(
                "limpid_output_events_failed_total",
                "Total events that reached a terminal failure disposition for this output."
            ),
            retries: counter!("limpid_output_retries_total", "Total output write retries."),
            events_wedged: counter!(
                "limpid_output_events_wedged_total",
                "Total disk queue consumers wedged after an unrecoverable drop."
            ),
            events_errored_unwritable: counter!(
                "limpid_output_events_errored_unwritable_total",
                "Total output errors whose recovery record could not be written."
            ),
            bytes_written: counter!(
                "limpid_output_bytes_written_total",
                "Total logical bytes whose transfer was confirmed by the output adapter."
            ),
            queue_depth: gauge!(
                "limpid_output_queue_depth",
                "Current unread or unacknowledged depth of the output queue."
            ),
            in_retry: gauge!(
                "limpid_output_in_retry",
                "Whether this output currently has an active retry cycle."
            ),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_testing() -> Arc<Self> {
        Self::register(&Registry::new(), "test-output").expect("test output metrics must register")
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
        /// Configured boundaries are rejected rather than repaired:
        /// every bound must be finite (`NaN` cannot participate in a
        /// total numeric ordering, and `+Inf` duplicates the implicit
        /// `+Inf` bucket), and strictly ascending order is required
        /// so the cumulative shape assembled by `snapshot_series` is
        /// well defined.
        InvalidBuckets {
            name: String,
            labelset: Vec<(String, String)>,
        },
        /// The wire schema identifies each series by its exported
        /// label map, which keys labels by name; a registration
        /// carrying duplicate label names would collapse to a map
        /// indistinguishable from other layouts. Registration is
        /// rejected instead of exporting the ambiguity.
        DuplicateLabelName {
            name: String,
            labelset: Vec<(String, String)>,
            label: String,
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
        /// Defensive guard for callers that compile process metrics
        /// without first running the config analyzer.
        ProcessCallCycle { path: Vec<String> },
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
                Self::InvalidBuckets { name, labelset } => write!(
                    formatter,
                    "invalid histogram buckets: name={name:?}, labelset={labelset:?}; boundaries must be finite and strictly increasing"
                ),
                Self::DuplicateLabelName {
                    name,
                    labelset,
                    label,
                } => write!(
                    formatter,
                    "duplicate metric label name: name={name:?}, labelset={labelset:?}, label={label:?}"
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
                Self::ProcessCallCycle { path } => write!(
                    formatter,
                    "process call cycle reached metric compilation: {}",
                    path.join(" -> ")
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

        pub(crate) fn inc_by(&self, value: u64) {
            self.value.fetch_add(value, Ordering::Relaxed);
        }

        #[cfg(test)]
        pub(crate) fn load(&self, _ordering: Ordering) -> u64 {
            self.value.load(Ordering::Relaxed)
        }
    }

    pub(crate) struct Gauge {
        value: AtomicU64,
    }

    impl Gauge {
        pub(crate) fn set(&self, value: u64) {
            self.value.store(value, Ordering::Relaxed);
        }

        pub(crate) fn inc(&self) {
            self.value.fetch_add(1, Ordering::Relaxed);
        }

        /// Decrement by one, saturating at zero. `fetch_update`
        /// carries the concurrent CAS, and `checked_sub` keeps an
        /// underflow at zero instead of wrapping to `u64::MAX`; the
        /// `debug_assert!` surfaces a lifecycle imbalance loudly in
        /// debug builds.
        pub(crate) fn dec(&self) {
            let updated = self
                .value
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_sub(1)
                });
            debug_assert!(updated.is_ok(), "gauge decrement underflow");
        }

        #[cfg(test)]
        pub(crate) fn load(&self, _ordering: Ordering) -> u64 {
            self.value.load(Ordering::Relaxed)
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

        #[cfg(test)]
        pub(crate) fn count(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        #[cfg(test)]
        pub(crate) fn sum(&self) -> f64 {
            f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum MetricType {
        Counter,
        Gauge,
        Histogram,
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
            let metrics = inner.families.iter().map(snapshot_family).collect();
            MetricsSnapshot::new(metrics)
        }

        fn register_counter(
            &self,
            name: String,
            help: Option<String>,
            labels: Labels,
        ) -> Result<Arc<Counter>, MetricsError> {
            let labels = canonical_labels(labels);
            validate_unique_label_names(&name, &labels)?;
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
            validate_unique_label_names(&name, &labels)?;
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
            validate_unique_label_names(&name, &labels)?;
            validate_metric_name(&name, &labels)?;
            let help = require_help(&name, &labels, help)?;
            validate_histogram_boundaries(&name, &labels, &boundaries)?;
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

    fn validate_unique_label_names(name: &str, labels: &Labels) -> Result<(), MetricsError> {
        let duplicate = labels
            .windows(2)
            .find(|pair| pair[0].0 == pair[1].0)
            .map(|pair| pair[0].0.clone());
        match duplicate {
            Some(label) => Err(MetricsError::DuplicateLabelName {
                name: name.to_owned(),
                labelset: labels.clone(),
                label,
            }),
            None => Ok(()),
        }
    }

    fn validate_histogram_boundaries(
        name: &str,
        labels: &Labels,
        boundaries: &[f64],
    ) -> Result<(), MetricsError> {
        let all_finite = boundaries.iter().all(|boundary| boundary.is_finite());
        let strictly_increasing = boundaries.windows(2).all(|pair| pair[0] < pair[1]);
        if all_finite && strictly_increasing {
            Ok(())
        } else {
            Err(MetricsError::InvalidBuckets {
                name: name.to_owned(),
                labelset: labels.clone(),
            })
        }
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

    fn snapshot_family(family: &MetricFamily) -> SnapshotFamily {
        match &family.kind {
            FamilyKind::Counter(series) => SnapshotFamily::counter(
                family.name.clone(),
                family.help.clone(),
                snapshot_value_series(series),
            ),
            FamilyKind::Gauge(series) => SnapshotFamily::gauge(
                family.name.clone(),
                family.help.clone(),
                snapshot_value_series(series),
            ),
            FamilyKind::Histogram(series) => SnapshotFamily::histogram(
                family.name.clone(),
                family.help.clone(),
                series
                    .iter()
                    .map(|series| {
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
                        HistogramSeries::new(
                            labels_map(&series.labels),
                            buckets,
                            f64::from_bits(series.handle.sum_bits.load(Ordering::Relaxed)),
                            series.handle.count.load(Ordering::Relaxed),
                        )
                    })
                    .collect(),
            ),
        }
    }

    fn snapshot_value_series<T>(series: &[ValueSeries<T>]) -> Vec<SnapshotValueSeries>
    where
        T: ValueHandle,
    {
        series
            .iter()
            .map(|series| {
                SnapshotValueSeries::new(labels_map(&series.labels), series.handle.load_value())
            })
            .collect()
    }

    trait ValueHandle {
        fn load_value(&self) -> u64;
    }

    impl ValueHandle for Counter {
        fn load_value(&self) -> u64 {
            self.value.load(Ordering::Relaxed)
        }
    }

    impl ValueHandle for Gauge {
        fn load_value(&self) -> u64 {
            self.value.load(Ordering::Relaxed)
        }
    }

    fn labels_map(labels: &Labels) -> BTreeMap<String, String> {
        labels.iter().cloned().collect()
    }
}

#[allow(unused_imports)]
pub(crate) use registry_core::{MetricsError, Registry};

#[cfg(test)]
mod registry_tests {
    use super::{
        InputMetrics, LTP_HOP_LATENCY_BUCKETS, LtpMetrics, MetricsError, MetricsSnapshot,
        OutputMetrics, PipelineMetrics, Registry,
    };
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::sync::Arc;

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

    fn assert_invalid_buckets<T>(
        result: Result<T, MetricsError>,
        expected_name: &str,
        expected_label: (&str, &str),
    ) {
        let error = match result {
            Ok(_) => panic!("histogram registration unexpectedly accepted invalid buckets"),
            Err(error) => error,
        };
        let diagnostic = error.to_string();
        match error {
            MetricsError::InvalidBuckets { name, labelset } => {
                assert_eq!(name, expected_name);
                assert!(
                    labelset.contains(&(expected_label.0.to_owned(), expected_label.1.to_owned()))
                );
            }
            other => panic!("expected InvalidBuckets, got {other:?}"),
        }
        assert!(diagnostic.contains(expected_name));
        assert!(diagnostic.contains(expected_label.0));
        assert!(diagnostic.contains(expected_label.1));
        assert!(diagnostic.contains("finite"));
        assert!(diagnostic.contains("strictly increasing"));
    }

    fn assert_duplicate_label_name<T>(
        result: Result<T, MetricsError>,
        expected_name: &str,
        expected_labelset: &[(&str, &str)],
        expected_duplicate: &str,
    ) {
        let error = match result {
            Ok(_) => panic!("metric registration unexpectedly accepted a duplicate label name"),
            Err(error) => error,
        };
        let diagnostic = error.to_string();
        match error {
            MetricsError::DuplicateLabelName {
                name,
                labelset,
                label,
            } => {
                assert_eq!(name, expected_name);
                assert_eq!(label, expected_duplicate);
                for &(key, value) in expected_labelset {
                    assert!(labelset.contains(&(key.to_owned(), value.to_owned())));
                }
            }
            other => panic!("expected DuplicateLabelName, got {other:?}"),
        }
        assert!(diagnostic.contains(expected_name));
        assert!(diagnostic.contains(expected_duplicate));
        for &(key, value) in expected_labelset {
            assert!(diagnostic.contains(key));
            assert!(diagnostic.contains(value));
        }
    }

    fn snapshot_json(registry: &Registry) -> Value {
        let snapshot: MetricsSnapshot = registry.snapshot();
        serde_json::to_value(snapshot).expect("snapshot must serialize")
    }

    #[test]
    fn registry_snapshot_matches_the_shared_wire_dto() {
        let registry = Registry::new();
        build_ok(
            registry
                .counter("limpid_test_events_total")
                .help("Test events.")
                .label("source", "unit")
                .build(),
        )
        .inc();

        let shared: limpid_metrics_schema::MetricsSnapshot = registry.snapshot();
        assert_eq!(
            serde_json::to_string(&shared).unwrap(),
            r#"{"schema":1,"metrics":[{"name":"limpid_test_events_total","type":"counter","help":"Test events.","series":[{"labels":{"source":"unit"},"value":1}]}]}"#
        );
        let produced = serde_json::to_value(&shared).unwrap();
        assert_eq!(produced["schema"], 1);
        assert_eq!(produced["metrics"][0]["name"], "limpid_test_events_total");
    }

    #[test]
    fn build_info_is_one_prepopulated_gauge_with_fixed_labels() {
        let registry = Registry::new();
        super::register_build_info(&registry, "0.7.15", "node-a")
            .expect("build info must register");

        let snapshot = snapshot_json(&registry);
        let family = metric(&snapshot, "limpid_build_info");
        assert_eq!(family["type"], "gauge");
        assert_eq!(
            family["help"],
            "Build information for the running limpid node."
        );
        assert_eq!(
            family["series"],
            serde_json::json!([{
                "labels": {"node_id": "node-a", "version": "0.7.15"},
                "value": 1
            }])
        );
        assert_eq!(snapshot["metrics"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ltp_metrics_prepopulate_the_static_peer_union_with_exact_metadata() {
        let registry = Registry::new();
        let peers = BTreeSet::from([
            "peer-b".to_owned(),
            "peer-a".to_owned(),
            "peer-a".to_owned(),
        ]);
        let metrics = LtpMetrics::register(&registry, &peers).unwrap();

        let first_peer_a = metrics.peer("peer-a").unwrap();
        let second_peer_a = metrics.peer("peer-a").unwrap();
        assert!(Arc::ptr_eq(
            &first_peer_a.network_latency,
            &second_peer_a.network_latency
        ));
        assert!(Arc::ptr_eq(
            &first_peer_a.intra_latency,
            &second_peer_a.intra_latency
        ));
        assert!(Arc::ptr_eq(
            &first_peer_a.negative_delta,
            &second_peer_a.negative_delta
        ));
        assert!(Arc::ptr_eq(
            &first_peer_a.loop_dropped,
            &second_peer_a.loop_dropped
        ));
        assert!(metrics.peer("peer-b").is_some());
        assert!(metrics.peer("unknown").is_none());

        let snapshot = snapshot_json(&registry);
        let latency = metric(&snapshot, "limpid_ltp_hop_latency_seconds");
        assert_eq!(latency["type"], "histogram");
        assert_eq!(
            latency["help"],
            "LTP hop latency between authenticated peers."
        );
        let series = latency["series"].as_array().unwrap();
        assert_eq!(series.len(), 4);
        assert_eq!(
            series
                .iter()
                .map(|series| (
                    series["labels"]["peer"].as_str().unwrap(),
                    series["labels"]["segment"].as_str().unwrap(),
                    series["count"].as_u64().unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("peer-a", "network", 0),
                ("peer-a", "intra", 0),
                ("peer-b", "network", 0),
                ("peer-b", "intra", 0),
            ]
        );
        for series in series {
            let buckets = series["buckets"].as_array().unwrap();
            assert_eq!(buckets.len(), LTP_HOP_LATENCY_BUCKETS.len());
            for (actual, expected) in buckets.iter().zip(LTP_HOP_LATENCY_BUCKETS) {
                assert_eq!(actual[0].as_f64().unwrap(), expected);
                assert_eq!(actual[1], 0);
            }
        }

        for (name, labels) in [
            ("limpid_ltp_negative_delta_total", 2),
            ("limpid_ltp_loop_dropped_total", 2),
            ("limpid_ltp_rejected_unknown_peer_total", 1),
        ] {
            let family = metric(&snapshot, name);
            assert_eq!(family["type"], "counter");
            assert_eq!(family["series"].as_array().unwrap().len(), labels);
            assert!(
                family["series"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|s| s["value"] == 0)
            );
        }
        assert!(
            metric(&snapshot, "limpid_ltp_rejected_unknown_peer_total")["series"][0]["labels"]
                .as_object()
                .unwrap()
                .is_empty()
        );
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
    fn histogram_builder_validates_finite_strictly_increasing_boundaries() {
        let invalid = [
            ("nan", vec![f64::NAN]),
            ("positive_infinity", vec![f64::INFINITY]),
            ("negative_infinity", vec![f64::NEG_INFINITY]),
            ("descending", vec![1.0, 0.5]),
            ("equal", vec![0.5, 0.5]),
        ];
        for (case, boundaries) in invalid {
            let registry = Registry::new();
            let name = format!("limpid_test_invalid_{case}_seconds");
            assert_invalid_buckets(
                registry
                    .histogram(&name)
                    .help("Invalid histogram boundaries test.")
                    .label("source", case)
                    .buckets(&boundaries)
                    .build(),
                &name,
                ("source", case),
            );
        }

        let registry = Registry::new();
        build_ok(
            registry
                .histogram("limpid_test_empty_buckets_seconds")
                .help("Empty histogram boundaries test.")
                .label("source", "empty")
                .buckets(&[])
                .build(),
        );
        build_ok(
            registry
                .histogram("limpid_test_single_bucket_seconds")
                .help("Single histogram boundary test.")
                .label("source", "single")
                .buckets(&[0.5])
                .build(),
        );
        build_ok(
            registry
                .histogram("limpid_test_ascending_buckets_seconds")
                .help("Ascending histogram boundaries test.")
                .label("source", "ascending")
                .buckets(&[0.1, 0.5, 1.0])
                .build(),
        );
    }

    #[test]
    fn every_builder_rejects_duplicate_label_names() {
        let registry = Registry::new();
        assert_duplicate_label_name(
            registry
                .counter("limpid_test_duplicate_counter_label_total")
                .help("Duplicate counter label test.")
                .label("source", "one")
                .label("source", "two")
                .build(),
            "limpid_test_duplicate_counter_label_total",
            &[("source", "one"), ("source", "two")],
            "source",
        );
        assert_duplicate_label_name(
            registry
                .gauge("limpid_test_duplicate_gauge_label")
                .help("Duplicate gauge label test.")
                .label("source", "one")
                .label("region", "west")
                .label("source", "two")
                .build(),
            "limpid_test_duplicate_gauge_label",
            &[("source", "one"), ("region", "west"), ("source", "two")],
            "source",
        );
        assert_duplicate_label_name(
            registry
                .histogram("limpid_test_duplicate_histogram_label_seconds")
                .help("Duplicate histogram label test.")
                .label("source", "one")
                .label("source", "two")
                .buckets(&[0.1, 1.0])
                .build(),
            "limpid_test_duplicate_histogram_label_seconds",
            &[("source", "one"), ("source", "two")],
            "source",
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

    #[test]
    fn counter_inc_by_adds_the_exact_delta_and_zero_is_a_noop() {
        let registry = Registry::new();
        let counter = build_ok(
            registry
                .counter("limpid_test_bytes_total")
                .help("Test byte counter.")
                .label("input", "fixture")
                .build(),
        );

        counter.inc_by(0);
        counter.inc_by(4_096);
        counter.inc();

        let snapshot = snapshot_json(&registry);
        assert_eq!(
            metric(&snapshot, "limpid_test_bytes_total")["series"][0]["value"],
            4_097
        );
    }

    #[test]
    fn documented_metric_bundles_share_one_registry_without_shadow_series() {
        let registry = Registry::new();
        let input: Arc<InputMetrics> = build_ok(InputMetrics::register(&registry, "ingress"));
        let pipeline: Arc<PipelineMetrics> =
            build_ok(PipelineMetrics::register(&registry, "route"));
        let output: Arc<OutputMetrics> = build_ok(OutputMetrics::register(&registry, "egress"));
        let input_shared = Arc::clone(&input);
        let pipeline_shared = Arc::clone(&pipeline);
        let output_shared = Arc::clone(&output);

        let initial = snapshot_json(&registry);
        for (name, label, label_value) in [
            ("limpid_pipeline_inflight", "pipeline", "route"),
            ("limpid_output_queue_depth", "output", "egress"),
            ("limpid_output_in_retry", "output", "egress"),
        ] {
            let family = metric(&initial, name);
            assert_eq!(family["type"], "gauge");
            let series = family["series"].as_array().expect("gauge series array");
            assert_eq!(series.len(), 1, "{name} must be prepopulated once");
            assert_eq!(series[0]["labels"].as_object().unwrap().len(), 1);
            assert_eq!(series[0]["labels"][label], label_value);
            assert_eq!(series[0]["value"], 0, "{name} must start at zero");
        }

        macro_rules! inc {
            ($counter:expr, $times:expr) => {
                for _ in 0..$times {
                    $counter.inc();
                }
            };
        }

        inc!(input_shared.events_received, 1);
        inc!(input.events_invalid, 2);
        inc!(input.events_injected, 3);
        input.bytes_received.inc_by(17);
        inc!(pipeline_shared.events_received, 4);
        inc!(pipeline.events_finished, 5);
        inc!(pipeline.events_dropped, 6);
        inc!(pipeline.events_discarded, 7);
        inc!(pipeline.events_errored, 8);
        inc!(pipeline.events_errored_unwritable, 9);
        pipeline.inflight.set(2);
        inc!(output_shared.events_received, 10);
        inc!(output.events_injected, 11);
        inc!(output.events_written, 12);
        inc!(output.events_failed, 13);
        inc!(output.retries, 14);
        inc!(output.events_wedged, 15);
        inc!(output.events_errored_unwritable, 16);
        output.bytes_written.inc_by(23);
        output.queue_depth.set(3);
        output.in_retry.set(1);

        let snapshot = snapshot_json(&registry);
        let metrics = snapshot["metrics"]
            .as_array()
            .expect("metrics must be an array");
        assert_eq!(
            metrics.len(),
            21,
            "only the documented metric set is registered"
        );

        let expected = [
            ("limpid_input_events_received_total", "input", "ingress", 1),
            ("limpid_input_events_invalid_total", "input", "ingress", 2),
            ("limpid_input_events_injected_total", "input", "ingress", 3),
            ("limpid_input_bytes_received_total", "input", "ingress", 17),
            (
                "limpid_pipeline_events_received_total",
                "pipeline",
                "route",
                4,
            ),
            (
                "limpid_pipeline_events_finished_total",
                "pipeline",
                "route",
                5,
            ),
            (
                "limpid_pipeline_events_discarded_total",
                "pipeline",
                "route",
                7,
            ),
            (
                "limpid_pipeline_events_errored_total",
                "pipeline",
                "route",
                8,
            ),
            (
                "limpid_pipeline_events_errored_unwritable_total",
                "pipeline",
                "route",
                9,
            ),
            (
                "limpid_output_events_received_total",
                "output",
                "egress",
                10,
            ),
            (
                "limpid_output_events_injected_total",
                "output",
                "egress",
                11,
            ),
            ("limpid_output_events_written_total", "output", "egress", 12),
            ("limpid_output_events_failed_total", "output", "egress", 13),
            ("limpid_output_retries_total", "output", "egress", 14),
            ("limpid_output_events_wedged_total", "output", "egress", 15),
            (
                "limpid_output_events_errored_unwritable_total",
                "output",
                "egress",
                16,
            ),
            ("limpid_output_bytes_written_total", "output", "egress", 23),
        ];
        for (name, label, label_value, value) in expected {
            let family = metric(&snapshot, name);
            assert_eq!(family["type"], "counter");
            assert!(
                family["help"].as_str().is_some_and(|help| !help.is_empty()),
                "{name} must remain self-describing"
            );
            let series = family["series"]
                .as_array()
                .expect("series must be an array");
            assert_eq!(series.len(), 1, "{name} must have exactly one series");
            let series = &series[0];
            let labels = series["labels"]
                .as_object()
                .expect("labels must be an object");
            assert_eq!(labels.len(), 1, "{name} must have exactly one label");
            assert_eq!(
                labels.get(label).and_then(Value::as_str),
                Some(label_value),
                "{name} must have the documented labelset"
            );
            assert_eq!(series["value"], value);
        }

        let dropped = metric(&snapshot, "limpid_events_dropped_total");
        assert_eq!(dropped["type"], "counter");
        assert_eq!(
            dropped["help"],
            "Total events whose drop propagated through this processing node."
        );
        assert_eq!(dropped["series"].as_array().unwrap().len(), 1);
        assert_eq!(
            dropped["series"][0]["labels"],
            serde_json::json!({
                "pipeline": "route",
                "step": "0",
                "process_path": "/",
                "process_name": "",
            })
        );
        assert_eq!(dropped["series"][0]["value"], 6);

        let expected_gauges = [
            ("limpid_pipeline_inflight", "pipeline", "route", 2),
            ("limpid_output_queue_depth", "output", "egress", 3),
            ("limpid_output_in_retry", "output", "egress", 1),
        ];
        for (name, label, label_value, value) in expected_gauges {
            let family = metric(&snapshot, name);
            assert_eq!(family["type"], "gauge");
            assert!(
                family["help"].as_str().is_some_and(|help| !help.is_empty()),
                "{name} must remain self-describing"
            );
            let series = family["series"]
                .as_array()
                .expect("series must be an array");
            assert_eq!(series.len(), 1, "{name} must have exactly one series");
            let labels = series[0]["labels"]
                .as_object()
                .expect("labels must be an object");
            assert_eq!(labels.len(), 1, "{name} must have exactly one label");
            assert_eq!(labels.get(label).and_then(Value::as_str), Some(label_value));
            assert_eq!(series[0]["value"], value);
        }

        let duplicate = match OutputMetrics::register(&registry, "egress") {
            Ok(_) => panic!("the bundle must register into the supplied shared registry"),
            Err(error) => error,
        };
        let diagnostic = duplicate.to_string();
        assert_output_duplicate(&duplicate, "egress", &diagnostic);
    }

    #[test]
    fn gauge_delta_updates_are_atomic_across_threads() {
        const THREADS: usize = 8;
        const UPDATES: usize = 1_000;

        let registry = Registry::new();
        let gauge = build_ok(
            registry
                .gauge("limpid_test_concurrent_inflight")
                .help("Concurrent gauge delta test.")
                .label("pipeline", "unit")
                .build(),
        );
        let start = Arc::new(std::sync::Barrier::new(THREADS + 1));
        let incremented = Arc::new(std::sync::Barrier::new(THREADS + 1));
        let decrement = Arc::new(std::sync::Barrier::new(THREADS + 1));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let gauge = Arc::clone(&gauge);
                let start = Arc::clone(&start);
                let incremented = Arc::clone(&incremented);
                let decrement = Arc::clone(&decrement);
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..UPDATES {
                        gauge.inc();
                    }
                    incremented.wait();
                    decrement.wait();
                    for _ in 0..UPDATES {
                        gauge.dec();
                    }
                });
            }
            start.wait();
            incremented.wait();
            let incremented_value = metric(
                &snapshot_json(&registry),
                "limpid_test_concurrent_inflight",
            )["series"][0]["value"]
                .clone();
            decrement.wait();
            assert_eq!(incremented_value, THREADS * UPDATES);
        });

        assert_eq!(
            metric(&snapshot_json(&registry), "limpid_test_concurrent_inflight")["series"][0]["value"],
            0
        );
    }

    #[test]
    fn gauge_decrement_underflow_never_wraps_the_exported_value() {
        let registry = Registry::new();
        let gauge = build_ok(
            registry
                .gauge("limpid_test_saturating_gauge")
                .help("Saturating gauge test.")
                .label("pipeline", "unit")
                .build(),
        );

        #[cfg(debug_assertions)]
        assert!(
            std::panic::catch_unwind(|| gauge.dec()).is_err(),
            "debug builds must surface a lifecycle imbalance"
        );
        #[cfg(not(debug_assertions))]
        gauge.dec();

        assert_eq!(
            metric(&snapshot_json(&registry), "limpid_test_saturating_gauge")["series"][0]["value"],
            0
        );
    }

    fn assert_output_duplicate(error: &MetricsError, label_value: &str, diagnostic: &str) {
        let (name, labelset) = match error {
            MetricsError::DuplicateSeries { name, labelset } => (name, labelset),
            other => panic!("expected DuplicateSeries, got {other:?}"),
        };
        assert!(
            [
                "limpid_output_events_received_total",
                "limpid_output_events_injected_total",
                "limpid_output_events_written_total",
                "limpid_output_events_failed_total",
                "limpid_output_retries_total",
                "limpid_output_events_wedged_total",
                "limpid_output_events_errored_unwritable_total",
                "limpid_output_bytes_written_total",
                "limpid_output_queue_depth",
                "limpid_output_in_retry",
            ]
            .contains(&name.as_str())
        );
        assert_eq!(labelset, &[("output".to_owned(), label_value.to_owned())]);
        assert!(diagnostic.contains(&format!("name={name:?}")));
        assert!(diagnostic.contains(&format!("labelset={labelset:?}")));
    }

    #[test]
    fn every_retry_loop_updates_the_pre_resolved_retry_gauge() {
        let carriers = [
            ("batched", include_str!("modules/output/batched.rs")),
            ("file", include_str!("modules/output/file.rs")),
            ("kafka", include_str!("modules/output/kafka.rs")),
            ("stdout", include_str!("modules/output/stdout.rs")),
            ("syslog_tcp", include_str!("modules/output/syslog_tcp.rs")),
            ("syslog_udp", include_str!("modules/output/syslog_udp.rs")),
            ("unix_socket", include_str!("modules/output/unix_socket.rs")),
        ];

        for (name, source) in carriers {
            assert!(
                source.contains("retries.inc()"),
                "{name} must retain its retry counter"
            );
            assert!(
                source.contains("in_retry"),
                "{name} must update the shared fixed-label retry gauge"
            );
        }
    }
}

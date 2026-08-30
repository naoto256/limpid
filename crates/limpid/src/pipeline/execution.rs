use super::*;

/// Trace entry for --test mode output.
#[derive(Debug)]
pub struct TraceEntry {
    pub stage: String,
    pub label: String,
    pub detail: String,
}

/// How a pipeline terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineTermination {
    /// Reached end of pipeline or explicit `finish`
    Finished,
    /// Explicit `drop` statement (event filtered out)
    Dropped,
    /// A `process` statement raised a runtime error (unknown identifier,
    /// type mismatch, regex compile failure, …). The original event is
    /// surfaced via [`PipelineRunResult::errored`] so the runtime layer
    /// can route it to the dead-letter queue (operator-configured
    /// `error_log` JSONL file, or `tracing::error!` fallback). The
    /// downstream output stream is unaffected — only events that
    /// finished cleanly reach the configured outputs.
    Errored,
}

/// Sum-type failure context surfaced when an event is routed to the
/// dead-letter queue.
///
/// Two flavors distinguish *where* the failure occurred and therefore
/// what `event` snapshot is meaningful for replay:
///
/// - [`Process`](ErroredEventContext::Process) — a `process` step (named
///   `def process` invocation, inline `process { ... }` block, or a
///   top-level `error` statement) failed during pipeline execution.
///   The captured [`ProcessEvent`] holds the original key / ingress / source /
///   received_at; egress is not snapshotted because at the failure
///   point it may hold partial output of an earlier process in the
///   chain, which would confuse replay. Replay re-runs the pipeline
///   from scratch via `limpidctl inject input <pipeline_input>`.
///
/// - [`Output`](ErroredEventContext::Output) — an output sink failed to
///   accept the event (queue enqueue failure on the runtime side, retry
///   exhaustion on the sink side, or batched shutdown drain). The
///   captured [`OutputEvent`] holds both ingress AND egress, because
///   the pipeline body already finished and the egress is the rendered
///   payload the sink was about to write. Replay routes through
///   `limpidctl inject output <output_name>` and the sink's
///   `consume()` re-runs internal routing.
///
/// The Output flavor records *only* the output name — never an
/// address, destination, path, key, topic, partition, endpoint,
/// URL, peer, or any other sink-specific routing metadata. Replay
/// sends the event back through
/// the named output's `consume()`, which re-routes internally.
#[derive(Debug, Clone)]
pub enum ErroredEventContext {
    Process {
        /// Wall-clock at which the error was raised.
        timestamp: chrono::DateTime<chrono::Utc>,
        /// Pipeline name (from `def pipeline <name>`).
        pipeline: String,
        /// Failure site: `<process_name>` for an explicit `def process`
        /// invocation, `(inline)` for an inline `process { ... }` block,
        /// `(pipeline)` for an explicit `error` statement at pipeline
        /// scope, or `(pipeline body)` for a runtime error raised by
        /// expression evaluation outside any process.
        site: String,
        /// Stringified `ProcessError` / `anyhow::Error` from the failure.
        reason: String,
        /// Pre-failure event snapshot (key / ingress / source / received_at only).
        event: ProcessEvent,
    },
    Output {
        /// Wall-clock at which the error was raised.
        timestamp: chrono::DateTime<chrono::Utc>,
        /// Pipeline name when known. Runtime-side enqueue failures carry
        /// the dispatching pipeline; sink-side retry / shutdown records
        /// leave this blank because they no longer have pipeline context.
        pipeline: String,
        /// Failure site: `<output_name>` for retry exhaustion,
        /// `<output_name> shutdown` for batched shutdown drain, or
        /// `<output_name> enqueue` for runtime enqueue failure.
        site: String,
        /// Stringified failure reason.
        reason: String,
        /// Output name — the *only* sink-routing metadata captured.
        /// Replay = `limpidctl inject output <output_name>`.
        output_name: String,
        /// Event snapshot (key + ingress + egress + source + received_at).
        event: OutputEvent,
    },
}

/// Process-flavor event snapshot for the DLQ.
///
/// Carries only the fields needed to re-run the pipeline from scratch:
/// the original ingress bytes, the source socket, and the input
/// timestamp. Egress is not captured because at a process failure
/// point it may hold partial output of an earlier process step.
#[derive(Debug, Clone)]
pub struct ProcessEvent {
    key: uuid::Uuid,
    pub source: std::net::SocketAddr,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub ingress: bytes::Bytes,
    ltp_stamps: std::sync::Arc<[crate::ltp::HopStamp]>,
}

/// Output-flavor event snapshot for the DLQ.
///
/// Carries both ingress and egress: the pipeline body already finished
/// and produced an egress payload — replay through `inject output`
/// hands the egress directly to the sink's `consume()` for
/// re-rendering / re-shipping.
#[derive(Debug, Clone)]
pub struct OutputEvent {
    key: uuid::Uuid,
    pub source: std::net::SocketAddr,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub ingress: bytes::Bytes,
    pub egress: bytes::Bytes,
    ltp_stamps: std::sync::Arc<[crate::ltp::HopStamp]>,
}

impl ProcessEvent {
    /// Snapshot the process-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        Self {
            key: ev.key(),
            source: ev.source,
            received_at: ev.received_at,
            ingress: ev.ingress.clone(),
            ltp_stamps: ev.ltp_stamps_arc(),
        }
    }

    /// Return the immutable identity of the captured event.
    pub fn key(&self) -> uuid::Uuid {
        self.key
    }

    pub(crate) fn ltp_stamps(&self) -> std::sync::Arc<[crate::ltp::HopStamp]> {
        std::sync::Arc::clone(&self.ltp_stamps)
    }
}

impl OutputEvent {
    /// Snapshot the output-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        Self {
            key: ev.key(),
            source: ev.source,
            received_at: ev.received_at,
            ingress: ev.ingress.clone(),
            egress: ev.egress.clone(),
            ltp_stamps: ev.ltp_stamps_arc(),
        }
    }

    /// Return the immutable identity of the captured event.
    pub fn key(&self) -> uuid::Uuid {
        self.key
    }

    pub(crate) fn ltp_stamps(&self) -> std::sync::Arc<[crate::ltp::HopStamp]> {
        std::sync::Arc::clone(&self.ltp_stamps)
    }
}

impl ErroredEventContext {
    /// Failure-site accessor.
    pub fn site(&self) -> &str {
        match self {
            Self::Process { site, .. } | Self::Output { site, .. } => site,
        }
    }

    /// Reason string accessor.
    pub fn reason(&self) -> &str {
        match self {
            Self::Process { reason, .. } | Self::Output { reason, .. } => reason,
        }
    }

    /// Wall-clock timestamp accessor.
    pub fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            Self::Process { timestamp, .. } | Self::Output { timestamp, .. } => *timestamp,
        }
    }

    /// Byte-length hint of the recoverable payload — `egress` on
    /// Output flavor (that is what a replay would ship), `ingress`
    /// on Process flavor (that is what a replay would re-enter). Used
    /// by the `Meta` tracing fallback so operators can correlate
    /// journald records with their pipeline / output metrics
    /// without the payload bytes themselves leaving the daemon.
    pub fn payload_size_hint(&self) -> usize {
        match self {
            Self::Process { event, .. } => event.ingress.len(),
            Self::Output { event, .. } => event.egress.len(),
        }
    }
}

/// Result of running an event through a pipeline.
///
/// Execution trace is no longer carried here — see `run_pipeline`'s
/// `trace: Option<&mut Vec<TraceEntry>>` parameter. `--test-pipeline`
/// passes its own `Vec` and reads it back directly after the call;
/// the daemon hot path passes `None` and never allocates one.
pub struct PipelineRunResult {
    pub outputs: Vec<(String, QueuedEvent)>,
    /// True iff at least one `output` statement was reached during
    /// execution (i.e. `outputs` was non-empty *before* the runtime
    /// drained it into the per-output queues). Needed because the
    /// runtime moves `outputs` out of this struct on the way to the
    /// queue senders, so a later `outputs.is_empty()` check would
    /// always see `true`. Used to distinguish
    /// `events_finished` (Finished AND emitted ≥1 output) from
    /// `events_discarded` (Finished AND emitted nothing).
    pub had_outputs: bool,
    pub termination: PipelineTermination,
    /// DLQ records accumulated during this run. Non-empty iff
    /// `termination == Errored` from a pipeline-side failure, or when
    /// the runtime layer appends per-failed-output enqueue records
    /// (one per failed output). The runtime drains each record into
    /// the configured `error_log`, or — when none is configured —
    /// delegates to the `error_log_fallback` ladder helper to emit a
    /// payload-free operator signal by default (or `Meta` / `Full`
    /// on explicit opt-in). The tracing line is best-effort, not
    /// load-bearing recovery.
    pub errored: Vec<ErroredEventContext>,
}

///
/// Only user-defined `def process { ... }` blocks resolve here.
/// Built-in processes were removed in v0.3.0 — former native
/// transforms are now DSL functions (`syslog.parse`, `parse_json`,
/// `regex_replace`, …) invoked via expression statements.
struct DslProcessRegistry<'a> {
    processes: &'a HashMap<String, ProcessDef>,
    funcs: &'a FunctionRegistry,
    tap: Option<&'a TapRegistry>,
    process_metrics: Option<&'a PipelineProcessMetrics>,
}

impl<'a> DslProcessRegistry<'a> {
    fn new(
        processes: &'a HashMap<String, ProcessDef>,
        funcs: &'a FunctionRegistry,
        tap: Option<&'a TapRegistry>,
        process_metrics: Option<&'a PipelineProcessMetrics>,
    ) -> Self {
        Self {
            processes,
            funcs,
            tap,
            process_metrics,
        }
    }

    fn call_node<'bump>(
        &self,
        metric_token: Option<usize>,
        name: &str,
        event: BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> std::result::Result<Option<BorrowedEvent<'bump>>, ProcessError> {
        let metric_node = match (self.process_metrics, metric_token) {
            (Some(metrics), Some(token)) => Some(metrics.select_node(token).ok_or_else(|| {
                ProcessError::Failed("compiled process metric token is out of range".to_owned())
            })?),
            (Some(_), None) => {
                return Err(ProcessError::Failed(
                    "compiled process metric token is missing".to_owned(),
                ));
            }
            (None, _) => None,
        };
        if let Some(node) = metric_node {
            node.counters.start();
        }

        let result = if let Some(process_def) = self.processes.get(name) {
            trace!("process '{}' (user-defined): executing", name);
            match metric_node {
                Some(node) => exec_process_body_with_metric_plan(
                    &process_def.body,
                    &node.body_plan,
                    event,
                    self,
                    self.funcs,
                    arena,
                ),
                None => exec_process_body(&process_def.body, event, self, self.funcs, arena),
            }
            .map_err(|error| ProcessError::Failed(error.to_string()))
        } else {
            tracing::warn!(
                "unknown process '{}', passing event through unchanged",
                name
            );
            Ok(ExecResult::Continue(event))
        };

        match result {
            Ok(ExecResult::Continue(event)) => {
                if let Some(node) = metric_node {
                    node.counters.continued();
                }
                trace!("process '{}': ok", name);
                self.emit_tap(name, &event);
                Ok(Some(event))
            }
            Ok(ExecResult::Dropped) => {
                if let Some(node) = metric_node {
                    node.counters.dropped();
                }
                trace!("process '{}': dropped", name);
                Ok(None)
            }
            Err(error) => {
                if let Some(node) = metric_node {
                    node.counters.errored();
                }
                Err(error)
            }
        }
    }
}

impl ProcessRegistry for DslProcessRegistry<'_> {
    fn call<'bump>(
        &self,
        name: &str,
        event: BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> std::result::Result<Option<BorrowedEvent<'bump>>, ProcessError> {
        self.call_node(None, name, event, arena)
    }
}

impl CompiledProcessRegistry for DslProcessRegistry<'_> {
    fn call_pre_resolved<'bump>(
        &self,
        name: &str,
        metric_token: usize,
        event: BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> std::result::Result<Option<BorrowedEvent<'bump>>, ProcessError> {
        self.call_node(Some(metric_token), name, event, arena)
    }
}

impl DslProcessRegistry<'_> {
    fn emit_tap<'bump>(&self, process_name: &str, event: &BorrowedEvent<'bump>) {
        if let Some(tap) = self.tap {
            let key = format!("process {}", process_name);
            // Avoid the per-event `to_owned()` workspace clone unless a
            // tap subscriber is actually attached. `is_subscribed`
            // collapses to a single relaxed atomic load on the hot path
            // (no lock when the registry isn't being mutated).
            if tap.is_subscribed(&key) {
                let owned = event.to_owned();
                tap.try_emit(&key, &owned);
            }
        }
    }
}

/// Run a single event through a pipeline definition.
///
/// After this change the executor never resolves an output sink — the `output`
/// statement just enqueues the owned event, and render happens
/// consumer-side inside each sink's `Output::consume`. The previous
/// `output_sinks: &HashMap<String, Arc<dyn Output>>` parameter is gone
/// as part of that cleanup.
///
/// `trace` collects a human-readable execution trace for
/// `--test-pipeline` (see `main.rs::run_test`). Pass `None` on the
/// daemon hot path (`runtime/pipeline_worker.rs::run_pipeline_with_outputs_inner`) — every
/// trace push site (and the `format!`/`to_string` calls that build
/// its fields) is gated on `trace.is_some()` so no throwaway
/// formatting work runs when nothing will read it. The collected
/// entries live in the caller's `Vec`; `PipelineRunResult` does not
/// carry them.
#[allow(clippy::too_many_arguments)]
pub fn run_pipeline(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
) -> Result<PipelineRunResult> {
    run_pipeline_at(
        pipeline,
        event,
        config,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        crate::time::UnixNanos::now(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_at(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    run_pipeline_inner(
        pipeline,
        event,
        config,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        None,
        dispatch_started_at,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_with_process_metrics_at(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    process_metrics: &PipelineProcessMetrics,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    run_pipeline_inner(
        pipeline,
        event,
        config,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        Some(process_metrics),
        dispatch_started_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_inner(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    process_metrics: Option<&PipelineProcessMetrics>,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    let registry = DslProcessRegistry::new(&config.processes, funcs, tap, process_metrics);
    let mut trace = trace;
    let mut outputs = Vec::new();

    // Log initial state — formatted from `event` while it's still in
    // owned form, before we view it into the arena. The `format!` /
    // `from_utf8_lossy` here only run when a caller actually wants a
    // trace (--test-pipeline); the daemon hot path passes `None` and
    // skips this entirely.
    if let Some(trace) = trace.as_mut() {
        trace.push(TraceEntry {
            stage: "input".into(),
            label: String::new(),
            detail: format!("ingress: {}", String::from_utf8_lossy(&event.ingress)),
        });
    }

    // Per-event arena. The entire `Value` tree built during execution
    // (HashLits, parser outputs, workspace mutations) lives in `bump`
    // and is reset to offset zero by the caller after this function
    // returns — see `runtime::run_pipeline_workers`. The `Bump` itself
    // is owned by the per-input pipeline-worker task and reused
    // across events, so the underlying chunk-group is malloc'd once
    // at task startup and never again on the hot path. This
    // eliminates the xzm-zone-lock contention that capped
    // multi-pipeline scaling at ~2.4× / 4 cores on v0.6.0 (where
    // every event called `Bump::new` and the system allocator
    // serialised concurrent malloc/free across pipelines).
    let arena = EventArena::new(bump);
    let bevent = event.view_in(&arena);

    let mut errored: Vec<ErroredEventContext> = Vec::new();
    let exec_ctx = PipelineExecCtx {
        pipeline_name: &pipeline.name,
        registry: &registry,
        funcs,
        arena: &arena,
        output_capture,
        dispatch_started_at,
    };
    let mut exec_out = PipelineExecOut {
        trace,
        outputs: &mut outputs,
        errored: &mut errored,
    };
    let (_, termination) = exec_pipeline_body(
        &pipeline.body,
        process_metrics.map(|metrics| metrics.statements.as_slice()),
        bevent,
        &exec_ctx,
        &mut exec_out,
    )?;

    let had_outputs = !outputs.is_empty();
    Ok(PipelineRunResult {
        outputs,
        had_outputs,
        termination,
        errored,
    })
}

/// Whether each per-output `OwnedEvent` snapshot pushed at
/// `output` statements carries the pipeline's per-event `workspace`
/// or drops it on the floor.
///
/// The snapshot is what the downstream queue transports. A memory
/// queue's consumer only reads `egress` (with `file` also reading
/// `source` / `received_at` and `kafka` optionally `source.ip`),
/// and every DLQ record projection stores only `OutputEvent`'s four
/// fields, so the workspace deep-clone was pure overhead on the
/// hot path — the largest contributor to the D-shape throughput
/// regression the `b7625bb` refactor introduced (see the release
/// notes for 0.7.10). Two consumers of the snapshot do still need
/// the workspace, and this policy names them explicitly rather
/// than inferring from ambient state:
///
/// - **Disk-backed queues** serialise the full `Event` JSON to the
///   WAL and rehydrate the workspace on replay. Skipping the
///   capture there would silently change on-disk semantics on the
///   next restart.
/// - **`--test-pipeline`** shows the resulting `OwnedEvent`
///   (workspace included) in its CLI output so operators can see
///   what a pipeline produced at each sink boundary.
///
/// The `tap output --json` path is *not* a workspace consumer under
/// 0.7.10's tap contract: the tap projection strips `workspace`
/// unconditionally on the emit side, so both memory and disk
/// queues expose the same tap-JSON shape regardless of what the
/// snapshot carries.
#[derive(Debug, Clone, Copy)]
pub enum OutputCapturePolicy<'a> {
    /// Strip the workspace from every output snapshot. Used by
    /// unit tests that don't care about workspace round-trip.
    #[allow(dead_code)]
    StripAll,
    /// Capture the workspace on every output snapshot. Used by
    /// `--test-pipeline` so the CLI display shows what the
    /// pipeline actually built.
    CaptureAll,
    /// Capture the workspace only on the named outputs (those whose
    /// queue is disk-backed). The daemon hot path builds this set
    /// once at startup from the compiled config's queue kinds and
    /// hands it in per event.
    DiskOnly(&'a std::collections::HashSet<String>),
}

impl<'a> OutputCapturePolicy<'a> {
    fn should_capture_workspace(&self, output_name: &str) -> bool {
        match self {
            Self::StripAll => false,
            Self::CaptureAll => true,
            Self::DiskOnly(disk) => disk.contains(output_name),
        }
    }
}

/// Immutable shared context threaded through the pipeline executor.
///
/// `pipeline_name` is here purely so a process-runtime error can
/// populate the [`ErroredEventContext`] surfaced in [`PipelineExecOut::errored`].
///
/// `arena` is the per-event bump arena — the same one
/// `run_pipeline` opened on the stack. The reference itself is held at
/// `'bump` so closures and primitive impls allocating into it can
/// produce values that live for the rest of the pipeline body.
///
/// `output_capture` decides per output whether the snapshot pushed
/// to `PipelineExecOut::outputs` carries the workspace — see
/// [`OutputCapturePolicy`].
struct PipelineExecCtx<'a, 'bump: 'a> {
    pipeline_name: &'a str,
    registry: &'a DslProcessRegistry<'a>,
    funcs: &'a FunctionRegistry,
    arena: &'bump EventArena<'bump>,
    output_capture: OutputCapturePolicy<'a>,
    dispatch_started_at: crate::time::UnixNanos,
}

/// Mutable accumulators threaded through the pipeline executor:
/// trace entries, output queue pushes, and the optional errored event
/// context. Bundled together to keep the recursive helpers under
/// clippy's `too_many_arguments` threshold and to make the executor's
/// "what comes out" surface explicit.
///
/// Outputs and errored contexts are heap-owned — they cross the
/// per-event arena boundary on the way out of `run_pipeline`.
struct PipelineExecOut<'a> {
    /// `Some` only on the `--test-pipeline` path (see `run_pipeline`'s
    /// `trace` parameter). `None` on the daemon hot path, where no
    /// caller ever reads `PipelineRunResult::trace` — keeping this an
    /// `Option` (rather than always allocating a `Vec`) lets every
    /// push site below skip its `format!` / `to_string` formatting
    /// work entirely when tracing isn't requested, instead of
    /// computing throwaway `String`s on every event just to push them
    /// into a `Vec` nobody drains.
    trace: Option<&'a mut Vec<TraceEntry>>,
    outputs: &'a mut Vec<(String, QueuedEvent)>,
    errored: &'a mut Vec<ErroredEventContext>,
}

impl PipelineExecOut<'_> {
    /// Push a trace entry, built lazily from `f`, iff tracing is
    /// enabled. `f` is only invoked when `trace` is `Some`, so the
    /// `format!`/`to_string` work that builds `TraceEntry` fields
    /// never runs on the daemon hot path.
    fn push_trace(&mut self, f: impl FnOnce() -> TraceEntry) {
        if let Some(trace) = self.trace.as_deref_mut() {
            trace.push(f());
        }
    }
}

/// Execute a pipeline body (sequence of pipeline statements).
/// Returns (remaining event if any, how the pipeline terminated).
fn exec_pipeline_body<'bump>(
    stmts: &[PipelineStatement],
    metric_stmts: Option<&[PipelineMetricStatement]>,
    mut event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    if ctx.registry.process_metrics.is_some() {
        let metric_stmts = metric_stmts
            .ok_or_else(|| anyhow::anyhow!("compiled pipeline metric plan is missing"))?;
        if metric_stmts.len() != stmts.len() {
            bail!("compiled pipeline metric plan length does not match the pipeline body");
        }
    }
    for (index, stmt) in stmts.iter().enumerate() {
        let metric_stmt = match ctx.registry.process_metrics {
            Some(_) => Some(
                metric_stmts
                    .and_then(|metrics| metrics.get(index))
                    .ok_or_else(|| anyhow::anyhow!("compiled pipeline metric entry is missing"))?,
            ),
            None => None,
        };
        match exec_pipeline_stmt(stmt, metric_stmt, event, ctx, out)? {
            (Some(e), _) => event = e,
            (None, term) => return Ok((None, term)),
        }
    }
    Ok((Some(event), PipelineTermination::Finished))
}

fn exec_pipeline_stmt<'bump>(
    stmt: &PipelineStatement,
    metric_stmt: Option<&PipelineMetricStatement>,
    event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    let cont = |event| Ok((Some(event), PipelineTermination::Finished));
    let dropped = || Ok((None, PipelineTermination::Dropped));
    let finished = || Ok((None, PipelineTermination::Finished));

    match stmt {
        PipelineStatement::Input(_) => cont(event),

        PipelineStatement::Error(msg_expr) => {
            // Render the optional message and route the event to the
            // error_log via PipelineTermination::Errored, mirroring how
            // a process-level Err lands in the DLQ.
            let msg = match msg_expr {
                Some(e) => value_to_string(&eval_expr(e, &event, ctx.funcs, ctx.arena)?),
                None => "explicit error routing".to_string(),
            };
            tracing::warn!(
                "pipeline '{}': error '{}' — event routed to error_log",
                ctx.pipeline_name,
                msg
            );
            out.push_trace(|| TraceEntry {
                stage: "error".into(),
                label: msg.clone(),
                detail: "event → error_log".into(),
            });
            // Cross to owned form for the DLQ context (which must
            // outlive the per-event arena).
            let owned = event.to_owned();
            out.errored.push(ErroredEventContext::Process {
                timestamp: chrono::Utc::now(),
                pipeline: ctx.pipeline_name.to_string(),
                site: "(pipeline)".to_string(),
                reason: msg,
                event: ProcessEvent::from_owned(&owned),
            });
            Ok((None, PipelineTermination::Errored))
        }

        PipelineStatement::ProcessChain(chain) => {
            let mut current = event;
            let metric_nodes = match metric_stmt {
                Some(PipelineMetricStatement::ProcessChain(nodes)) => Some(nodes.as_slice()),
                _ => None,
            };
            for (index, element) in chain.iter().enumerate() {
                let metric_token = metric_nodes.and_then(|nodes| nodes.get(index)).copied();
                match element {
                    ProcessChainElement::Named(name) => {
                        // Snapshot the pre-call view before the registry
                        // consumes the borrowed event — the Err arm
                        // needs a stable, DLQ-ready event. Use an
                        // arena-local shallow snapshot rather than
                        // `to_owned`: the success path (dominant on the
                        // hot path) only needs the snapshot to survive
                        // until the `match` returns `Ok(...)`, and
                        // paying the heap materialization of the
                        // workspace `HashMap` + `Value` tree on every
                        // successful process call was a significant
                        // fraction of the runtime for multi-process
                        // pipelines where `parse | enrich | route`-shaped
                        // chains re-entered process #2+ with a populated
                        // workspace. `snapshot_in` bumps `Bytes`
                        // refcounts and copies the workspace index vec;
                        // the deep `to_owned` clone happens only in the
                        // Err arm below, where the DLQ record's owned
                        // event has to cross the arena boundary anyway.
                        let backup_view = current.snapshot_in(ctx.arena);
                        let call_result = match ctx.registry.process_metrics {
                            Some(_) => {
                                let token = metric_token.ok_or_else(|| {
                                    anyhow::anyhow!("compiled root process token is missing")
                                })?;
                                ctx.registry
                                    .call_pre_resolved(name, token, current, ctx.arena)
                            }
                            None => ctx.registry.call(name, current, ctx.arena),
                        };
                        match call_result {
                            Ok(Some(e)) => {
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(None) => {
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "process '{}': {} — event routed to error_log",
                                    name,
                                    e
                                );
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: format!("error: {} (event → error_log)", e),
                                });
                                out.errored.push(ErroredEventContext::Process {
                                    timestamp: chrono::Utc::now(),
                                    pipeline: ctx.pipeline_name.to_string(),
                                    site: name.clone(),
                                    reason: e.to_string(),
                                    // Cross the arena boundary here on
                                    // the (rare) failure path only: the
                                    // owned DLQ record outlives the
                                    // per-event arena.
                                    event: ProcessEvent::from_owned(&backup_view.to_owned()),
                                });
                                return Ok((None, PipelineTermination::Errored));
                            }
                        }
                    }
                    ProcessChainElement::Inline(body) => {
                        // Same rationale as the Named arm: arena-local
                        // shallow snapshot for the DLQ Err path; the
                        // heap materialization happens only on failure.
                        let backup_view = current.snapshot_in(ctx.arena);
                        let metric_node = match ctx.registry.process_metrics {
                            Some(metrics) => {
                                let token = metric_token.ok_or_else(|| {
                                    anyhow::anyhow!("compiled inline process token is missing")
                                })?;
                                Some(metrics.select_node(token).ok_or_else(|| {
                                    anyhow::anyhow!("compiled inline process token is out of range")
                                })?)
                            }
                            None => None,
                        };
                        if let Some(node) = metric_node {
                            node.counters.start();
                        }
                        let result = match metric_node {
                            Some(node) => exec_process_body_with_metric_plan(
                                body,
                                &node.body_plan,
                                current,
                                ctx.registry,
                                ctx.funcs,
                                ctx.arena,
                            ),
                            None => {
                                exec_process_body(body, current, ctx.registry, ctx.funcs, ctx.arena)
                            }
                        };
                        match result {
                            Ok(ExecResult::Continue(e)) => {
                                if let Some(node) = metric_node {
                                    node.counters.continued();
                                }
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(ExecResult::Dropped) => {
                                if let Some(node) = metric_node {
                                    node.counters.dropped();
                                }
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                            Err(e) => {
                                if let Some(node) = metric_node {
                                    node.counters.errored();
                                }
                                tracing::warn!("inline process: {} — event routed to error_log", e);
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: format!("error: {} (event → error_log)", e),
                                });
                                out.errored.push(ErroredEventContext::Process {
                                    timestamp: chrono::Utc::now(),
                                    pipeline: ctx.pipeline_name.to_string(),
                                    site: "(inline)".to_string(),
                                    reason: e.to_string(),
                                    event: ProcessEvent::from_owned(&backup_view.to_owned()),
                                });
                                return Ok((None, PipelineTermination::Errored));
                            }
                        }
                    }
                }
            }
            cont(current)
        }

        PipelineStatement::Output(name) => {
            trace!(target: "limpid::pipeline", "output → {}", name);
            out.push_trace(|| TraceEntry {
                stage: "output".into(),
                label: format!("→ {}", name),
                detail: String::new(),
            });
            // The queue transports a plain `OwnedEvent`; both memory
            // and disk queues carry `Event` end-to-end and render
            // runs consumer-side inside each sink's `Output::consume`.
            // What the snapshot pushed here contains is decided by
            // `OutputCapturePolicy`:
            //
            // - Memory queue + non-test-pipeline: the snapshot drops
            //   the `workspace` (via `to_owned_without_workspace`).
            //   The downstream sink reads `egress` (and, for `file`
            //   and `kafka`, `source` / `received_at` / `source.ip`),
            //   the DLQ path projects to `OutputEvent`'s five fields,
            //   and the output-flavor `tap` strips `workspace` on the
            //   emit side too — so nobody observes the missing
            //   workspace.
            // - Disk queue (or `--test-pipeline`): the snapshot keeps
            //   the `workspace` (via `to_owned`). Disk queues need it
            //   because the WAL persists the full `Event` JSON and
            //   replay rehydrates it; `--test-pipeline` needs it
            //   because its CLI display shows the snapshot verbatim.
            //
            // The live `event` is unchanged either way — any
            // subsequent `if workspace.x == ...` gate at pipeline
            // scope still sees the populated workspace on the
            // borrowed view.
            let snapshot = if ctx.output_capture.should_capture_workspace(name) {
                event.to_owned()
            } else {
                event.to_owned_without_workspace()
            };
            let emitted_at = crate::time::UnixNanos::now();
            if let Some(PipelineMetricStatement::Output(timer)) = metric_stmt {
                timer.observe_between(ctx.dispatch_started_at, emitted_at);
            } else if ctx.registry.process_metrics.is_some() {
                bail!("compiled pipeline output metric entry is missing");
            }
            out.outputs
                .push((name.clone(), QueuedEvent::new(snapshot, emitted_at)));
            cont(event)
        }

        PipelineStatement::Drop => {
            trace!(target: "limpid::pipeline", "drop");
            out.push_trace(|| TraceEntry {
                stage: "drop".into(),
                label: String::new(),
                detail: String::new(),
            });
            dropped()
        }

        PipelineStatement::Finish => {
            trace!(target: "limpid::pipeline", "finish");
            out.push_trace(|| TraceEntry {
                stage: "finish".into(),
                label: String::new(),
                detail: String::new(),
            });
            finished()
        }

        PipelineStatement::If(if_chain) => {
            match select_if_branch_with_ordinal(if_chain, |c| {
                eval_expr(c, &event, ctx.funcs, ctx.arena)
            })? {
                Some(selection) => {
                    let metric_body = match metric_stmt {
                        Some(PipelineMetricStatement::If {
                            branches,
                            else_body,
                        }) => match selection.ordinal {
                            Some(ordinal) => branches.get(ordinal).map(Vec::as_slice),
                            None => else_body.as_deref(),
                        },
                        _ => None,
                    };
                    exec_pipeline_branch_body(selection.body, metric_body, event, ctx, out)
                }
                None => cont(event),
            }
        }

        PipelineStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr(discriminant, &event, ctx.funcs, ctx.arena)?;
            match select_switch_arm_with_ordinal(&disc_val, arms, |e| {
                eval_expr(e, &event, ctx.funcs, ctx.arena)
            })? {
                Some((ordinal, body)) => {
                    let metric_body = match metric_stmt {
                        Some(PipelineMetricStatement::Switch(metrics)) => {
                            metrics.get(ordinal).map(Vec::as_slice)
                        }
                        _ => None,
                    };
                    exec_pipeline_branch_body(body, metric_body, event, ctx, out)
                }
                None => cont(event),
            }
        }
    }
}

fn exec_pipeline_branch_body<'bump>(
    body: &[BranchBody],
    metric_body: Option<&[PipelineMetricStatement]>,
    mut event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    if ctx.registry.process_metrics.is_some() {
        let metric_body = metric_body
            .ok_or_else(|| anyhow::anyhow!("compiled pipeline branch plan is missing"))?;
        if metric_body.len() != body.len() {
            bail!("compiled pipeline branch plan length does not match the selected body");
        }
    }
    for (index, item) in body.iter().enumerate() {
        let metric_stmt = match ctx.registry.process_metrics {
            Some(_) => Some(
                metric_body
                    .and_then(|metrics| metrics.get(index))
                    .ok_or_else(|| anyhow::anyhow!("compiled pipeline branch entry is missing"))?,
            ),
            None => None,
        };
        match item {
            BranchBody::Pipeline(stmt) => {
                match exec_pipeline_stmt(stmt, metric_stmt, event, ctx, out)? {
                    (Some(e), _) => event = e,
                    (None, term) => return Ok((None, term)),
                }
            }
            BranchBody::Process(_) => {
                bail!("process statement found in pipeline context")
            }
        }
    }
    Ok((Some(event), PipelineTermination::Finished))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;
    use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
    use bytes::Bytes;

    #[test]
    fn finish_terminates_without_emitting_an_output() {
        let config =
            CompiledConfig::from_config(parse_config("def pipeline p { finish }").unwrap())
                .unwrap();
        let mut functions = FunctionRegistry::new();
        register_builtins(
            &mut functions,
            TableStore::from_configs(Vec::new()).unwrap(),
        );
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let result = run_pipeline(
            config.pipelines.get("p").unwrap(),
            &event,
            &config,
            &functions,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();

        assert_eq!(result.termination, PipelineTermination::Finished);
        assert!(result.outputs.is_empty());
        assert!(!result.had_outputs);
    }
}

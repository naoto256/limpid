//! Pipeline engine: compiles DSL definitions into an executable pipeline
//! and runs events through process chains.
//!
//! The boundary between **owned** and **borrowed (arena)** event forms
//! is drawn at [`run_pipeline`]: the function takes an [`OwnedEvent`]
//! (which is what the input layer / channel hands over) along with a
//! caller-owned `&mut bumpalo::Bump`, and views the event into that
//! arena. The runtime owns the bump so it can amortise allocation
//! across many events instead of paying a fresh allocator per event.
//! Everything inside the pipeline executor — eval, exec, function
//! dispatch — operates on [`BorrowedEvent<'bump>`]. At each output sink
//! and at each error path we cross back to the heap by calling
//! [`BorrowedEvent::to_owned`], so the post-pipeline code (channel
//! sends, DLQ persistence) keeps the same `OwnedEvent` shape it had
//! before v0.6.0.

use std::collections::HashMap;

use anyhow::{Result, bail};
use tracing::trace;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::*;
use crate::dsl::eval::{eval_expr, select_if_branch, select_switch_arm, value_to_string};
use crate::dsl::exec::{ExecResult, ProcessError, ProcessRegistry, exec_process_body};
use crate::event::{BorrowedEvent, OwnedEvent};
use crate::functions::FunctionRegistry;
use crate::tap::TapRegistry;

// ---------------------------------------------------------------------------
// Compiled configuration
// ---------------------------------------------------------------------------

/// A fully resolved configuration ready for execution.
#[derive(Clone)]
pub struct CompiledConfig {
    pub inputs: HashMap<String, InputDef>,
    pub outputs: HashMap<String, OutputDef>,
    pub processes: HashMap<String, ProcessDef>,
    pub pipelines: HashMap<String, PipelineDef>,
    /// User-defined `def function` declarations, indexed by name.
    /// Registered into the [`FunctionRegistry`] at runtime startup so
    /// call sites dispatch through the same `(namespace, name)` path
    /// as built-in primitives.
    pub functions: HashMap<String, FunctionDef>,
    pub global_blocks: HashMap<String, Vec<Property>>,
}

impl CompiledConfig {
    pub fn from_config(config: Config) -> Result<Self> {
        let mut inputs = HashMap::new();
        let mut outputs = HashMap::new();
        let mut processes = HashMap::new();
        let mut pipelines = HashMap::new();
        let mut functions: HashMap<String, FunctionDef> = HashMap::new();
        let mut global_blocks = HashMap::new();

        for def in config.definitions {
            match def {
                Definition::Input(d) => {
                    if inputs.contains_key(&d.name) {
                        bail!("duplicate input definition: {}", d.name);
                    }
                    inputs.insert(d.name.clone(), d);
                }
                Definition::Output(d) => {
                    if outputs.contains_key(&d.name) {
                        bail!("duplicate output definition: {}", d.name);
                    }
                    outputs.insert(d.name.clone(), d);
                }
                Definition::Process(d) => {
                    if processes.contains_key(&d.name) {
                        bail!("duplicate process definition: {}", d.name);
                    }
                    processes.insert(d.name.clone(), d);
                }
                Definition::Pipeline(d) => {
                    if pipelines.contains_key(&d.name) {
                        bail!("duplicate pipeline definition: {}", d.name);
                    }
                    pipelines.insert(d.name.clone(), d);
                }
                Definition::Function(d) => {
                    if functions.contains_key(&d.name) {
                        bail!("duplicate function definition: {}", d.name);
                    }
                    functions.insert(d.name.clone(), d);
                }
            }
        }

        for block in config.global_blocks {
            global_blocks.insert(block.name, block.properties);
        }

        let compiled = Self {
            inputs,
            outputs,
            processes,
            pipelines,
            functions,
            global_blocks,
        };
        Ok(compiled)
    }

    /// Validate cross-references: all referenced inputs, outputs, and processes exist.
    ///
    /// Takes no `ModuleRegistry` — process names are resolved
    /// exclusively against user-defined DSL processes (v0.3.0 removed
    /// the native process layer), and inputs/outputs are validated by
    /// construction (`ModuleRegistry::create_input`/`create_output`
    /// already reject unknown types at build time), so there is
    /// nothing left here that needs the registry.
    pub fn validate(&self) -> Result<()> {
        for (name, pipeline) in &self.pipelines {
            for stmt in &pipeline.body {
                self.validate_pipeline_stmt(name, stmt)?;
            }
        }
        Ok(())
    }

    fn validate_pipeline_stmt(&self, pipeline_name: &str, stmt: &PipelineStatement) -> Result<()> {
        match stmt {
            PipelineStatement::Input(input_names) => {
                if input_names.is_empty() {
                    bail!(
                        "pipeline '{}': input statement has no input names",
                        pipeline_name
                    );
                }
                let mut seen = std::collections::HashSet::new();
                for input_name in input_names {
                    if !self.inputs.contains_key(input_name) {
                        bail!(
                            "pipeline '{}': references unknown input '{}'",
                            pipeline_name,
                            input_name
                        );
                    }
                    if !seen.insert(input_name.as_str()) {
                        bail!(
                            "pipeline '{}': input '{}' listed more than once",
                            pipeline_name,
                            input_name
                        );
                    }
                }
            }
            PipelineStatement::Output(output_name) => {
                if !self.outputs.contains_key(output_name) {
                    bail!(
                        "pipeline '{}': references unknown output '{}'",
                        pipeline_name,
                        output_name
                    );
                }
            }
            PipelineStatement::ProcessChain(chain) => {
                for element in chain {
                    if let ProcessChainElement::Named(proc_name) = element
                        && !self.processes.contains_key(proc_name)
                    {
                        bail!(
                            "pipeline '{}': references unknown process '{}'. \
                             Built-in processes were removed in v0.3.0 — use a DSL \
                             function (e.g. `syslog.parse(ingress)` as a statement) \
                             or define your own with `def process {{ ... }}`.",
                            pipeline_name,
                            proc_name
                        );
                    }
                }
            }
            PipelineStatement::If(if_chain) => {
                for (_, body) in &if_chain.branches {
                    for item in body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s)?;
                        }
                    }
                }
                if let Some(else_body) = &if_chain.else_body {
                    for item in else_body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s)?;
                        }
                    }
                }
            }
            PipelineStatement::Switch(_, arms) => {
                for arm in arms {
                    for item in &arm.body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s)?;
                        }
                    }
                }
            }
            PipelineStatement::Drop | PipelineStatement::Finish | PipelineStatement::Error(_) => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pipeline runner (for --test mode and runtime)
// ---------------------------------------------------------------------------

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
///   The captured [`ProcessEvent`] holds the original ingress / source /
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
        /// Pre-failure event snapshot (ingress / source / received_at only).
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
        /// Event snapshot (ingress + egress + source + received_at).
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
    pub source: std::net::SocketAddr,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub ingress: bytes::Bytes,
}

/// Output-flavor event snapshot for the DLQ.
///
/// Carries both ingress and egress: the pipeline body already finished
/// and produced an egress payload — replay through `inject output`
/// hands the egress directly to the sink's `consume()` for
/// re-rendering / re-shipping.
#[derive(Debug, Clone)]
pub struct OutputEvent {
    pub source: std::net::SocketAddr,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub ingress: bytes::Bytes,
    pub egress: bytes::Bytes,
}

impl ProcessEvent {
    /// Snapshot the process-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        Self {
            source: ev.source,
            received_at: ev.received_at,
            ingress: ev.ingress.clone(),
        }
    }
}

impl OutputEvent {
    /// Snapshot the output-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        Self {
            source: ev.source,
            received_at: ev.received_at,
            ingress: ev.ingress.clone(),
            egress: ev.egress.clone(),
        }
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
    pub outputs: Vec<(String, OwnedEvent)>,
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

/// A process registry backed by compiled DSL process definitions.
///
/// Only user-defined `def process { ... }` blocks resolve here.
/// Built-in processes were removed in v0.3.0 — former native
/// transforms are now DSL functions (`syslog.parse`, `parse_json`,
/// `regex_replace`, …) invoked via expression statements.
struct DslProcessRegistry<'a> {
    processes: &'a HashMap<String, ProcessDef>,
    funcs: &'a FunctionRegistry,
    tap: Option<&'a TapRegistry>,
}

impl<'a> DslProcessRegistry<'a> {
    fn new(
        processes: &'a HashMap<String, ProcessDef>,
        funcs: &'a FunctionRegistry,
        tap: Option<&'a TapRegistry>,
    ) -> Self {
        Self {
            processes,
            funcs,
            tap,
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
        if let Some(process_def) = self.processes.get(name) {
            trace!("process '{}' (user-defined): executing", name);
            return match exec_process_body(&process_def.body, event, self, self.funcs, arena) {
                Ok(ExecResult::Continue(e)) => {
                    trace!("process '{}': ok", name);
                    self.emit_tap(name, &e);
                    Ok(Some(e))
                }
                Ok(ExecResult::Dropped) => {
                    trace!("process '{}': dropped", name);
                    Ok(None)
                }
                Err(e) => Err(ProcessError::Failed(e.to_string())),
            };
        }

        // Unknown process — warn and pass through. Config validation in
        // `CompiledConfig::validate` catches this up front; this branch
        // is a safety net for paths that skip validation.
        tracing::warn!(
            "unknown process '{}', passing event through unchanged",
            name
        );
        Ok(Some(event))
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
/// daemon hot path (`runtime.rs::run_pipeline_with_outputs`) — every
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
    let registry = DslProcessRegistry::new(&config.processes, funcs, tap);
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
    };
    let mut exec_out = PipelineExecOut {
        trace,
        outputs: &mut outputs,
        errored: &mut errored,
    };
    let (_, termination) = exec_pipeline_body(&pipeline.body, bevent, &exec_ctx, &mut exec_out)?;

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
    outputs: &'a mut Vec<(String, OwnedEvent)>,
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
    mut event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    for stmt in stmts {
        match exec_pipeline_stmt(stmt, event, ctx, out)? {
            (Some(e), _) => event = e,
            (None, term) => return Ok((None, term)),
        }
    }
    Ok((Some(event), PipelineTermination::Finished))
}

fn exec_pipeline_stmt<'bump>(
    stmt: &PipelineStatement,
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
            for element in chain {
                match element {
                    ProcessChainElement::Named(name) => {
                        // Snapshot the heap-owned form before the
                        // registry consumes the borrowed event — the
                        // Err arm needs a stable, arena-independent
                        // event for the DLQ context.
                        let backup_owned = current.to_owned();
                        match ctx.registry.call(name, current, ctx.arena) {
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
                                    event: ProcessEvent::from_owned(&backup_owned),
                                });
                                return Ok((None, PipelineTermination::Errored));
                            }
                        }
                    }
                    ProcessChainElement::Inline(body) => {
                        let backup_owned = current.to_owned();
                        match exec_process_body(body, current, ctx.registry, ctx.funcs, ctx.arena) {
                            Ok(ExecResult::Continue(e)) => {
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(ExecResult::Dropped) => {
                                out.push_trace(|| TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                            Err(e) => {
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
                                    event: ProcessEvent::from_owned(&backup_owned),
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
            //   the DLQ path projects to `OutputEvent`'s four fields,
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
            out.outputs.push((name.clone(), snapshot));
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
            match select_if_branch(if_chain, |c| eval_expr(c, &event, ctx.funcs, ctx.arena))? {
                Some(body) => exec_pipeline_branch_body(body, event, ctx, out),
                None => cont(event),
            }
        }

        PipelineStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr(discriminant, &event, ctx.funcs, ctx.arena)?;
            match select_switch_arm(&disc_val, arms, |e| {
                eval_expr(e, &event, ctx.funcs, ctx.arena)
            })? {
                Some(body) => exec_pipeline_branch_body(body, event, ctx, out),
                None => cont(event),
            }
        }
    }
}

fn exec_pipeline_branch_body<'bump>(
    body: &[BranchBody],
    mut event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    for item in body {
        match item {
            BranchBody::Pipeline(stmt) => match exec_pipeline_stmt(stmt, event, ctx, out)? {
                (Some(e), _) => event = e,
                (None, term) => return Ok((None, term)),
            },
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

    fn compile(src: &str) -> Result<CompiledConfig> {
        CompiledConfig::from_config(parse_config(src)?)
    }

    #[test]
    fn validate_rejects_unknown_input_in_fan_in() {
        let src = r#"
def input a { type syslog_udp bind "0.0.0.0:5140" }
def output o { type file path "/tmp/x.log" }
def pipeline p {
    input a, missing
    output o
    drop
}
"#;
        let cfg = compile(src).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("unknown input 'missing'"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn validate_rejects_duplicate_input_in_fan_in() {
        let src = r#"
def input a { type syslog_udp bind "0.0.0.0:5140" }
def output o { type file path "/tmp/x.log" }
def pipeline p {
    input a, a
    output o
    drop
}
"#;
        let cfg = compile(src).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("listed more than once"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn process_runtime_error_populates_errored_context() {
        // bare `timestamp` is not a reserved ident in 0.5+; the runtime
        // raises `unknown identifier: timestamp` which must surface as
        // an ErroredEventContext on the run result, with the original
        // ingress preserved for replay via `inject --json`.
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def process wrap {
    egress = strftime(timestamp, "%Y", "UTC")
}
def pipeline p {
    input i
    process wrap
    output o
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"original payload"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        assert_eq!(result.errored.len(), 1);
        let ctx = &result.errored[0];
        match ctx {
            ErroredEventContext::Process {
                pipeline,
                site,
                reason,
                event,
                ..
            } => {
                assert_eq!(pipeline, "p");
                assert_eq!(site, "wrap");
                assert!(
                    reason.contains("unknown identifier"),
                    "unexpected reason: {}",
                    reason
                );
                assert_eq!(&event.ingress[..], b"original payload");
            }
            other => panic!("expected Process variant, got {:?}", other),
        }
        assert!(result.outputs.is_empty());
        let line = ctx.to_jsonl();
        assert!(line.contains("\"schema_version\":2"));
        assert!(line.contains("\"kind\":\"process\""));
        assert!(line.contains("\"pipeline\":\"p\""));
        assert!(line.contains("\"name\":\"wrap\""));
        assert!(line.contains("original payload"));
        // ProcessEvent has no egress in the serialised event block.
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["event"]["egress"].is_null());
        assert!(v["output"].is_null());
    }

    #[test]
    fn explicit_error_keyword_in_process_routes_to_dlq() {
        // `error "msg"` inside a def process body must surface the
        // same way a runtime process error does — PipelineTermination::Errored,
        // ErroredEventContext populated with the rendered message,
        // and outputs empty.
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def process refuse {
    error "I refuse"
}
def pipeline p {
    input i
    process refuse
    output o
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        assert_eq!(result.errored.len(), 1);
        match &result.errored[0] {
            ErroredEventContext::Process {
                pipeline,
                site,
                reason,
                ..
            } => {
                assert_eq!(pipeline, "p");
                assert_eq!(site, "refuse");
                assert!(reason.contains("I refuse"), "unexpected reason: {}", reason);
            }
            other => panic!("expected Process variant, got {:?}", other),
        }
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn explicit_error_keyword_at_pipeline_level_routes_to_dlq() {
        // `error "msg"` directly in the pipeline body must populate
        // ErroredEventContext with `process = "(pipeline)"` so DLQ
        // entries from pipeline-level routing are distinguishable
        // from process-body failures.
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p {
    input i
    error "blocked at pipeline gate"
    output o
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        assert_eq!(result.errored.len(), 1);
        match &result.errored[0] {
            ErroredEventContext::Process {
                pipeline,
                site,
                reason,
                ..
            } => {
                assert_eq!(pipeline, "p");
                assert_eq!(site, "(pipeline)");
                assert!(
                    reason.contains("blocked at pipeline gate"),
                    "unexpected reason: {}",
                    reason
                );
            }
            other => panic!("expected Process variant, got {:?}", other),
        }
        assert!(result.outputs.is_empty());
    }

    // This restructure deleted `render_failure_falls_back_to_owned_sink_input`.
    // The pipeline-side render-Err → Owned fallback no longer exists;
    // render now runs consumer-side inside each sink's `Output::consume`,
    // and a render failure tagged with `RenderError` routes straight to
    // the DLQ from the consumer loop without retrying.

    #[test]
    fn validate_accepts_fan_in_when_all_inputs_exist() {
        let src = r#"
def input a { type syslog_udp bind "0.0.0.0:5140" }
def input b { type syslog_udp bind "0.0.0.0:5141" }
def output o { type file path "/tmp/x.log" }
def pipeline p {
    input a, b
    output o
    drop
}
"#;
        let cfg = compile(src).unwrap();
        cfg.validate().unwrap();
    }

    // The `to_jsonl` wire-shape tests (schema_version, forbidden
    // routing fields, Event::from_json round-trip) live with the writer
    // in `crate::error_log` — that module owns the JSONL contract even
    // though `ErroredEventContext` itself is built here. See
    // `error_log::tests` for the pin.

    #[test]
    fn process_variant_named_process_site_selection() {
        // Named `def process` invocation surfaces site = "<process_name>".
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def process wrap { egress = strftime(timestamp, "%Y", "UTC") }
def pipeline p { input i; process wrap; output o }
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.errored.len(), 1);
        assert!(
            matches!(&result.errored[0], ErroredEventContext::Process { site, .. } if site == "wrap")
        );
    }

    #[test]
    fn process_variant_inline_site_selection() {
        // Inline `process { ... }` block surfaces site = "(inline)".
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p {
    input i
    process { egress = strftime(timestamp, "%Y", "UTC") }
    output o
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.errored.len(), 1);
        assert!(matches!(
            &result.errored[0],
            ErroredEventContext::Process { site, .. } if site == "(inline)"
        ));
    }

    #[test]
    fn output_capture_strip_all_leaves_live_event_workspace_intact_for_downstream_if() {
        // Contract pin for the output-snapshot workspace strip: dropping workspace from the
        // per-output *snapshot* (the value pushed onto
        // `PipelineExecOut::outputs`) must not affect the *live event*
        // that the executor threads to subsequent pipeline statements.
        // Concretely: a process sets `workspace.route = "keep"`, an
        // `output` statement runs under `StripAll` policy, and the
        // following pipeline-level `if workspace.route == "keep"`
        // still sees the populated workspace and takes its true arm.
        //
        // If this test breaks it means the `Output` arm accidentally
        // consumed / mutated the live event when preparing its
        // workspace-less snapshot.
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output a { type stdout }
def output b { type stdout }
def process tag {
    workspace.route = "keep"
}
def pipeline p {
    input i
    process tag
    output a
    if workspace.route == "keep" {
        output b
    }
    finish
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::StripAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        assert_eq!(result.termination, PipelineTermination::Finished);
        // Both outputs pushed → the pipeline-level `if` correctly read
        // `workspace.route` from the live event after the `output a`
        // statement executed against the strip-all policy.
        let names: Vec<&str> = result.outputs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        // Both snapshots have empty workspace (strip policy took effect).
        assert!(
            result.outputs[0].1.workspace.is_empty(),
            "output 'a' snapshot must have empty workspace under StripAll"
        );
        assert!(
            result.outputs[1].1.workspace.is_empty(),
            "output 'b' snapshot must have empty workspace under StripAll"
        );
    }

    #[test]
    fn output_capture_disk_only_captures_workspace_selectively() {
        // Contract pin: given the DiskOnly policy with `a` marked
        // disk-backed and `b` memory-backed, only `a`'s snapshot
        // carries the workspace. This is the shape the daemon path
        // uses per event.
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
        use bytes::Bytes;
        use std::collections::HashSet;
        use std::net::SocketAddr;

        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output a { type stdout }
def output b { type stdout }
def process tag {
    workspace.route = "keep"
}
def pipeline p {
    input i
    process tag
    output a
    output b
}
"#;
        let cfg = compile(src).unwrap();
        let pipeline = cfg.pipelines.get("p").unwrap();
        let mut funcs = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut funcs, store);
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let disk: HashSet<String> = ["a".to_string()].into_iter().collect();
        let result = run_pipeline(
            pipeline,
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::DiskOnly(&disk),
            &mut bumpalo::Bump::new(),
        )
        .unwrap();
        let a_snapshot = &result.outputs.iter().find(|(n, _)| n == "a").unwrap().1;
        let b_snapshot = &result.outputs.iter().find(|(n, _)| n == "b").unwrap().1;
        assert_eq!(
            a_snapshot.workspace.get("route"),
            Some(&crate::dsl::value::OwnedValue::String("keep".into())),
            "disk-backed output 'a' must carry workspace"
        );
        assert!(
            b_snapshot.workspace.is_empty(),
            "memory-backed output 'b' must have empty workspace"
        );
    }
}

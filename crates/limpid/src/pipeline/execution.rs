use anyhow::{Result, bail};
use thiserror::Error;

use crate::dsl::arena::EventArena;
use crate::dsl::eval::{eval_expr, value_to_string};
use crate::event::{BorrowedEvent, OwnedEvent, QueuedEvent};
use crate::functions::FunctionRegistry;
use crate::tap::TapRegistry;
use tracing::trace;

#[derive(Debug, Error)]
enum ProcessError {
    #[error("process failed: {0}")]
    Failed(String),
}

impl ProcessError {
    fn into_message(self) -> String {
        match self {
            Self::Failed(message) => message,
        }
    }
}

enum ExecResult<'bump> {
    Continue(BorrowedEvent<'bump>),
    Dropped,
}

#[cfg(test)]
std::thread_local! {
    static PROCESS_EVENT_FROM_OWNED_CALLS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn reset_process_event_from_owned_calls_for_testing() {
    PROCESS_EVENT_FROM_OWNED_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn process_event_from_owned_calls_for_testing() -> usize {
    PROCESS_EVENT_FROM_OWNED_CALLS.with(std::cell::Cell::get)
}

/// Trace entry for --test mode output.
#[derive(Debug, PartialEq, Eq)]
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
    ltp_stamps: Option<std::sync::Arc<[crate::ltp::HopStamp]>>,
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
    ltp_stamps: Option<std::sync::Arc<[crate::ltp::HopStamp]>>,
}

impl ProcessEvent {
    /// Snapshot the process-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        #[cfg(test)]
        PROCESS_EVENT_FROM_OWNED_CALLS.with(|calls| calls.set(calls.get() + 1));

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

    #[allow(dead_code)] // slice-view contract retained for snapshot consumers
    pub(crate) fn ltp_stamps(&self) -> &[crate::ltp::HopStamp] {
        self.ltp_stamps.as_deref().unwrap_or(&[])
    }

    pub(crate) fn ltp_stamps_arc(&self) -> Option<std::sync::Arc<[crate::ltp::HopStamp]>> {
        self.ltp_stamps.clone()
    }

    #[cfg(test)]
    pub(crate) fn has_ltp_history_storage(&self) -> bool {
        self.ltp_stamps.is_some()
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

    #[allow(dead_code)] // slice-view contract retained for snapshot consumers
    pub(crate) fn ltp_stamps(&self) -> &[crate::ltp::HopStamp] {
        self.ltp_stamps.as_deref().unwrap_or(&[])
    }

    pub(crate) fn ltp_stamps_arc(&self) -> Option<std::sync::Arc<[crate::ltp::HopStamp]>> {
        self.ltp_stamps.clone()
    }

    #[cfg(test)]
    pub(crate) fn has_ltp_history_storage(&self) -> bool {
        self.ltp_stamps.is_some()
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

/// Compatibility facade for callers that hold a compiled config and pipeline
/// definition. Execution still goes through the same sealed IR and normal
/// registry as the daemon and `--test-pipeline`.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn run_pipeline(
    pipeline: &crate::dsl::ast::PipelineDef,
    event: &OwnedEvent,
    config: &super::CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
) -> Result<PipelineRunResult> {
    let blueprint = super::blueprint::compile_runtime_blueprint(config)?;
    let registry = crate::metrics::Registry::new();
    let bound = blueprint.bind(&registry)?;
    run_pipeline_blueprint(
        &bound,
        &pipeline.name,
        event,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
    )
}

/// Execute the sealed single IR with its per-start metric binding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_blueprint(
    bound: &super::blueprint::BoundRuntimeBlueprint,
    pipeline_name: &str,
    event: &OwnedEvent,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
) -> Result<PipelineRunResult> {
    run_pipeline_blueprint_at(
        bound,
        pipeline_name,
        event,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        crate::time::UnixNanos::now(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_blueprint_at(
    bound: &super::blueprint::BoundRuntimeBlueprint,
    pipeline_name: &str,
    event: &OwnedEvent,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    let pipeline_id = bound
        .blueprint
        .pipeline_id(pipeline_name)
        .ok_or_else(|| anyhow::anyhow!("pipeline '{pipeline_name}' is not in the blueprint"))?;
    run_pipeline_blueprint_by_id_at(
        bound,
        pipeline_id,
        event,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        dispatch_started_at,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_blueprint_by_id_at(
    bound: &super::blueprint::BoundRuntimeBlueprint,
    pipeline_id: super::blueprint::PipelineId,
    event: &OwnedEvent,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    let execution = bound
        .pipeline_execution(pipeline_id)
        .ok_or_else(|| anyhow::anyhow!("pipeline id is not in the bound blueprint"))?;
    run_pipeline_blueprint_resolved_at(
        execution,
        event,
        funcs,
        tap,
        trace,
        output_capture,
        bump,
        dispatch_started_at,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_blueprint_resolved_at(
    execution: &super::blueprint::BoundPipelineExecution,
    event: &OwnedEvent,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<PipelineRunResult> {
    let pipeline = execution.pipeline();
    let pipeline_name = pipeline.name.as_str();
    let metrics = execution.metrics();

    let mut trace = trace;
    if let Some(trace) = trace.as_mut() {
        trace.push(TraceEntry {
            stage: "input".into(),
            label: String::new(),
            detail: format!("ingress: {}", String::from_utf8_lossy(&event.ingress)),
        });
    }

    let arena = EventArena::new(bump);
    let bevent = event.view_in(&arena);
    let process_registry = IrProcessRegistry {
        blueprint: execution.blueprint(),
        pipeline,
        metrics,
        funcs,
        tap,
    };
    let mut outputs = Vec::new();
    let mut errored = Vec::new();
    let ctx = IrPipelineExecCtx {
        pipeline_name,
        original_event: event,
        process_registry: &process_registry,
        funcs,
        arena: &arena,
        output_capture,
        dispatch_started_at,
    };
    let mut out = PipelineExecOut {
        trace,
        outputs: &mut outputs,
        errored: &mut errored,
    };
    let (_, termination) = exec_ir_pipeline_body(&pipeline.code, bevent, &ctx, &mut out)?;
    let had_outputs = !outputs.is_empty();
    Ok(PipelineRunResult {
        outputs,
        had_outputs,
        termination,
        errored,
    })
}

struct IrProcessRegistry<'a> {
    blueprint: &'a super::blueprint::RuntimeBlueprint,
    pipeline: &'a super::blueprint::PipelineBlueprint,
    metrics: &'a super::blueprint::BoundPipelineMetrics,
    funcs: &'a FunctionRegistry,
    tap: Option<&'a TapRegistry>,
}

impl IrProcessRegistry<'_> {
    fn call<'bump>(
        &self,
        target: super::blueprint::ProcessTarget,
        process_name: &str,
        metric_node_id: super::blueprint::MetricNodeId,
        event: BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
        site_kind: super::blueprint::SiteKind,
    ) -> std::result::Result<Option<BorrowedEvent<'bump>>, ProcessError> {
        let node = self
            .pipeline
            .metric_nodes
            .get(metric_node_id.index())
            .ok_or_else(|| ProcessError::Failed("metric node id is out of range".to_owned()))?;
        if node.target != target {
            return Err(ProcessError::Failed(
                "process body and metric node identity mismatch".to_owned(),
            ));
        }
        let counters = self
            .metrics
            .process_counters
            .get(metric_node_id.index())
            .ok_or_else(|| ProcessError::Failed("process counter id is out of range".to_owned()))?;
        counters.start();
        let result = match target {
            super::blueprint::ProcessTarget::Known(body_id) => {
                let body = self
                    .blueprint
                    .process_bodies()
                    .get(body_id.index())
                    .ok_or_else(|| {
                        ProcessError::Failed("process body id is out of range".to_owned())
                    })?;
                if site_kind == super::blueprint::SiteKind::Named {
                    trace!("process '{}' (user-defined): executing", process_name);
                }
                exec_ir_process_body(&body.code, metric_node_id, event, self, self.funcs, arena)
                    .map_err(|error| ProcessError::Failed(error.to_string()))
            }
            super::blueprint::ProcessTarget::Unknown => {
                tracing::warn!(
                    "unknown process '{}', passing event through unchanged",
                    process_name
                );
                Ok(ExecResult::Continue(event))
            }
        };
        match result {
            Ok(ExecResult::Continue(event)) => {
                counters.continued();
                if site_kind == super::blueprint::SiteKind::Named {
                    trace!("process '{}': ok", process_name);
                    self.emit_tap(process_name, &event);
                }
                Ok(Some(event))
            }
            Ok(ExecResult::Dropped) => {
                counters.dropped();
                if site_kind == super::blueprint::SiteKind::Named {
                    trace!("process '{}': dropped", process_name);
                }
                Ok(None)
            }
            Err(error) => {
                counters.errored();
                Err(error)
            }
        }
    }

    fn emit_tap<'bump>(&self, process_name: &str, event: &BorrowedEvent<'bump>) {
        if let Some(tap) = self.tap {
            let key = format!("process {process_name}");
            if tap.is_subscribed(&key) {
                tap.try_emit(&key, &event.to_owned());
            }
        }
    }
}

fn exec_ir_process_body<'bump>(
    code: &[super::blueprint::ProcessCode],
    metric_node_id: super::blueprint::MetricNodeId,
    event: BorrowedEvent<'bump>,
    registry: &IrProcessRegistry<'_>,
    funcs: &FunctionRegistry,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    let mut scope = crate::dsl::eval::LocalScope::new();
    exec_ir_process_code(
        code,
        metric_node_id,
        event,
        registry,
        funcs,
        &mut scope,
        arena,
    )
}

fn exec_ir_process_code<'bump>(
    code: &[super::blueprint::ProcessCode],
    metric_node_id: super::blueprint::MetricNodeId,
    mut event: BorrowedEvent<'bump>,
    registry: &IrProcessRegistry<'_>,
    funcs: &FunctionRegistry,
    scope: &mut crate::dsl::eval::LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    use super::blueprint::ProcessCode;
    for statement in code {
        match statement {
            ProcessCode::Assign(target, expression) => {
                let value = crate::dsl::eval::eval_expr_with_scope(
                    expression, &event, funcs, scope, arena,
                )?;
                crate::dsl::exec::apply_assign(&mut event, target, value, arena)?;
            }
            ProcessCode::LetBinding(name, expression) => {
                let value = crate::dsl::eval::eval_expr_with_scope(
                    expression, &event, funcs, scope, arena,
                )?;
                scope.bind(name, value);
            }
            ProcessCode::Call { name, edge_slot } => {
                let current_node = registry
                    .pipeline
                    .metric_nodes
                    .get(metric_node_id.index())
                    .ok_or_else(|| anyhow::anyhow!("metric node id is out of range"))?;
                let super::blueprint::ProcessTarget::Known(current_body) = current_node.target
                else {
                    bail!("unknown process metric node cannot execute a process body");
                };
                let body = registry
                    .blueprint
                    .process_bodies()
                    .get(current_body.index())
                    .ok_or_else(|| anyhow::anyhow!("process body id is out of range"))?;
                let edge = body
                    .edges
                    .get(edge_slot.index())
                    .ok_or_else(|| anyhow::anyhow!("process edge slot is out of range"))?;
                if edge.name != *name {
                    bail!("process edge identity mismatch");
                }
                let child_node = *current_node
                    .children
                    .get(edge_slot.index())
                    .ok_or_else(|| anyhow::anyhow!("metric child slot is out of range"))?;
                match registry.call(
                    edge.target,
                    &edge.name,
                    child_node,
                    event,
                    arena,
                    super::blueprint::SiteKind::Named,
                )? {
                    Some(next) => event = next,
                    None => return Ok(ExecResult::Dropped),
                }
            }
            ProcessCode::Drop => return Ok(ExecResult::Dropped),
            ProcessCode::Error(expression) => {
                let message = match expression {
                    Some(expression) => value_to_string(&crate::dsl::eval::eval_expr_with_scope(
                        expression, &event, funcs, scope, arena,
                    )?),
                    None => "explicit error routing".to_owned(),
                };
                bail!(message);
            }
            ProcessCode::If {
                branches,
                else_body,
            } => {
                let mut selected = None;
                for (condition, body) in branches {
                    if crate::dsl::eval::eval_expr_with_scope(
                        condition, &event, funcs, scope, arena,
                    )?
                    .is_truthy()
                    {
                        selected = Some(body.as_slice());
                        break;
                    }
                }
                if let Some(body) = selected.or(else_body.as_deref()) {
                    match exec_ir_process_code(
                        body,
                        metric_node_id,
                        event,
                        registry,
                        funcs,
                        scope,
                        arena,
                    )? {
                        ExecResult::Continue(next) => event = next,
                        ExecResult::Dropped => return Ok(ExecResult::Dropped),
                    }
                }
            }
            ProcessCode::Switch { discriminant, arms } => {
                let value = crate::dsl::eval::eval_expr_with_scope(
                    discriminant,
                    &event,
                    funcs,
                    scope,
                    arena,
                )?;
                let mut selected = None;
                for (pattern, body) in arms {
                    match pattern {
                        Some(pattern)
                            if crate::dsl::eval::values_match(
                                &value,
                                &crate::dsl::eval::eval_expr_with_scope(
                                    pattern, &event, funcs, scope, arena,
                                )?,
                            ) =>
                        {
                            selected = Some(body.as_slice());
                            break;
                        }
                        None => {
                            selected = Some(body.as_slice());
                            break;
                        }
                        _ => {}
                    }
                }
                if let Some(body) = selected {
                    match exec_ir_process_code(
                        body,
                        metric_node_id,
                        event,
                        registry,
                        funcs,
                        scope,
                        arena,
                    )? {
                        ExecResult::Continue(next) => event = next,
                        ExecResult::Dropped => return Ok(ExecResult::Dropped),
                    }
                }
            }
            ProcessCode::TryCatch {
                try_body,
                catch_body,
            } => {
                let event_backup = event.snapshot_in(arena);
                let scope_backup = scope.clone();
                match exec_ir_process_code(
                    try_body,
                    metric_node_id,
                    event,
                    registry,
                    funcs,
                    scope,
                    arena,
                ) {
                    Ok(ExecResult::Continue(next)) => event = next,
                    Ok(ExecResult::Dropped) => return Ok(ExecResult::Dropped),
                    Err(error) => {
                        *scope = scope_backup;
                        let mut recovered = event_backup;
                        let message = arena.alloc_str(&error.to_string());
                        recovered.workspace_set_str(
                            arena,
                            "_error",
                            crate::dsl::value::Value::String(message),
                        );
                        match exec_ir_process_code(
                            catch_body,
                            metric_node_id,
                            recovered,
                            registry,
                            funcs,
                            scope,
                            arena,
                        )? {
                            ExecResult::Continue(mut next) => {
                                next.workspace_remove("_error");
                                event = next;
                            }
                            ExecResult::Dropped => return Ok(ExecResult::Dropped),
                        }
                    }
                }
            }
            ProcessCode::Expr(expression) => {
                let value = crate::dsl::eval::eval_expr_with_scope(
                    expression, &event, funcs, scope, arena,
                )?;
                match value {
                    crate::dsl::value::Value::Object(entries) => {
                        for (key, value) in entries {
                            event.workspace_set(key, *value);
                        }
                    }
                    crate::dsl::value::Value::Null => {}
                    other => bail!(
                        "bare expression statement must return Object or Null; got {}",
                        other.type_name()
                    ),
                }
            }
        }
    }
    Ok(ExecResult::Continue(event))
}

struct IrPipelineExecCtx<'a, 'bump: 'a> {
    pipeline_name: &'a str,
    original_event: &'a OwnedEvent,
    process_registry: &'a IrProcessRegistry<'a>,
    funcs: &'a FunctionRegistry,
    arena: &'bump EventArena<'bump>,
    output_capture: OutputCapturePolicy<'a>,
    dispatch_started_at: crate::time::UnixNanos,
}

fn exec_ir_pipeline_body<'bump>(
    code: &[super::blueprint::PipelineCode],
    mut event: BorrowedEvent<'bump>,
    ctx: &IrPipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    use super::blueprint::PipelineCode;
    for statement in code {
        match statement {
            PipelineCode::Input(_) => {}
            PipelineCode::ProcessChain(sites) => {
                for site in sites {
                    match ctx.process_registry.call(
                        super::blueprint::ProcessTarget::Known(site.body),
                        &site.name,
                        site.metric_node,
                        event,
                        ctx.arena,
                        site.kind,
                    ) {
                        Ok(Some(next)) => {
                            out.push_trace(|| TraceEntry {
                                stage: "process".into(),
                                label: site.name.clone(),
                                detail: "ok".into(),
                            });
                            event = next;
                        }
                        Ok(None) => {
                            out.push_trace(|| TraceEntry {
                                stage: "process".into(),
                                label: site.name.clone(),
                                detail: "dropped".into(),
                            });
                            return Ok((None, PipelineTermination::Dropped));
                        }
                        Err(error) => {
                            let reason = match site.kind {
                                super::blueprint::SiteKind::Named => {
                                    tracing::warn!(
                                        "process '{}': {} — event routed to error_log",
                                        site.name,
                                        error
                                    );
                                    error.to_string()
                                }
                                super::blueprint::SiteKind::Inline => {
                                    let reason = error.into_message();
                                    tracing::warn!(
                                        "inline process: {} — event routed to error_log",
                                        reason
                                    );
                                    reason
                                }
                            };
                            out.push_trace(|| TraceEntry {
                                stage: "process".into(),
                                label: site.name.clone(),
                                detail: format!("error: {reason} (event → error_log)"),
                            });
                            out.errored.push(ErroredEventContext::Process {
                                timestamp: chrono::Utc::now(),
                                pipeline: ctx.pipeline_name.to_owned(),
                                site: site.name.clone(),
                                reason,
                                event: ProcessEvent::from_owned(ctx.original_event),
                            });
                            return Ok((None, PipelineTermination::Errored));
                        }
                    }
                }
            }
            PipelineCode::Output { name, timer_slot } => {
                trace!(target: "limpid::pipeline", "output → {}", name);
                out.push_trace(|| TraceEntry {
                    stage: "output".into(),
                    label: format!("→ {name}"),
                    detail: String::new(),
                });
                let snapshot = if ctx.output_capture.should_capture_workspace(name) {
                    event.to_owned()
                } else {
                    event.to_owned_without_workspace()
                };
                let emitted_at = crate::time::UnixNanos::now();
                ctx.process_registry
                    .metrics
                    .output_timers
                    .get(timer_slot.index())
                    .ok_or_else(|| anyhow::anyhow!("output timer slot is out of range"))?
                    .observe_between(ctx.dispatch_started_at, emitted_at);
                out.outputs
                    .push((name.clone(), QueuedEvent::new(snapshot, emitted_at)));
            }
            PipelineCode::Drop => {
                trace!(target: "limpid::pipeline", "drop");
                out.push_trace(|| TraceEntry {
                    stage: "drop".into(),
                    label: String::new(),
                    detail: String::new(),
                });
                return Ok((None, PipelineTermination::Dropped));
            }
            PipelineCode::Finish => {
                trace!(target: "limpid::pipeline", "finish");
                out.push_trace(|| TraceEntry {
                    stage: "finish".into(),
                    label: String::new(),
                    detail: String::new(),
                });
                return Ok((None, PipelineTermination::Finished));
            }
            PipelineCode::Error(expression) => {
                let message = match expression {
                    Some(expression) => {
                        value_to_string(&eval_expr(expression, &event, ctx.funcs, ctx.arena)?)
                    }
                    None => "explicit error routing".to_owned(),
                };
                tracing::warn!(
                    "pipeline '{}': error '{}' — event routed to error_log",
                    ctx.pipeline_name,
                    message
                );
                out.push_trace(|| TraceEntry {
                    stage: "error".into(),
                    label: message.clone(),
                    detail: "event → error_log".into(),
                });
                out.errored.push(ErroredEventContext::Process {
                    timestamp: chrono::Utc::now(),
                    pipeline: ctx.pipeline_name.to_owned(),
                    site: "(pipeline)".to_owned(),
                    reason: message,
                    event: ProcessEvent::from_owned(ctx.original_event),
                });
                return Ok((None, PipelineTermination::Errored));
            }
            PipelineCode::If {
                branches,
                else_body,
            } => {
                let mut selected = None;
                for (condition, body) in branches {
                    if eval_expr(condition, &event, ctx.funcs, ctx.arena)?.is_truthy() {
                        selected = Some(body.as_slice());
                        break;
                    }
                }
                if let Some(body) = selected.or(else_body.as_deref()) {
                    match exec_ir_pipeline_body(body, event, ctx, out)? {
                        (Some(next), _) => event = next,
                        (None, termination) => return Ok((None, termination)),
                    }
                }
            }
            PipelineCode::Switch { discriminant, arms } => {
                let value = eval_expr(discriminant, &event, ctx.funcs, ctx.arena)?;
                let mut selected = None;
                for (pattern, body) in arms {
                    match pattern {
                        Some(pattern)
                            if crate::dsl::eval::values_match(
                                &value,
                                &eval_expr(pattern, &event, ctx.funcs, ctx.arena)?,
                            ) =>
                        {
                            selected = Some(body.as_slice());
                            break;
                        }
                        None => {
                            selected = Some(body.as_slice());
                            break;
                        }
                        _ => {}
                    }
                }
                if let Some(body) = selected {
                    match exec_ir_pipeline_body(body, event, ctx, out)? {
                        (Some(next), _) => event = next,
                        (None, termination) => return Ok((None, termination)),
                    }
                }
            }
        }
    }
    Ok((Some(event), PipelineTermination::Finished))
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

#[cfg(test)]
mod tests {
    use super::super::CompiledConfig;
    use super::*;
    use crate::dsl::parser::parse_config;
    use crate::event::{reset_snapshot_in_calls_for_testing, snapshot_in_calls_for_testing};
    use crate::functions::{FunctionRegistry, register_builtins, table::TableStore};
    use bytes::Bytes;
    use chrono::{TimeZone, Utc};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata, Subscriber};

    struct CapturingSubscriber {
        messages: Arc<Mutex<Vec<String>>>,
        next_span: AtomicU64,
    }

    struct MessageVisitor(Option<String>);

    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    impl Subscriber for CapturingSubscriber {
        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            // The tracing-core callsite cache is global. `sometimes` forces
            // each thread-local dispatch to evaluate `enabled` even when a
            // parallel test first registered this callsite as disabled.
            Interest::sometimes()
        }

        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0 {
                self.messages
                    .lock()
                    .expect("trace capture lock")
                    .push(message);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    fn capture_tracing(f: impl FnOnce()) -> String {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let keepalive = tracing::Dispatch::new(CapturingSubscriber {
            messages: Arc::new(Mutex::new(Vec::new())),
            next_span: AtomicU64::new(0),
        });
        let dispatch = tracing::Dispatch::new(CapturingSubscriber {
            messages: Arc::clone(&messages),
            next_span: AtomicU64::new(0),
        });
        // Keep two scoped dispatches registered while capturing. tracing-core's
        // single-dispatch fast path rebuilds against the current default before
        // `with_default` installs a newly-created dispatch; a parallel test can
        // therefore leave a first-use callsite cached as disabled. The second
        // live dispatch forces the ordinary multi-dispatch rebuild, while each
        // callsite remains dynamically filtered through `Interest::sometimes`.
        // This neither resets nor explicitly rebuilds the global cache.
        tracing::dispatcher::with_default(&dispatch, f);
        drop(keepalive);
        messages.lock().expect("trace capture lock").join("\n")
    }

    fn functions() -> FunctionRegistry {
        let mut functions = FunctionRegistry::new();
        register_builtins(
            &mut functions,
            TableStore::from_configs(Vec::new()).unwrap(),
        );
        functions
    }

    fn functions_for(config: &CompiledConfig) -> FunctionRegistry {
        let mut functions = functions();
        crate::functions::register_user_functions(&mut functions, config);
        functions
    }

    fn run_with_trace(
        source: &str,
        pipeline_name: &str,
        event: &OwnedEvent,
    ) -> (PipelineRunResult, Vec<TraceEntry>) {
        let config = CompiledConfig::from_config(parse_config(source).expect("parse fixture"))
            .expect("compile fixture");
        let functions = functions_for(&config);
        let blueprint =
            super::super::blueprint::compile_runtime_blueprint(&config).expect("compile sealed IR");
        let registry = crate::metrics::Registry::new();
        let bound = blueprint.bind(&registry).expect("bind sealed IR");
        let mut trace = Vec::new();
        let result = run_pipeline_blueprint_at(
            &bound,
            pipeline_name,
            event,
            &functions,
            None,
            Some(&mut trace),
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
            crate::time::UnixNanos::new(100),
        )
        .expect("run sealed executor");
        (result, trace)
    }

    fn run(source: &str, pipeline: &str, event: &OwnedEvent) -> PipelineRunResult {
        let config = CompiledConfig::from_config(parse_config(source).unwrap()).unwrap();
        run_pipeline(
            config.pipelines.get(pipeline).unwrap(),
            event,
            &config,
            &functions(),
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .unwrap()
    }

    fn binary_ltp_event() -> OwnedEvent {
        OwnedEvent::from_ltp_parts(
            uuid::Uuid::now_v7(),
            Utc.timestamp_nanos(123),
            "127.0.0.1:1514".parse().unwrap(),
            Bytes::from_static(&[0, 0xff, b'\n', 0x80]),
            vec![crate::ltp::HopStamp {
                node_id: "upstream".to_owned(),
                arrival_unix_nano: 100,
                departure_unix_nano: 110,
            }],
        )
    }

    #[test]
    fn sealed_ir_deep_control_flow_has_exact_outputs_workspace_and_trace_order() {
        let source = r#"
def process leaf {
    let selected = "first"
    if true { workspace.if_value = selected } else { workspace.if_value = "wrong" }
    switch selected {
        "first" { workspace.switch_value = "matched" }
        default { error "wrong arm" }
    }

    try {
        workspace.rolled_back = "wrong"
        error "caught"
    } catch {
        workspace.caught = workspace._error
    }
    egress = "first:matched"
}
def process parent { process leaf }
def pipeline p {
    process parent | { workspace.inline = "yes" }
    if workspace.inline == "yes" { output first } else { error "wrong branch" }
    switch workspace.switch_value {
        "matched" { output second }
        default { drop }
    }
    finish
}
"#;
        let event = binary_ltp_event();
        let (result, trace) = run_with_trace(source, "p", &event);
        assert_eq!(result.termination, PipelineTermination::Finished);
        assert_eq!(
            trace
                .iter()
                .map(|entry| (&*entry.stage, &*entry.label))
                .collect::<Vec<_>>(),
            vec![
                ("input", ""),
                ("process", "parent"),
                ("process", "(inline)"),
                ("output", "→ first"),
                ("output", "→ second"),
                ("finish", ""),
            ]
        );
        assert_eq!(result.outputs.len(), 2);
        assert_eq!(result.outputs[0].0, "first");
        assert_eq!(result.outputs[1].0, "second");
        for (_, output) in &result.outputs {
            assert_eq!(output.egress, Bytes::from_static(b"first:matched"));
            assert_eq!(
                output.workspace.get("inline"),
                Some(&crate::dsl::value::OwnedValue::String("yes".into()))
            );
            assert!(!output.workspace.contains_key("rolled_back"));
            assert_eq!(output.ltp_stamps(), event.ltp_stamps());
        }
    }

    #[test]
    fn nested_unknown_process_passes_through_and_counts_the_unknown_frame() {
        let source = r#"
def process parent { process missing }
def pipeline p { process parent; output sink; finish }
"#;
        let config = CompiledConfig::from_config(parse_config(source).expect("parse fixture"))
            .expect("compile fixture");
        let functions = functions_for(&config);
        let blueprint =
            super::super::blueprint::compile_runtime_blueprint(&config).expect("seal blueprint");
        let registry = crate::metrics::Registry::new();
        let bound = blueprint.bind(&registry).expect("bind blueprint");
        let event = OwnedEvent::new(
            Bytes::from_static(b"raw"),
            "127.0.0.1:1514".parse().unwrap(),
        );
        let result = run_pipeline_blueprint(
            &bound,
            "p",
            &event,
            &functions,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .expect("unknown nested process is a runtime passthrough");
        assert_eq!(result.termination, PipelineTermination::Finished);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].1.ingress, Bytes::from_static(b"raw"));

        let snapshot = serde_json::to_value(registry.snapshot()).expect("serialize metrics");
        for family in [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
        ] {
            let series = snapshot["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["name"] == family)
                .unwrap_or_else(|| panic!("missing family {family}"))["series"]
                .as_array()
                .unwrap()
                .iter()
                .find(|series| {
                    series["labels"]
                        == serde_json::json!({
                            "pipeline": "p",
                            "step": "2",
                            "process_path": "/parent/missing",
                            "process_name": "missing",
                        })
                })
                .unwrap_or_else(|| panic!("missing nested unknown series for {family}"));
            assert_eq!(series["value"], 1, "unexpected {family} count");
        }
    }

    #[tokio::test]
    async fn inline_nested_unknown_process_preserves_event_and_emits_its_tap() {
        let source = r#"
def pipeline p { process { process missing }; output sink; finish }
"#;
        let config = CompiledConfig::from_config(parse_config(source).expect("parse fixture"))
            .expect("compile fixture");
        let functions = functions_for(&config);
        let blueprint =
            super::super::blueprint::compile_runtime_blueprint(&config).expect("seal blueprint");
        let registry = crate::metrics::Registry::new();
        let bound = blueprint.bind(&registry).expect("bind blueprint");
        let tap = TapRegistry::new();
        tap.register("process missing").await;
        tap.register("process (inline)").await;
        let mut subscription = tap
            .subscribe("process missing")
            .await
            .expect("missing-process tap");
        let mut inline_subscription = tap
            .subscribe("process (inline)")
            .await
            .expect("test-only inline tap registration");
        let event = OwnedEvent::new(
            Bytes::from_static(b"inline-raw"),
            "127.0.0.1:1514".parse().unwrap(),
        );
        let result = run_pipeline_blueprint(
            &bound,
            "p",
            &event,
            &functions,
            Some(&tap),
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .expect("inline unknown process is a runtime passthrough");
        assert_eq!(result.outputs[0].1.ingress, event.ingress);
        let tapped = tokio::time::timeout(std::time::Duration::from_secs(1), subscription.recv())
            .await
            .expect("tap delivery timeout")
            .expect("tap delivery");
        assert_eq!(tapped.ingress, event.ingress);
        assert_eq!(tapped.key(), event.key());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                inline_subscription.recv()
            )
            .await
            .is_err(),
            "inline lexical sites must not emit a ghost process tap"
        );

        let pipeline = blueprint.pipeline("p").expect("pipeline p");
        let missing = pipeline
            .metric_nodes
            .iter()
            .find(|node| node.process_name == "missing")
            .expect("unknown metric frame");
        assert_eq!(missing.process_path, "/(inline)/missing");
    }

    #[test]
    fn sealed_ir_drop_and_error_diagnostics_are_exact() {
        let event = binary_ltp_event();
        for (
            source,
            pipeline,
            expected_termination,
            expected_site,
            expected_reason,
            expected_process_detail,
        ) in [
            (
                r#"
def process fail { workspace.partial = "discarded"; error "expected" }
def process parent { process fail }
def pipeline error_path { process parent; output unreachable; finish }
"#,
                "error_path",
                PipelineTermination::Errored,
                Some("parent"),
                Some("process failed: process failed: expected"),
                Some("error: process failed: process failed: expected (event → error_log)"),
            ),
            (
                r#"def pipeline inline_error { process { error "inline expected" } }"#,
                "inline_error",
                PipelineTermination::Errored,
                Some("(inline)"),
                Some("inline expected"),
                Some("error: inline expected (event → error_log)"),
            ),
            (
                r#"def pipeline pipeline_error { error "pipeline expected" }"#,
                "pipeline_error",
                PipelineTermination::Errored,
                Some("(pipeline)"),
                Some("pipeline expected"),
                None,
            ),
            (
                r#"def pipeline dropped { drop }"#,
                "dropped",
                PipelineTermination::Dropped,
                None,
                None,
                None,
            ),
        ] {
            let (result, trace) = run_with_trace(source, pipeline, &event);
            assert_eq!(result.termination, expected_termination);
            match (expected_site, expected_reason) {
                (Some(site), Some(reason)) => {
                    assert_eq!(result.errored.len(), 1);
                    assert_eq!(result.errored[0].site(), site);
                    assert_eq!(result.errored[0].reason(), reason);
                    assert_eq!(result.errored[0].payload_size_hint(), event.ingress.len());
                }
                _ => assert!(result.errored.is_empty()),
            }
            assert_eq!(
                trace.first().map(|entry| entry.stage.as_str()),
                Some("input")
            );
            if let Some(detail) = expected_process_detail {
                let process = trace
                    .iter()
                    .find(|entry| entry.stage == "process")
                    .expect("process diagnostic trace");
                assert_eq!(process.detail, detail);
            }
        }
    }

    #[test]
    fn process_error_diagnostics_match_the_baseline_for_all_root_site_shapes() {
        let event = binary_ltp_event();
        for (source, site, reason, warning) in [
            (
                r#"def process direct { error "expected" }
                   def pipeline p { process direct }"#,
                "direct",
                "process failed: expected",
                "process 'direct': process failed: expected — event routed to error_log",
            ),
            (
                r#"def process nested { error "expected" }
                   def process outer { process nested }
                   def pipeline p { process outer }"#,
                "outer",
                "process failed: process failed: expected",
                "process 'outer': process failed: process failed: expected — event routed to error_log",
            ),
            (
                r#"def pipeline p { process { error "inline expected" } }"#,
                "(inline)",
                "inline expected",
                "inline process: inline expected — event routed to error_log",
            ),
            (
                r#"def process nested { error "expected" }
                   def pipeline p { process { process nested } }"#,
                "(inline)",
                "process failed: expected",
                "inline process: process failed: expected — event routed to error_log",
            ),
        ] {
            let mut observed = None;
            let logs = capture_tracing(|| {
                observed = Some(run_with_trace(source, "p", &event));
            });
            let (result, trace) = observed.expect("captured execution result");
            let warnings = logs
                .lines()
                .filter(|line| line.contains("event routed to error_log"))
                .collect::<Vec<_>>();
            assert_eq!(warnings, [warning]);
            assert_eq!(result.termination, PipelineTermination::Errored);
            assert_eq!(result.errored.len(), 1);
            assert_eq!(result.errored[0].site(), site);
            assert_eq!(result.errored[0].reason(), reason);
            let process = trace
                .iter()
                .find(|entry| entry.stage == "process")
                .expect("process diagnostic trace");
            assert_eq!(process.label, site);
            assert_eq!(
                process.detail,
                format!("error: {reason} (event → error_log)")
            );
        }
    }

    #[test]
    fn test_pipeline_source_uses_bound_ir_and_discards_its_normal_registry() {
        let main = include_str!("../main.rs");
        let run_test = main
            .split("fn run_test(")
            .nth(1)
            .expect("run_test function")
            .split("fn build_test_event")
            .next()
            .expect("run_test body");
        assert!(run_test.contains("compile_runtime_blueprint(&compiled)"));
        assert!(run_test.contains("crate::metrics::Registry::new()"));
        assert!(run_test.contains("blueprint.bind(&metric_registry)"));
        assert!(run_test.contains("run_pipeline_blueprint("));
        assert!(!run_test.contains("run_pipeline("));
        assert!(!run_test.contains("process_metrics: Option"));
        assert!(
            run_test.find("compile_runtime_blueprint").unwrap()
                < run_test.find("runtime::init_tables").unwrap(),
            "IR compile/seal/bind failures must occur before external table acquisition"
        );
    }

    fn assert_process_jsonl_matches_original(result: &PipelineRunResult, original: &OwnedEvent) {
        assert_eq!(result.termination, PipelineTermination::Errored);
        assert_eq!(result.errored.len(), 1);
        let actual = &result.errored[0];
        let expected = match actual {
            ErroredEventContext::Process {
                timestamp,
                pipeline,
                site,
                reason,
                ..
            } => ErroredEventContext::Process {
                timestamp: *timestamp,
                pipeline: pipeline.clone(),
                site: site.clone(),
                reason: reason.clone(),
                event: ProcessEvent::from_owned(original),
            },
            other => panic!("expected process error, got {other:?}"),
        };
        assert_eq!(actual.to_jsonl(), expected.to_jsonl());
    }

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

    #[test]
    fn named_process_success_takes_no_dlq_snapshot() {
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        reset_snapshot_in_calls_for_testing();
        reset_process_event_from_owned_calls_for_testing();

        let result = run(
            r#"
def process pass { egress = ingress }
def pipeline p { process pass }
"#,
            "p",
            &event,
        );

        assert_eq!(result.termination, PipelineTermination::Finished);
        assert_eq!(snapshot_in_calls_for_testing(), 0);
        assert_eq!(process_event_from_owned_calls_for_testing(), 0);
    }

    #[test]
    fn inline_process_success_takes_no_dlq_snapshot() {
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        reset_snapshot_in_calls_for_testing();
        reset_process_event_from_owned_calls_for_testing();

        let result = run(
            r#"
def pipeline p { process { egress = ingress } }
"#,
            "p",
            &event,
        );

        assert_eq!(result.termination, PipelineTermination::Finished);
        assert_eq!(snapshot_in_calls_for_testing(), 0);
        assert_eq!(process_event_from_owned_calls_for_testing(), 0);
    }

    #[test]
    fn named_process_error_jsonl_matches_the_original_binary_ltp_header() {
        let event = binary_ltp_event();
        reset_process_event_from_owned_calls_for_testing();
        let result = run(
            r#"
def process fail { error "named failure" }
def pipeline p { process fail }
"#,
            "p",
            &event,
        );

        assert_eq!(process_event_from_owned_calls_for_testing(), 1);
        let ErroredEventContext::Process {
            event: captured, ..
        } = &result.errored[0]
        else {
            panic!("expected process error");
        };
        assert!(std::sync::Arc::ptr_eq(
            event.ltp_stamps_arc().as_ref().unwrap(),
            captured.ltp_stamps_arc().as_ref().unwrap(),
        ));
        assert_process_jsonl_matches_original(&result, &event);
    }

    #[test]
    fn inline_process_error_jsonl_matches_the_original_binary_ltp_header() {
        let event = binary_ltp_event();
        reset_process_event_from_owned_calls_for_testing();
        let result = run(
            r#"
def pipeline p { process { error "inline failure" } }
"#,
            "p",
            &event,
        );

        assert_eq!(process_event_from_owned_calls_for_testing(), 1);
        assert_eq!(result.errored[0].site(), "(inline)");
        assert_eq!(result.errored[0].reason(), "inline failure");
        let jsonl = result.errored[0].to_jsonl();
        assert!(jsonl.contains(r#""reason":"inline failure""#));
        assert!(!jsonl.contains("process failed: inline failure"));
        assert_process_jsonl_matches_original(&result, &event);
    }

    #[test]
    fn pipeline_error_jsonl_matches_the_original_binary_ltp_header() {
        let event = binary_ltp_event();
        reset_process_event_from_owned_calls_for_testing();
        let result = run(
            r#"
def pipeline p { error "pipeline failure" }
"#,
            "p",
            &event,
        );

        assert_eq!(process_event_from_owned_calls_for_testing(), 1);
        assert_process_jsonl_matches_original(&result, &event);
    }

    #[test]
    fn named_process_error_with_empty_ltp_omits_history() {
        let event = OwnedEvent::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let result = run(
            r#"
def process fail { error "named failure" }
def pipeline p { process fail }
"#,
            "p",
            &event,
        );
        let json: serde_json::Value = serde_json::from_str(&result.errored[0].to_jsonl()).unwrap();

        let ErroredEventContext::Process {
            event: captured, ..
        } = &result.errored[0]
        else {
            panic!("expected process error");
        };
        assert!(!captured.has_ltp_history_storage());
        assert!(json["event"].get("ltp_stamps").is_none());
    }

    #[test]
    fn fanout_pipelines_share_the_same_original_process_dlq_header() {
        let event = binary_ltp_event();
        reset_process_event_from_owned_calls_for_testing();
        let source = r#"
def process fail { error "named failure" }
def pipeline named { process fail }
def pipeline inline { process { error "inline failure" } }
"#;

        let named = run(source, "named", &event);
        let inline = run(source, "inline", &event);
        let named_json: serde_json::Value =
            serde_json::from_str(&named.errored[0].to_jsonl()).unwrap();
        let inline_json: serde_json::Value =
            serde_json::from_str(&inline.errored[0].to_jsonl()).unwrap();

        assert_eq!(named_json["event"], inline_json["event"]);
        assert_eq!(
            named_json["event"]["ingress"],
            event.to_json_value()["ingress"]
        );
        assert_eq!(
            named_json["event"]["ltp_stamps"],
            event.to_json_value()["ltp_stamps"]
        );
        assert!(named_json["event"]["egress"].is_null());
        assert!(named_json["event"]["workspace"].is_null());
        assert_eq!(process_event_from_owned_calls_for_testing(), 2);
    }

    #[test]
    fn try_catch_keeps_its_single_rollback_snapshot() {
        let event = binary_ltp_event();
        reset_snapshot_in_calls_for_testing();
        reset_process_event_from_owned_calls_for_testing();

        let result = run(
            r#"
def process recover {
    try { workspace.before = "mutated" egress = "partial" error "expected" }
    catch { egress = "recovered" }
}
def output o { type stdout }
def pipeline p { process recover output o }
"#,
            "p",
            &event,
        );

        assert_eq!(result.termination, PipelineTermination::Finished);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].1.egress, Bytes::from_static(b"recovered"));
        assert!(!result.outputs[0].1.workspace.contains_key("before"));
        assert!(std::sync::Arc::ptr_eq(
            event.ltp_stamps_arc().as_ref().unwrap(),
            result.outputs[0].1.ltp_stamps_arc().as_ref().unwrap(),
        ));
        assert_eq!(snapshot_in_calls_for_testing(), 1);
        assert_eq!(process_event_from_owned_calls_for_testing(), 0);
    }

    #[test]
    fn process_executor_does_not_write_immutable_header_fields() {
        let source = include_str!("execution.rs");
        let ir_process = &source[source.find("fn exec_ir_process_code").unwrap()
            ..source.find("struct IrPipelineExecCtx").unwrap()];
        for forbidden in [".ingress =", ".source =", ".received_at =", ".ltp_stamps ="] {
            assert!(
                !ir_process.contains(forbidden),
                "process executor mutates immutable event header via {forbidden}"
            );
        }
        assert_eq!(
            ir_process
                .matches("let event_backup = event.snapshot_in(arena);")
                .count(),
            1
        );
        assert_eq!(ir_process.matches("snapshot_in(arena)").count(), 1);
    }

    #[test]
    fn operational_tracing_matches_the_base_executor_without_name_allocation() {
        let source = include_str!("execution.rs");
        let process_call = &source[source.find("impl IrProcessRegistry").unwrap()
            ..source.find("fn exec_ir_process_body").unwrap()];
        for message in [
            "process '{}' (user-defined): executing",
            "unknown process '{}', passing event through unchanged",
            "process '{}': ok",
            "process '{}': dropped",
        ] {
            assert!(
                process_call.contains(message),
                "missing base trace: {message}"
            );
        }
        assert!(!process_call.contains("process_name.to_owned()"));
        assert!(!process_call.contains("process_name.to_string()"));

        let pipeline = &source[source.find("fn exec_ir_pipeline_body").unwrap()
            ..source.find("pub enum OutputCapturePolicy").unwrap()];
        for message in [
            "trace!(target: \"limpid::pipeline\", \"drop\")",
            "trace!(target: \"limpid::pipeline\", \"finish\")",
            "pipeline '{}': error '{}' — event routed to error_log",
        ] {
            assert!(pipeline.contains(message), "missing base trace: {message}");
        }
        assert!(pipeline.contains("match site.kind"));
        assert!(pipeline.contains("site.kind,"));
        assert!(pipeline.contains("inline process: {} — event routed to error_log"));
        assert!(!pipeline.contains("starts_with(\"(inline"));
        assert!(!pipeline.contains("process_bodies()[site.body.index()]"));
    }

    #[test]
    fn operational_tracing_distinguishes_named_unknown_and_root_inline_sites() {
        let event = binary_ltp_event();
        let logs = capture_tracing(|| {
            for source in [
                r#"def process named { egress = "ok" }
                   def pipeline p { process named }"#,
                r#"def process named { drop }
                   def pipeline p { process named }"#,
                r#"def pipeline p { process { egress = "inline-ok" } }"#,
                r#"def pipeline p { process { drop } }"#,
                r#"def pipeline p { process { error "inline expected" } }"#,
                r#"def process parent { process missing }
                   def pipeline p { process parent }"#,
            ] {
                let _ = run_with_trace(source, "p", &event);
            }
        });
        assert_eq!(
            logs.lines().collect::<Vec<_>>(),
            [
                "process 'named' (user-defined): executing",
                "process 'named': ok",
                "process 'named' (user-defined): executing",
                "process 'named': dropped",
                "inline process: inline expected — event routed to error_log",
                "process 'parent' (user-defined): executing",
                "unknown process 'missing', passing event through unchanged",
                "process 'missing': ok",
                "process 'parent': ok",
            ],
            "base operational tracing order and wording must remain exact"
        );
        assert!(!logs.contains("inline-ok"));
        assert!(!logs.contains("(inline"));
    }

    #[test]
    fn single_ir_process_chain_constructs_dlq_headers_only_in_its_cold_error_arm() {
        let source = include_str!("execution.rs");
        let production = source
            .split_once("#[cfg(test)]\nmod tests")
            .expect("tests follow production")
            .0;
        let ir = production
            .split_once("pub(crate) fn run_pipeline_blueprint_by_id_at")
            .expect("single IR runner exists")
            .1
            .split_once("fn exec_ir_process_body")
            .expect("single IR runner precedes process executor")
            .0;
        assert_eq!(ir.matches("original_event: event,").count(), 1);
        let body = production
            .split_once("PipelineCode::ProcessChain(sites) => {")
            .expect("single IR process-chain arm exists")
            .1
            .split_once("PipelineCode::Output { name, timer_slot } => {")
            .expect("single IR output arm follows process-chain arm")
            .0;

        assert_eq!(
            body.matches("ProcessEvent::from_owned(ctx.original_event)")
                .count(),
            1
        );
        assert!(!body.contains("ingress.clone()"));
        assert!(!body.contains("ltp_stamps_arc()"));
    }
}

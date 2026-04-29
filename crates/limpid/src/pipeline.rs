//! Pipeline engine: compiles DSL definitions into an executable pipeline
//! and runs events through process chains.
//!
//! The boundary between **owned** and **borrowed (arena)** event forms
//! is drawn at [`run_pipeline`]: the function takes an [`OwnedEvent`]
//! (which is what the input layer / channel hands over), creates a
//! per-event [`bumpalo::Bump`], and views the event into the arena.
//! Everything inside the pipeline executor — eval, exec, function
//! dispatch — operates on [`BorrowedEvent<'bump>`]. At each output sink
//! and at each error path we cross back to the heap by calling
//! [`BorrowedEvent::to_owned`], so the post-pipeline code (channel
//! sends, DLQ persistence) keeps the same `OwnedEvent` shape it had
//! before v0.6.0.

use std::collections::HashMap;

use anyhow::{Result, bail};
use tracing::trace;

use std::sync::Arc;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::*;
use crate::dsl::eval::{eval_expr, is_truthy, value_to_string, values_match};
use crate::dsl::exec::{ExecResult, ProcessError, ProcessRegistry, exec_process_body};
use crate::dsl::value::Value;
use crate::event::{BorrowedEvent, OwnedEvent};
use crate::functions::FunctionRegistry;
use crate::modules::{ModuleRegistry, Output};
use crate::queue::{QueueKind, SinkInput};
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
    /// Per-output queue kind, derived from each output's `queue { type }`.
    /// Drives the pipeline's output-statement dispatch: `Memory` outputs
    /// take the render hot path (`SinkInput::Rendered`), `Disk` outputs
    /// take the owned-event persist path (`SinkInput::Owned`).
    pub outputs_queue_kind: HashMap<String, QueueKind>,
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

        let mut outputs_queue_kind: HashMap<String, QueueKind> = HashMap::new();
        for (name, output_def) in &outputs {
            let kind =
                crate::queue::QueueConfig::kind_from_output_properties(&output_def.properties);
            outputs_queue_kind.insert(name.clone(), kind);
        }

        let compiled = Self {
            inputs,
            outputs,
            processes,
            pipelines,
            functions,
            global_blocks,
            outputs_queue_kind,
        };
        Ok(compiled)
    }

    /// Validate cross-references: all referenced inputs, outputs, and processes exist.
    ///
    /// `_builtins` is kept in the signature for callers that want to
    /// validate against registered inputs/outputs in the future; process
    /// names are now resolved exclusively against user-defined DSL
    /// processes (v0.3.0 Block 4 removed the native process layer).
    pub fn validate(&self, _builtins: &ModuleRegistry) -> Result<()> {
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
                    if let ProcessChainElement::Named(proc_name, _) = element
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

/// Failure context surfaced when a pipeline terminates with [`PipelineTermination::Errored`].
///
/// The `event` carries the **original** ingress / source / received_at
/// (egress and workspace are intentionally not snapshotted — at the
/// point of failure they may hold partial state from earlier processes
/// in the chain, which would confuse `inject --json` replay). Replay
/// re-runs the pipeline from scratch on `event`.
#[derive(Debug, Clone)]
pub struct ErroredEventContext {
    /// Wall-clock at which the error was raised.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Pipeline name (from `def pipeline <name>`).
    pub pipeline: String,
    /// Failed process: a named `def process` invocation surfaces its
    /// name; an inline `process { ... }` block surfaces `(inline)`.
    pub process: String,
    /// Stringified `ProcessError` / `anyhow::Error` from the failure.
    pub reason: String,
    /// Pre-failure event with original ingress / source / received_at.
    /// Heap-owned so it can outlive the per-event arena that produced it.
    pub event: OwnedEvent,
}

impl ErroredEventContext {
    /// Serialise as a single-line JSON record for the dead-letter queue.
    ///
    /// The `event` sub-object only carries `source` / `received_at` /
    /// `ingress` — exactly the fields `OwnedEvent::from_json` (and
    /// therefore `limpidctl inject --json`) needs to reconstruct a fresh
    /// event. `egress` is omitted because at the failure point it may
    /// be a partial result of earlier processes in the chain;
    /// `workspace` is omitted for the same reason. Replay should
    /// rebuild both from scratch.
    ///
    /// Format is operator-stable: pre-1.0 we may add new top-level
    /// fields, but `timestamp` / `reason` / `process` / `pipeline` /
    /// `event` keep their current shape so existing
    /// `jq | inject` recipes survive.
    pub fn to_jsonl(&self) -> String {
        let mut event_json = self.event.to_json_value();
        if let serde_json::Value::Object(ref mut map) = event_json {
            map.remove("egress");
            map.remove("workspace");
        }
        let record = serde_json::json!({
            "timestamp": self.timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            "reason": self.reason,
            "process": self.process,
            "pipeline": self.pipeline,
            "event": event_json,
        });
        record.to_string()
    }
}

/// Result of running an event through a pipeline.
pub struct PipelineRunResult {
    pub trace: Vec<TraceEntry>,
    pub outputs: Vec<(String, SinkInput)>,
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
    /// Populated iff `termination == Errored`. The runtime layer writes
    /// this to the configured dead-letter queue (`error_log`) or, if
    /// none is configured, emits a structured `tracing::error!` line
    /// with the same payload.
    pub errored: Option<ErroredEventContext>,
}

/// A process registry backed by compiled DSL process definitions.
///
/// Only user-defined `def process { ... }` blocks resolve here.
/// Built-in processes were removed in v0.3.0 Block 4 — former native
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
        _args: &[Value<'bump>],
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
/// `output_sinks` maps output names to their concrete `Output` instances
/// so the `output` statement can call `render` directly on the per-event
/// arena (no `to_owned()` round-trip on the workspace). Outputs that
/// aren't in the map (or that map to a disk queue) fall back to the
/// owned-event path.
pub fn run_pipeline(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    output_sinks: &HashMap<String, Arc<dyn Output>>,
    bump: &mut bumpalo::Bump,
) -> Result<PipelineRunResult> {
    let registry = DslProcessRegistry::new(&config.processes, funcs, tap);
    let mut trace_entries = Vec::new();
    let mut outputs = Vec::new();

    // Log initial state — formatted from `event` while it's still in
    // owned form, before we view it into the arena.
    trace_entries.push(TraceEntry {
        stage: "input".into(),
        label: String::new(),
        detail: format!("ingress: {}", String::from_utf8_lossy(&event.ingress)),
    });

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

    let mut errored: Option<ErroredEventContext> = None;
    let exec_ctx = PipelineExecCtx {
        pipeline_name: &pipeline.name,
        registry: &registry,
        funcs,
        arena: &arena,
        outputs_queue_kind: &config.outputs_queue_kind,
        output_sinks,
    };
    let mut exec_out = PipelineExecOut {
        trace: &mut trace_entries,
        outputs: &mut outputs,
        errored: &mut errored,
    };
    let (_, termination) = exec_pipeline_body(&pipeline.body, bevent, &exec_ctx, &mut exec_out)?;

    let had_outputs = !outputs.is_empty();
    Ok(PipelineRunResult {
        trace: trace_entries,
        outputs,
        had_outputs,
        termination,
        errored,
    })
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
struct PipelineExecCtx<'a, 'bump: 'a> {
    pipeline_name: &'a str,
    registry: &'a DslProcessRegistry<'a>,
    funcs: &'a FunctionRegistry,
    arena: &'bump EventArena<'bump>,
    /// Queue kind per output (Memory → render hot path,
    /// Disk → owned-event persist path).
    outputs_queue_kind: &'a HashMap<String, QueueKind>,
    /// Concrete `Output` instances looked up at the `output` statement
    /// to render a sink-specific payload from the borrowed event.
    output_sinks: &'a HashMap<String, Arc<dyn Output>>,
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
    trace: &'a mut Vec<TraceEntry>,
    outputs: &'a mut Vec<(String, SinkInput)>,
    errored: &'a mut Option<ErroredEventContext>,
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
            out.trace.push(TraceEntry {
                stage: "error".into(),
                label: msg.clone(),
                detail: "event → error_log".into(),
            });
            // Cross to owned form for the DLQ context (which must
            // outlive the per-event arena).
            let owned = event.to_owned();
            *out.errored = Some(ErroredEventContext {
                timestamp: chrono::Utc::now(),
                pipeline: ctx.pipeline_name.to_string(),
                process: "(pipeline)".to_string(),
                reason: msg,
                event: owned,
            });
            Ok((None, PipelineTermination::Errored))
        }

        PipelineStatement::ProcessChain(chain) => {
            let mut current = event;
            for element in chain {
                match element {
                    ProcessChainElement::Named(name, args) => {
                        let mut evaluated_args = bumpalo::collections::Vec::with_capacity_in(
                            args.len(),
                            ctx.arena.bump(),
                        );
                        for a in args {
                            evaluated_args.push(eval_expr(a, &current, ctx.funcs, ctx.arena)?);
                        }
                        let arg_repr: Vec<String> = evaluated_args
                            .iter()
                            .map(|v| v.to_string())
                            .collect();

                        // Snapshot the heap-owned form before the
                        // registry consumes the borrowed event — the
                        // Err arm needs a stable, arena-independent
                        // event for the DLQ context.
                        let backup_owned = current.to_owned();
                        match ctx
                            .registry
                            .call(name, &evaluated_args, current, ctx.arena)
                        {
                            Ok(Some(e)) => {
                                out.trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: if args.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{}({})", name, arg_repr.join(", "))
                                    },
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(None) => {
                                out.trace.push(TraceEntry {
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
                                out.trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: format!("error: {} (event → error_log)", e),
                                });
                                *out.errored = Some(ErroredEventContext {
                                    timestamp: chrono::Utc::now(),
                                    pipeline: ctx.pipeline_name.to_string(),
                                    process: name.clone(),
                                    reason: e.to_string(),
                                    event: backup_owned,
                                });
                                return Ok((None, PipelineTermination::Errored));
                            }
                        }
                    }
                    ProcessChainElement::Inline(body) => {
                        let backup_owned = current.to_owned();
                        match exec_process_body(body, current, ctx.registry, ctx.funcs, ctx.arena) {
                            Ok(ExecResult::Continue(e)) => {
                                out.trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(ExecResult::Dropped) => {
                                out.trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                            Err(e) => {
                                tracing::warn!("inline process: {} — event routed to error_log", e);
                                out.trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: format!("error: {} (event → error_log)", e),
                                });
                                *out.errored = Some(ErroredEventContext {
                                    timestamp: chrono::Utc::now(),
                                    pipeline: ctx.pipeline_name.to_string(),
                                    process: "(inline)".to_string(),
                                    reason: e.to_string(),
                                    event: backup_owned,
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
            out.trace.push(TraceEntry {
                stage: "output".into(),
                label: format!("→ {}", name),
                detail: String::new(),
            });
            // Pick render hot-path or owned persist-path based on the
            // output's queue type. Memory queues take a sink-specific
            // `RenderedPayload` built against the per-event arena;
            // disk-persist queues take an `OwnedEvent` because the
            // payload must outlive the arena and survive a process
            // restart via JSONL serialisation.
            let kind = ctx
                .outputs_queue_kind
                .get(name)
                .copied()
                .unwrap_or(QueueKind::Memory);
            let sink_input = match kind {
                QueueKind::Disk => SinkInput::Owned(event.to_owned()),
                QueueKind::Memory => match ctx.output_sinks.get(name) {
                    Some(sink) => match sink.render(&event, ctx.arena) {
                        Ok(payload) => SinkInput::Rendered(payload),
                        Err(e) => {
                            tracing::warn!(
                                "output '{}': render failed ({}); falling back to owned-event path",
                                name,
                                e
                            );
                            SinkInput::Owned(event.to_owned())
                        }
                    },
                    None => {
                        // No registered sink (e.g. tests, --test-pipeline,
                        // or --check). Fall back to the owned-event form
                        // so callers can still inspect outputs.
                        SinkInput::Owned(event.to_owned())
                    }
                },
            };
            out.outputs.push((name.clone(), sink_input));
            cont(event)
        }

        PipelineStatement::Drop => {
            trace!(target: "limpid::pipeline", "drop");
            out.trace.push(TraceEntry {
                stage: "drop".into(),
                label: String::new(),
                detail: String::new(),
            });
            dropped()
        }

        PipelineStatement::Finish => {
            trace!(target: "limpid::pipeline", "finish");
            out.trace.push(TraceEntry {
                stage: "finish".into(),
                label: String::new(),
                detail: String::new(),
            });
            finished()
        }

        PipelineStatement::If(if_chain) => exec_pipeline_if(if_chain, event, ctx, out),

        PipelineStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr(discriminant, &event, ctx.funcs, ctx.arena)?;
            for arm in arms {
                if arm.pattern.is_none() {
                    return exec_pipeline_branch_body(&arm.body, event, ctx, out);
                }
                let pattern_val = eval_expr(
                    arm.pattern.as_ref().unwrap(),
                    &event,
                    ctx.funcs,
                    ctx.arena,
                )?;
                if values_match(&disc_val, &pattern_val) {
                    return exec_pipeline_branch_body(&arm.body, event, ctx, out);
                }
            }
            cont(event)
        }
    }
}

fn exec_pipeline_if<'bump>(
    if_chain: &IfChain,
    event: BorrowedEvent<'bump>,
    ctx: &PipelineExecCtx<'_, 'bump>,
    out: &mut PipelineExecOut<'_>,
) -> Result<(Option<BorrowedEvent<'bump>>, PipelineTermination)> {
    for (condition, body) in &if_chain.branches {
        let cond_val = eval_expr(condition, &event, ctx.funcs, ctx.arena)?;
        if is_truthy(&cond_val) {
            return exec_pipeline_branch_body(body, event, ctx, out);
        }
    }
    if let Some(else_body) = &if_chain.else_body {
        return exec_pipeline_branch_body(else_body, event, ctx, out);
    }
    Ok((Some(event), PipelineTermination::Finished))
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
        let err = cfg
            .validate(&ModuleRegistry::new())
            .unwrap_err()
            .to_string();
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
        let err = cfg
            .validate(&ModuleRegistry::new())
            .unwrap_err()
            .to_string();
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
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
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
        let sinks: HashMap<String, Arc<dyn Output>> = HashMap::new();
        let result = run_pipeline(pipeline, &event, &cfg, &funcs, None, &sinks, &mut bumpalo::Bump::new()).unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        let ctx = result.errored.expect("errored context must be populated");
        assert_eq!(ctx.pipeline, "p");
        assert_eq!(ctx.process, "wrap");
        assert!(
            ctx.reason.contains("unknown identifier"),
            "unexpected reason: {}",
            ctx.reason
        );
        assert_eq!(&ctx.event.ingress[..], b"original payload");
        assert!(result.outputs.is_empty());
        let line = ctx.to_jsonl();
        assert!(line.contains("\"pipeline\":\"p\""));
        assert!(line.contains("\"process\":\"wrap\""));
        assert!(line.contains("original payload"));
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
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
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
        let sinks: HashMap<String, Arc<dyn Output>> = HashMap::new();
        let result = run_pipeline(pipeline, &event, &cfg, &funcs, None, &sinks, &mut bumpalo::Bump::new()).unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        let ctx = result.errored.expect("errored context must be populated");
        assert_eq!(ctx.pipeline, "p");
        assert_eq!(ctx.process, "refuse");
        assert!(
            ctx.reason.contains("I refuse"),
            "unexpected reason: {}",
            ctx.reason
        );
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
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
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
        let sinks: HashMap<String, Arc<dyn Output>> = HashMap::new();
        let result = run_pipeline(pipeline, &event, &cfg, &funcs, None, &sinks, &mut bumpalo::Bump::new()).unwrap();
        assert_eq!(result.termination, PipelineTermination::Errored);
        let ctx = result.errored.expect("errored context must be populated");
        assert_eq!(ctx.pipeline, "p");
        assert_eq!(ctx.process, "(pipeline)");
        assert!(
            ctx.reason.contains("blocked at pipeline gate"),
            "unexpected reason: {}",
            ctx.reason
        );
        assert!(result.outputs.is_empty());
    }

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
        cfg.validate(&ModuleRegistry::new()).unwrap();
    }
}

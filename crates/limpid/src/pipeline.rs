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

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use tracing::trace;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::*;
use crate::dsl::eval::{
    eval_expr, select_if_branch_with_ordinal, select_switch_arm_with_ordinal, value_to_string,
};
use crate::dsl::exec::{
    CompiledProcessRegistry, ExecResult, ProcessError, ProcessMetricStatement, ProcessRegistry,
    exec_process_body, exec_process_body_with_metric_plan,
};
use crate::event::{BorrowedEvent, OwnedEvent};
use crate::functions::FunctionRegistry;
use crate::tap::TapRegistry;

// ---------------------------------------------------------------------------
// Compiled configuration
// ---------------------------------------------------------------------------

/// A fully resolved configuration ready for execution.
#[derive(Clone)]
pub struct CompiledConfig {
    pub(crate) node_id: Option<String>,
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
            node_id: config.node_id,
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
    pub key: uuid::Uuid,
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
    pub key: uuid::Uuid,
    pub source: std::net::SocketAddr,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub ingress: bytes::Bytes,
    pub egress: bytes::Bytes,
}

impl ProcessEvent {
    /// Snapshot the process-flavor fields from an [`OwnedEvent`].
    pub fn from_owned(ev: &OwnedEvent) -> Self {
        Self {
            key: ev.key,
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
            key: ev.key,
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

/// Pre-compiled metric plan for a pipeline's process invocations.
/// The plan is validated once at startup, so event execution uses
/// only O(1) token lookups. Validation rejects shape or identity
/// mismatches before worker construction; defensive checked access
/// returns an internal error instead of silently attributing an
/// invocation to the wrong metric series.
pub(crate) struct PipelineProcessMetrics {
    nodes: Vec<ProcessMetricNode>,
    statements: Vec<PipelineMetricStatement>,
    #[cfg(test)]
    selection_trap: std::sync::Arc<MetricNodeSelectionTrapState>,
}

struct ProcessMetricNode {
    counters: crate::metrics::ProcessCounters,
    body_plan: Vec<ProcessMetricStatement>,
    #[cfg(test)]
    call_sites: Vec<usize>,
}

pub(crate) struct RawPipelineProcessMetrics {
    nodes: Vec<ProcessMetricNode>,
    identities: Vec<ProcessMetricNodeIdentity>,
    statements: Vec<PipelineMetricStatement>,
    #[cfg(test)]
    selection_trap: std::sync::Arc<MetricNodeSelectionTrapState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessMetricNodeIdentity {
    parent: Option<usize>,
    step: usize,
    kind: ProcessMetricNodeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessMetricNodeKind {
    Named(String),
    Inline,
}

enum PipelineMetricStatement {
    None,
    ProcessChain(Vec<usize>),
    If {
        branches: Vec<Vec<PipelineMetricStatement>>,
        else_body: Option<Vec<PipelineMetricStatement>>,
    },
    Switch(Vec<Vec<PipelineMetricStatement>>),
}

impl PipelineProcessMetrics {
    pub(crate) fn register(
        pipeline: &PipelineDef,
        processes: &HashMap<String, ProcessDef>,
        registry: &crate::metrics::Registry,
    ) -> anyhow::Result<Self> {
        Self::compile_raw(pipeline, processes, registry)?.validate(pipeline, processes)
    }

    pub(crate) fn compile_raw(
        pipeline: &PipelineDef,
        processes: &HashMap<String, ProcessDef>,
        registry: &crate::metrics::Registry,
    ) -> Result<RawPipelineProcessMetrics, crate::metrics::MetricsError> {
        let mut builder = ProcessMetricsBuilder {
            pipeline: &pipeline.name,
            processes,
            registry,
            nodes: Vec::new(),
            identities: Vec::new(),
            children: Vec::new(),
            next_step: 1,
        };
        let statements = builder.pipeline_body(&pipeline.body)?;
        Ok(RawPipelineProcessMetrics {
            nodes: builder.nodes,
            identities: builder.identities,
            statements,
            #[cfg(test)]
            selection_trap: std::sync::Arc::new(MetricNodeSelectionTrapState::default()),
        })
    }

    fn select_node(&self, token: usize) -> Option<&ProcessMetricNode> {
        #[cfg(test)]
        let armed = self
            .selection_trap
            .armed
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        if armed {
            self.selection_trap
                .total_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let node = self.nodes.get(token);
        #[cfg(test)]
        if armed && node.is_none() {
            self.selection_trap
                .invalid_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        node
    }

    #[cfg(test)]
    pub(crate) fn select_node_for_testing(&self, token: usize) -> bool {
        self.select_node(token).is_some()
    }
}

impl RawPipelineProcessMetrics {
    pub(crate) fn validate(
        self,
        pipeline: &PipelineDef,
        processes: &HashMap<String, ProcessDef>,
    ) -> anyhow::Result<PipelineProcessMetrics> {
        let mut validator = ProcessMetricPlanValidator {
            raw: &self,
            processes,
            next_step: 1,
            edges: HashMap::new(),
            validated_nodes: HashSet::new(),
        };
        validator.pipeline_body(&pipeline.body, &self.statements)?;
        if validator.validated_nodes.len() != self.nodes.len() {
            anyhow::bail!("compiled process metric plan contains unreachable nodes");
        }
        Ok(PipelineProcessMetrics {
            nodes: self.nodes,
            statements: self.statements,
            #[cfg(test)]
            selection_trap: self.selection_trap,
        })
    }

    #[cfg(test)]
    pub(crate) fn root_token_for_testing(&self, step: usize) -> Option<usize> {
        self.identities
            .iter()
            .position(|identity| identity.parent.is_none() && identity.step == step)
    }

    #[cfg(test)]
    pub(crate) fn child_token_for_testing(&self, parent: usize, ordinal: usize) -> Option<usize> {
        self.nodes.get(parent)?.call_sites.get(ordinal).copied()
    }

    #[cfg(test)]
    pub(crate) fn metric_node_selection_trap_for_testing(&self) -> MetricNodeSelectionTrap {
        MetricNodeSelectionTrap(std::sync::Arc::clone(&self.selection_trap))
    }

    #[cfg(test)]
    pub(crate) fn remove_root_plan_for_testing(&mut self) {
        self.statements.clear();
    }

    #[cfg(test)]
    pub(crate) fn replace_first_root_plan_with_none_for_testing(&mut self) {
        if let Some(statement) = self.statements.first_mut() {
            *statement = PipelineMetricStatement::None;
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_first_root_token_for_testing(&mut self) {
        for statement in &mut self.statements {
            if let PipelineMetricStatement::ProcessChain(tokens) = statement
                && let Some(token) = tokens.first_mut()
            {
                *token = usize::MAX;
                return;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn swap_first_two_root_tokens_for_testing(&mut self) {
        for statement in &mut self.statements {
            if let PipelineMetricStatement::ProcessChain(tokens) = statement
                && tokens.len() >= 2
            {
                tokens.swap(0, 1);
                return;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_first_nested_token_for_testing(&mut self) {
        for node in &mut self.nodes {
            if let Some(ProcessMetricStatement::Call(token)) = node
                .body_plan
                .iter_mut()
                .find(|statement| matches!(statement, ProcessMetricStatement::Call(_)))
            {
                *token = usize::MAX;
                return;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn swap_first_two_nested_tokens_for_testing(&mut self) {
        let positions: Vec<(usize, usize)> = self
            .nodes
            .iter()
            .enumerate()
            .flat_map(|(node_index, node)| {
                node.body_plan
                    .iter()
                    .enumerate()
                    .filter(|(_, statement)| matches!(statement, ProcessMetricStatement::Call(_)))
                    .map(move |(statement_index, _)| (node_index, statement_index))
            })
            .take(2)
            .collect();
        if let [(left_node, left_statement), (right_node, right_statement)] = positions.as_slice() {
            let left = match self.nodes[*left_node].body_plan[*left_statement] {
                ProcessMetricStatement::Call(token) => token,
                _ => unreachable!(),
            };
            let right = match self.nodes[*right_node].body_plan[*right_statement] {
                ProcessMetricStatement::Call(token) => token,
                _ => unreachable!(),
            };
            self.nodes[*left_node].body_plan[*left_statement] = ProcessMetricStatement::Call(right);
            self.nodes[*right_node].body_plan[*right_statement] =
                ProcessMetricStatement::Call(left);
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_first_inline_token_for_testing(&mut self) {
        for statement in &mut self.statements {
            if let PipelineMetricStatement::ProcessChain(tokens) = statement
                && let Some(token) = tokens.first_mut()
            {
                *token = usize::MAX;
                return;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_first_process_body_plan_with_none_for_testing(&mut self) {
        for node in &mut self.nodes {
            if let Some(statement) = node
                .body_plan
                .iter_mut()
                .find(|statement| matches!(statement, ProcessMetricStatement::Call(_)))
            {
                *statement = ProcessMetricStatement::None;
                return;
            }
        }
    }
}

struct ProcessMetricPlanValidator<'a> {
    raw: &'a RawPipelineProcessMetrics,
    processes: &'a HashMap<String, ProcessDef>,
    next_step: usize,
    edges: HashMap<(usize, String), usize>,
    validated_nodes: HashSet<usize>,
}

impl ProcessMetricPlanValidator<'_> {
    fn pipeline_body(
        &mut self,
        statements: &[PipelineStatement],
        plan: &[PipelineMetricStatement],
    ) -> anyhow::Result<()> {
        if statements.len() != plan.len() {
            anyhow::bail!("compiled pipeline metric plan length mismatch");
        }
        for (statement, metric) in statements.iter().zip(plan) {
            self.pipeline_statement(statement, metric)?;
        }
        Ok(())
    }

    fn pipeline_branch(
        &mut self,
        body: &[BranchBody],
        plan: &[PipelineMetricStatement],
    ) -> anyhow::Result<()> {
        if body.len() != plan.len() {
            anyhow::bail!("compiled pipeline branch metric plan length mismatch");
        }
        for (item, metric) in body.iter().zip(plan) {
            let BranchBody::Pipeline(statement) = item else {
                anyhow::bail!("process statement found in pipeline metric plan");
            };
            self.pipeline_statement(statement, metric)?;
        }
        Ok(())
    }

    fn pipeline_statement(
        &mut self,
        statement: &PipelineStatement,
        metric: &PipelineMetricStatement,
    ) -> anyhow::Result<()> {
        match (statement, metric) {
            (
                PipelineStatement::ProcessChain(chain),
                PipelineMetricStatement::ProcessChain(tokens),
            ) => {
                if chain.len() != tokens.len() {
                    anyhow::bail!("compiled root process token count mismatch");
                }
                for (site, token) in chain.iter().zip(tokens) {
                    let step = self.next_step;
                    self.next_step += 1;
                    let expected_kind = match site {
                        ProcessChainElement::Named(name) => {
                            ProcessMetricNodeKind::Named(name.clone())
                        }
                        ProcessChainElement::Inline(_) => ProcessMetricNodeKind::Inline,
                    };
                    let identity = self.raw.identities.get(*token).ok_or_else(|| {
                        anyhow::anyhow!("compiled root process token is out of range")
                    })?;
                    if identity.parent.is_some()
                        || identity.step != step
                        || identity.kind != expected_kind
                    {
                        anyhow::bail!("compiled root process token identity mismatch");
                    }
                    match site {
                        ProcessChainElement::Named(name) => {
                            let body = self
                                .processes
                                .get(name)
                                .map(|process| process.body.as_slice())
                                .unwrap_or_default();
                            self.process_body(*token, body, &mut vec![(name.clone(), *token)])?;
                        }
                        ProcessChainElement::Inline(body) => {
                            self.process_body(*token, body, &mut Vec::new())?;
                        }
                    }
                }
            }
            (
                PipelineStatement::If(chain),
                PipelineMetricStatement::If {
                    branches,
                    else_body,
                },
            ) => {
                if chain.branches.len() != branches.len()
                    || chain.else_body.is_some() != else_body.is_some()
                {
                    anyhow::bail!("compiled pipeline if metric plan shape mismatch");
                }
                for ((_, body), branch_plan) in chain.branches.iter().zip(branches) {
                    self.pipeline_branch(body, branch_plan)?;
                }
                if let (Some(body), Some(branch_plan)) =
                    (chain.else_body.as_deref(), else_body.as_deref())
                {
                    self.pipeline_branch(body, branch_plan)?;
                }
            }
            (PipelineStatement::Switch(_, arms), PipelineMetricStatement::Switch(metric_arms)) => {
                if arms.len() != metric_arms.len() {
                    anyhow::bail!("compiled pipeline switch metric plan shape mismatch");
                }
                for (arm, arm_plan) in arms.iter().zip(metric_arms) {
                    self.pipeline_branch(&arm.body, arm_plan)?;
                }
            }
            (
                PipelineStatement::Input(_)
                | PipelineStatement::Output(_)
                | PipelineStatement::Drop
                | PipelineStatement::Finish
                | PipelineStatement::Error(_),
                PipelineMetricStatement::None,
            ) => {}
            _ => anyhow::bail!("compiled pipeline metric statement variant mismatch"),
        }
        Ok(())
    }

    fn process_body(
        &mut self,
        parent: usize,
        statements: &[ProcessStatement],
        ancestors: &mut Vec<(String, usize)>,
    ) -> anyhow::Result<()> {
        if !self.validated_nodes.insert(parent) {
            return Ok(());
        }
        let plan = &self
            .raw
            .nodes
            .get(parent)
            .ok_or_else(|| anyhow::anyhow!("compiled process node is out of range"))?
            .body_plan;
        self.process_statements(parent, statements, plan, ancestors)
    }

    fn process_statements(
        &mut self,
        parent: usize,
        statements: &[ProcessStatement],
        plan: &[ProcessMetricStatement],
        ancestors: &mut Vec<(String, usize)>,
    ) -> anyhow::Result<()> {
        if statements.len() != plan.len() {
            anyhow::bail!("compiled process metric plan length mismatch");
        }
        for (statement, metric) in statements.iter().zip(plan) {
            self.process_statement(parent, statement, metric, ancestors)?;
        }
        Ok(())
    }

    fn process_branch(
        &mut self,
        parent: usize,
        body: &[BranchBody],
        plan: &[ProcessMetricStatement],
        ancestors: &mut Vec<(String, usize)>,
    ) -> anyhow::Result<()> {
        if body.len() != plan.len() {
            anyhow::bail!("compiled process branch metric plan length mismatch");
        }
        for (item, metric) in body.iter().zip(plan) {
            let BranchBody::Process(statement) = item else {
                anyhow::bail!("pipeline statement found in process metric plan");
            };
            self.process_statement(parent, statement, metric, ancestors)?;
        }
        Ok(())
    }

    fn process_statement(
        &mut self,
        parent: usize,
        statement: &ProcessStatement,
        metric: &ProcessMetricStatement,
        ancestors: &mut Vec<(String, usize)>,
    ) -> anyhow::Result<()> {
        match (statement, metric) {
            (ProcessStatement::ProcessCall(name), ProcessMetricStatement::Call(token)) => {
                if ancestors.iter().any(|(ancestor, _)| ancestor == name) {
                    anyhow::bail!("compiled process metric plan contains a recursive process call");
                }
                let expected = if let Some(existing) = self.edges.get(&(parent, name.clone())) {
                    *existing
                } else {
                    let identity = self.raw.identities.get(*token).ok_or_else(|| {
                        anyhow::anyhow!("compiled nested process token is out of range")
                    })?;
                    let parent_identity = self.raw.identities.get(parent).ok_or_else(|| {
                        anyhow::anyhow!("compiled parent process token is out of range")
                    })?;
                    if identity.parent != Some(parent)
                        || identity.step != parent_identity.step
                        || identity.kind != ProcessMetricNodeKind::Named(name.clone())
                    {
                        anyhow::bail!("compiled nested process token identity mismatch");
                    }
                    self.edges.insert((parent, name.clone()), *token);
                    *token
                };
                if *token != expected {
                    anyhow::bail!("compiled process call-site token identity mismatch");
                }
                if !self.validated_nodes.contains(token) {
                    let body = self
                        .processes
                        .get(name)
                        .map(|process| process.body.as_slice())
                        .unwrap_or_default();
                    ancestors.push((name.clone(), *token));
                    self.process_body(*token, body, ancestors)?;
                    ancestors.pop();
                }
            }
            (
                ProcessStatement::If(chain),
                ProcessMetricStatement::If {
                    branches,
                    else_body,
                },
            ) => {
                if chain.branches.len() != branches.len()
                    || chain.else_body.is_some() != else_body.is_some()
                {
                    anyhow::bail!("compiled process if metric plan shape mismatch");
                }
                for ((_, body), branch_plan) in chain.branches.iter().zip(branches) {
                    self.process_branch(parent, body, branch_plan, ancestors)?;
                }
                if let (Some(body), Some(branch_plan)) =
                    (chain.else_body.as_deref(), else_body.as_deref())
                {
                    self.process_branch(parent, body, branch_plan, ancestors)?;
                }
            }
            (ProcessStatement::Switch(_, arms), ProcessMetricStatement::Switch(metric_arms)) => {
                if arms.len() != metric_arms.len() {
                    anyhow::bail!("compiled process switch metric plan shape mismatch");
                }
                for (arm, arm_plan) in arms.iter().zip(metric_arms) {
                    self.process_branch(parent, &arm.body, arm_plan, ancestors)?;
                }
            }
            (
                ProcessStatement::TryCatch(try_body, catch_body),
                ProcessMetricStatement::TryCatch {
                    try_body: try_plan,
                    catch_body: catch_plan,
                },
            ) => {
                self.process_statements(parent, try_body, try_plan, ancestors)?;
                self.process_statements(parent, catch_body, catch_plan, ancestors)?;
            }
            (
                ProcessStatement::Assign(_, _)
                | ProcessStatement::LetBinding(_, _)
                | ProcessStatement::Drop
                | ProcessStatement::Error(_)
                | ProcessStatement::ExprStmt(_),
                ProcessMetricStatement::None,
            ) => {}
            _ => anyhow::bail!("compiled process metric statement variant mismatch"),
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default)]
struct MetricNodeSelectionTrapState {
    armed: std::sync::atomic::AtomicBool,
    total_attempts: std::sync::atomic::AtomicUsize,
    invalid_attempts: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
pub(crate) struct MetricNodeSelectionTrap(std::sync::Arc<MetricNodeSelectionTrapState>);

#[cfg(test)]
impl MetricNodeSelectionTrap {
    pub(crate) fn arm_for_testing(&self) {
        self.0
            .armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn total_token_selections_for_testing(&self) -> usize {
        self.0
            .total_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn invalid_token_selections_for_testing(&self) -> usize {
        self.0
            .invalid_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

struct ProcessMetricsBuilder<'a> {
    pipeline: &'a str,
    processes: &'a HashMap<String, ProcessDef>,
    registry: &'a crate::metrics::Registry,
    nodes: Vec<ProcessMetricNode>,
    identities: Vec<ProcessMetricNodeIdentity>,
    children: Vec<HashMap<String, usize>>,
    next_step: usize,
}

impl ProcessMetricsBuilder<'_> {
    fn pipeline_body(
        &mut self,
        body: &[PipelineStatement],
    ) -> Result<Vec<PipelineMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|statement| self.pipeline_statement(statement))
            .collect()
    }

    fn pipeline_branch(
        &mut self,
        body: &[BranchBody],
    ) -> Result<Vec<PipelineMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|item| match item {
                BranchBody::Pipeline(statement) => self.pipeline_statement(statement),
                BranchBody::Process(_) => Ok(PipelineMetricStatement::None),
            })
            .collect()
    }

    fn pipeline_statement(
        &mut self,
        statement: &PipelineStatement,
    ) -> Result<PipelineMetricStatement, crate::metrics::MetricsError> {
        match statement {
            PipelineStatement::ProcessChain(chain) => {
                let mut nodes = Vec::with_capacity(chain.len());
                for element in chain {
                    let step = self.next_step;
                    self.next_step += 1;
                    let (name, kind, body) = match element {
                        ProcessChainElement::Named(name) => (
                            name.as_str(),
                            ProcessMetricNodeKind::Named(name.clone()),
                            self.processes
                                .get(name)
                                .map(|process| process.body.as_slice()),
                        ),
                        ProcessChainElement::Inline(body) => (
                            "(inline)",
                            ProcessMetricNodeKind::Inline,
                            Some(body.as_slice()),
                        ),
                    };
                    nodes.push(self.add_node(
                        ProcessMetricNodeIdentity {
                            parent: None,
                            step,
                            kind,
                        },
                        name,
                        format!("/{name}"),
                        body,
                        &mut Vec::new(),
                    )?);
                }
                Ok(PipelineMetricStatement::ProcessChain(nodes))
            }
            PipelineStatement::If(chain) => {
                let branches = chain
                    .branches
                    .iter()
                    .map(|(_, body)| self.pipeline_branch(body))
                    .collect::<Result<Vec<_>, _>>()?;
                let else_body = chain
                    .else_body
                    .as_deref()
                    .map(|body| self.pipeline_branch(body))
                    .transpose()?;
                Ok(PipelineMetricStatement::If {
                    branches,
                    else_body,
                })
            }
            PipelineStatement::Switch(_, arms) => Ok(PipelineMetricStatement::Switch(
                arms.iter()
                    .map(|arm| self.pipeline_branch(&arm.body))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            _ => Ok(PipelineMetricStatement::None),
        }
    }

    fn add_node(
        &mut self,
        identity: ProcessMetricNodeIdentity,
        name: &str,
        path: String,
        body: Option<&[ProcessStatement]>,
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<usize, crate::metrics::MetricsError> {
        let id = self.nodes.len();
        let step = identity.step;
        self.nodes.push(ProcessMetricNode {
            counters: crate::metrics::ProcessCounters::register(
                self.registry,
                self.pipeline,
                step,
                &path,
                name,
            )?,
            body_plan: Vec::new(),
            #[cfg(test)]
            call_sites: Vec::new(),
        });
        self.identities.push(identity);
        self.children.push(HashMap::new());
        ancestors.push((name.to_owned(), id));
        if let Some(body) = body {
            self.nodes[id].body_plan = self.process_body(id, &path, step, body, ancestors)?;
        }
        ancestors.pop();
        Ok(id)
    }

    fn process_body(
        &mut self,
        parent: usize,
        parent_path: &str,
        step: usize,
        body: &[ProcessStatement],
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<Vec<ProcessMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|statement| {
                self.process_statement(parent, parent_path, step, statement, ancestors)
            })
            .collect()
    }

    fn process_statement(
        &mut self,
        parent: usize,
        parent_path: &str,
        step: usize,
        statement: &ProcessStatement,
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<ProcessMetricStatement, crate::metrics::MetricsError> {
        match statement {
            ProcessStatement::ProcessCall(name) => {
                let child = if let Some(child) = self.children[parent].get(name) {
                    *child
                } else if let Some((position, _)) = ancestors
                    .iter()
                    .enumerate()
                    .find(|(_, (ancestor, _))| ancestor == name)
                {
                    let mut path: Vec<String> = ancestors[position..]
                        .iter()
                        .map(|(ancestor, _)| ancestor.clone())
                        .collect();
                    path.push(name.clone());
                    return Err(crate::metrics::MetricsError::ProcessCallCycle { path });
                } else {
                    let body = self
                        .processes
                        .get(name)
                        .map(|process| process.body.as_slice());
                    let child = self.add_node(
                        ProcessMetricNodeIdentity {
                            parent: Some(parent),
                            step,
                            kind: ProcessMetricNodeKind::Named(name.clone()),
                        },
                        name,
                        format!("{parent_path}/{name}"),
                        body,
                        ancestors,
                    )?;
                    self.children[parent].insert(name.clone(), child);
                    child
                };
                #[cfg(test)]
                self.nodes[parent].call_sites.push(child);
                Ok(ProcessMetricStatement::Call(child))
            }
            ProcessStatement::If(chain) => {
                let branches = chain
                    .branches
                    .iter()
                    .map(|(_, body)| {
                        self.process_branch(parent, parent_path, step, body, ancestors)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let else_body = chain
                    .else_body
                    .as_deref()
                    .map(|body| self.process_branch(parent, parent_path, step, body, ancestors))
                    .transpose()?;
                Ok(ProcessMetricStatement::If {
                    branches,
                    else_body,
                })
            }
            ProcessStatement::Switch(_, arms) => Ok(ProcessMetricStatement::Switch(
                arms.iter()
                    .map(|arm| self.process_branch(parent, parent_path, step, &arm.body, ancestors))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            ProcessStatement::TryCatch(try_body, catch_body) => {
                Ok(ProcessMetricStatement::TryCatch {
                    try_body: self.process_body(parent, parent_path, step, try_body, ancestors)?,
                    catch_body: self.process_body(
                        parent,
                        parent_path,
                        step,
                        catch_body,
                        ancestors,
                    )?,
                })
            }
            _ => Ok(ProcessMetricStatement::None),
        }
    }

    fn process_branch(
        &mut self,
        parent: usize,
        parent_path: &str,
        step: usize,
        body: &[BranchBody],
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<Vec<ProcessMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|item| match item {
                BranchBody::Process(statement) => {
                    self.process_statement(parent, parent_path, step, statement, ancestors)
                }
                BranchBody::Pipeline(_) => Ok(ProcessMetricStatement::None),
            })
            .collect()
    }
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
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pipeline_with_process_metrics(
    pipeline: &PipelineDef,
    event: &OwnedEvent,
    config: &CompiledConfig,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
    trace: Option<&mut Vec<TraceEntry>>,
    output_capture: OutputCapturePolicy<'_>,
    bump: &mut bumpalo::Bump,
    process_metrics: &PipelineProcessMetrics,
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
        assert!(line.contains("\"schema_version\":3"));
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

    fn compile_packaged_otlp_composers() -> Result<CompiledConfig> {
        use std::fs;
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut src = String::new();
        for relative in [
            "packaging/snippets/functions/timestamp_converter.limpid",
            "packaging/snippets/functions/severity_converter.limpid",
            "packaging/snippets/functions/proto_num.limpid",
            "packaging/snippets/functions/http_method_activity_id.limpid",
            "packaging/snippets/functions/parse_datetime_rfc3164.limpid",
            "packaging/snippets/parsers/parse_asa.limpid",
            "packaging/snippets/parsers/parse_auditd.limpid",
            "packaging/snippets/parsers/parse_aws_guardduty.limpid",
            "packaging/snippets/parsers/parse_aws_vpc_flow.limpid",
            "packaging/snippets/parsers/parse_azure_activity.limpid",
            "packaging/snippets/parsers/parse_bind.limpid",
            "packaging/snippets/parsers/parse_checkpoint_leef.limpid",
            "packaging/snippets/parsers/parse_cef.limpid",
            "packaging/snippets/parsers/parse_checkpoint_syslog.limpid",
            "packaging/snippets/parsers/parse_cloudtrail.limpid",
            "packaging/snippets/parsers/parse_combined_log.limpid",
            "packaging/snippets/parsers/parse_fortigate_cef.limpid",
            "packaging/snippets/parsers/parse_fortigate_syslog.limpid",
            "packaging/snippets/parsers/parse_juniper_srx_sd_syslog.limpid",
            "packaging/snippets/parsers/parse_juniper_srx_syslog.limpid",
            "packaging/snippets/parsers/parse_journald.limpid",
            "packaging/snippets/parsers/parse_k8s_audit.limpid",
            "packaging/snippets/parsers/parse_nsp.limpid",
            "packaging/snippets/parsers/parse_okta_system.limpid",
            "packaging/snippets/parsers/parse_openssh.limpid",
            "packaging/snippets/parsers/parse_paloalto_cef.limpid",
            "packaging/snippets/parsers/parse_paloalto_syslog.limpid",
            "packaging/snippets/parsers/parse_postfix.limpid",
            "packaging/snippets/parsers/parse_sudo.limpid",
            "packaging/snippets/parsers/parse_suricata.limpid",
            "packaging/snippets/parsers/parse_syslog.limpid",
            "packaging/snippets/parsers/parse_sysmon.limpid",
            "packaging/snippets/parsers/parse_winevent_json.limpid",
            "packaging/snippets/parsers/parse_zeek_default.limpid",
            "packaging/snippets/parsers/parse_zeek_soc.limpid",
            "packaging/snippets/parsers/parse_zeek_full.limpid",
            "packaging/snippets/composers/compose_ocsf.limpid",
            "packaging/snippets/composers/compose_otlp.limpid",
        ] {
            src.push_str(&fs::read_to_string(root.join(relative))?);
            src.push('\n');
        }
        src.push_str(
            r#"
def input i { type syslog_tcp bind "127.0.0.1:5514" }
def output o { type stdout }

def pipeline direct_otlp {
    input i
    process compose_otlp | otlp_to_egress
    output o
}

def pipeline ocsf_in_otlp {
    input i
    process compose_ocsf
          | {
              workspace.lsis.shed.otlp.log_record.body =
                  { string_value: workspace.lsis.composed.ocsf }
            }
          | compose_otlp
          | otlp_to_egress
    output o
}

def pipeline fortigate_native_otlp {
    input i
    process parse_fortigate_syslog
          | fortigate_syslog_to_otlp
          | compose_otlp
          | otlp_to_egress
    output o
}

def pipeline guardduty_otlp {
    input i
    process parse_aws_guardduty | aws_guardduty_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline vpc_flow_otlp {
    input i
    process parse_aws_vpc_flow | aws_vpc_flow_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline azure_activity_otlp {
    input i
    process parse_azure_activity | azure_activity_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline cloudtrail_otlp {
    input i
    process parse_cloudtrail | cloudtrail_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline k8s_audit_otlp {
    input i
    process parse_k8s_audit | k8s_audit_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline okta_system_otlp {
    input i
    process parse_okta_system | okta_system_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline suricata_otlp {
    input i
    process parse_suricata | suricata_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline asa_otlp {
    input i
    process parse_syslog | parse_asa | asa_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline cef_otlp {
    input i
    process parse_syslog | parse_cef | cef_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline checkpoint_leef_otlp {
    input i
    process parse_checkpoint_leef | checkpoint_leef_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline checkpoint_syslog_otlp {
    input i
    process parse_checkpoint_syslog | checkpoint_syslog_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline fortigate_cef_otlp {
    input i
    process parse_syslog | parse_cef | parse_fortigate_cef | fortigate_cef_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline juniper_legacy_otlp {
    input i
    process parse_juniper_srx_syslog | juniper_srx_syslog_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline juniper_structured_otlp {
    input i
    process parse_juniper_srx_sd_syslog | juniper_srx_sd_syslog_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline nsp_otlp {
    input i
    process parse_nsp | nsp_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline paloalto_cef_otlp {
    input i
    process parse_syslog | parse_cef | parse_paloalto_cef | paloalto_cef_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline paloalto_native_otlp {
    input i
    process parse_syslog | parse_paloalto_syslog | paloalto_syslog_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline auditd_otlp {
    input i
    process parse_auditd | auditd_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline bind_otlp {
    input i
    process parse_bind | bind_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline combined_log_otlp {
    input i
    process parse_combined_log | combined_log_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline journald_otlp {
    input i
    process parse_journald | journald_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline openssh_otlp {
    input i
    process parse_openssh | openssh_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline postfix_otlp {
    input i
    process parse_postfix | postfix_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline sudo_otlp {
    input i
    process parse_sudo | sudo_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline syslog_otlp {
    input i
    process parse_syslog | syslog_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline sysmon_otlp {
    input i
    process parse_sysmon | sysmon_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline winevent_otlp {
    input i
    process parse_winevent_json | winevent_json_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline zeek_default_otlp {
    input i
    process parse_zeek_default | zeek_default_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline zeek_soc_otlp {
    input i
    process parse_zeek_soc | zeek_soc_to_otlp | compose_otlp | otlp_to_egress
    output o
}

def pipeline zeek_full_otlp {
    input i
    process parse_zeek_full | zeek_full_to_otlp | compose_otlp | otlp_to_egress
    output o
}
"#,
        );
        compile(&src)
    }

    fn run_packaged_parser_pipeline(
        parsers: &[&str],
        setup: Option<&str>,
        process: &str,
        ingress: &[u8],
    ) -> PipelineRunResult {
        use crate::event::OwnedEvent;
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use bytes::Bytes;
        use std::io::Write;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let snippets = root.join("packaging/snippets");
        let mut config_file = tempfile::Builder::new()
            .prefix(".limpid-parser-test-")
            .tempfile_in(&snippets)
            .expect("temporary packaged parser config");
        for parser in parsers {
            writeln!(config_file, "include \"parsers/{parser}.limpid\"")
                .expect("write parser include");
        }
        writeln!(
            config_file,
            "def input i {{ type syslog_tcp bind \"127.0.0.1:5514\" }}\n\
             def output o {{ type stdout }}\n\
             def pipeline p {{\n\
                 input i"
        )
        .expect("write parser pipeline header");
        if let Some(setup) = setup {
            writeln!(config_file, "    process {{ {setup} }}").expect("write parser setup");
        }
        writeln!(
            config_file,
            "    process {process}\n\
                 process {{ egress = to_json(workspace.lsis.parsed) }}\n\
                 output o\n\
             }}"
        )
        .expect("write parser pipeline body");
        config_file.flush().expect("flush parser config");

        let (config, source_map) = crate::config::load_config_with_source_map(config_file.path())
            .expect("load packaged parser with include closure");
        let cfg = CompiledConfig::from_config(config).expect("compile packaged parser");
        cfg.validate().expect("validate packaged parser pipeline");
        let diagnostics = crate::check::analyze(&cfg, &source_map);
        assert!(
            diagnostics.is_empty(),
            "packaged parser analyzer diagnostics: {diagnostics:#?}"
        );
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);

        let event = OwnedEvent::new(
            Bytes::copy_from_slice(ingress),
            "127.0.0.1:0".parse().expect("valid test source"),
        );
        run_pipeline(
            cfg.pipelines.get("p").expect("test pipeline"),
            &event,
            &cfg,
            &funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .expect("packaged parser pipeline must run")
    }

    fn run_packaged_parser_json(
        parsers: &[&str],
        setup: Option<&str>,
        process: &str,
        ingress: &[u8],
    ) -> serde_json::Value {
        let result = run_packaged_parser_pipeline(parsers, setup, process, ingress);
        assert_eq!(
            result.termination,
            PipelineTermination::Finished,
            "pipeline errors: {:?}",
            result.errored
        );
        assert_eq!(result.outputs.len(), 1);
        serde_json::from_slice(&result.outputs[0].1.egress).expect("parser egress must be JSON")
    }

    fn paloalto_traffic_wire(generate_time: &str) -> Vec<u8> {
        let mut fields = vec![""; 53];
        fields[0] = "1";
        fields[1] = generate_time;
        fields[2] = "012345678";
        fields[3] = "TRAFFIC";
        fields[4] = "end";
        fields[6] = generate_time;
        fields[7] = "192.0.2.10";
        fields[8] = "198.51.100.5";
        fields[14] = "ssl";
        fields[24] = "54321";
        fields[25] = "443";
        fields[29] = "tcp";
        fields[30] = "allow";
        fields[31] = "1536";
        fields[32] = "1024";
        fields[33] = "512";
        fields[34] = "3";
        fields[35] = generate_time;
        fields[36] = "60";
        fields[44] = "2";
        fields[45] = "1";
        fields[46] = "aged-out";
        fields[51] = "vsys1";
        fields[52] = "fw-pan01";
        format!("<134>Apr 30 01:23:45 fw-pan01 {}", fields.join(",")).into_bytes()
    }

    #[test]
    fn juniper_rfc3164_space_padded_single_digit_day_preserves_source_time() {
        use chrono::{Datelike, Duration, TimeZone, Utc};

        let now = Utc::now();
        let this_year = Utc
            .with_ymd_and_hms(now.year(), 8, 9, 2, 38, 3)
            .single()
            .expect("valid RFC 3164 fixture time");
        let expected = if this_year > now + Duration::days(1) {
            Utc.with_ymd_and_hms(now.year() - 1, 8, 9, 2, 38, 3)
                .single()
                .expect("valid previous-year RFC 3164 fixture time")
        } else {
            this_year
        }
        .timestamp_nanos_opt()
        .expect("fixture fits i64");

        let event = run_packaged_parser_json(
            &["parse_juniper_srx_syslog"],
            Some("workspace.juniper_srx_syslog = { body: ingress, timezone: \"UTC\" }"),
            "parse_juniper_srx_syslog",
            b"<14>Aug  9 02:38:03 srx01 RT_IDP: IDP_ATTACK_LOG_EVENT: IDP: at 1778905950, SIG Attack log <198.51.100.10/63074->192.0.2.100/445> for TCP protocol and service SERVICE_IDP application SMB by rule Tap of rulebase IPS in policy Tap. attack: id=19519, repeat=0, action=DROP, threat-severity=HIGH, name=Example, NAT <198.51.100.10:0->0.0.0.0:0>",
        );

        assert_eq!(event["time"], expected);
    }

    #[test]
    fn packaged_parsers_preserve_source_event_time_as_nanoseconds() {
        use chrono::{Datelike, Duration, TimeZone, Timelike, Utc};

        let rfc3164_time = Utc::now() - Duration::days(2);
        let rfc3164_wire = rfc3164_time.format("%b %e %H:%M:%S").to_string();
        let rfc3164_expected = Utc
            .with_ymd_and_hms(
                rfc3164_time.year(),
                rfc3164_time.month(),
                rfc3164_time.day(),
                rfc3164_time.hour(),
                rfc3164_time.minute(),
                rfc3164_time.second(),
            )
            .single()
            .expect("valid RFC 3164 fixture time")
            .timestamp_nanos_opt()
            .expect("fixture fits i64");

        let asa = run_packaged_parser_json(
            &["parse_syslog", "parse_asa"],
            Some("workspace.asa = { timezone: \"UTC\" }"),
            "parse_syslog | parse_asa",
            format!(
                "<165>{rfc3164_wire} fw-asa01 : %ASA-6-605005: Login permitted from 192.0.2.10/54321 to outside:198.51.100.5/SSH for user admin"
            )
            .as_bytes(),
        );
        assert_eq!(asa["time"], rfc3164_expected);

        let auditd = run_packaged_parser_json(
            &["parse_auditd"],
            Some("workspace.auditd = { body: ingress }"),
            "parse_auditd",
            b"type=USER_LOGIN msg=audit(1710000000.123:1): pid=42 uid=0 auid=1000 ses=1 res=success acct=alice addr=192.0.2.10 terminal=pts/0 exe=/usr/bin/login",
        );
        assert_eq!(auditd["time"], 1_710_000_000_123_000_000_i64);

        let bind = run_packaged_parser_json(
            &["parse_bind"],
            Some(
                "workspace.bind = { body: ingress, hostname: \"dns01\", timezone: \"Asia/Tokyo\" }",
            ),
            "parse_bind",
            b"30-Apr-2026 10:23:45.123 client @0x1 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)",
        );
        assert_eq!(bind["time"], 1_777_512_225_123_000_000_i64);

        let checkpoint_leef = run_packaged_parser_json(
            &["parse_checkpoint_leef"],
            Some("workspace.checkpoint_leef = { body: ingress }"),
            "parse_checkpoint_leef",
            b"<14>1 2026-04-30T01:23:45.123456789Z cpgw01 CheckPoint - - LEEF:2.0|Check Point|VPN-1 & FireWall-1|R81|Accept|src=192.0.2.10\tdst=198.51.100.5\tsrcPort=51234\tdstPort=443\tproto=tcp\taction=Accept",
        );
        assert_eq!(checkpoint_leef["time"], 1_777_512_225_123_456_789_i64);

        let checkpoint_syslog = run_packaged_parser_json(
            &["parse_checkpoint_syslog"],
            Some("workspace.checkpoint_syslog = { body: ingress }"),
            "parse_checkpoint_syslog",
            b"<14>1 2026-04-30T01:23:45.123456789Z cpgw01 CheckPoint - - [action:\"Accept\"; src:\"192.0.2.10\"; dst:\"198.51.100.5\"; service:\"443\"; proto:\"tcp\"; product:\"VPN-1 & FireWall-1\"; severity:\"Low\"]",
        );
        assert_eq!(checkpoint_syslog["time"], 1_777_512_225_123_456_789_i64);

        let fortigate_cef = run_packaged_parser_json(
            &["parse_syslog", "parse_cef", "parse_fortigate_cef"],
            Some("workspace.fortigate_cef = { timezone: \"UTC\" }"),
            "parse_syslog | parse_cef | parse_fortigate_cef",
            format!(
                "<129>{rfc3164_wire} fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|cat=utm:ips src=192.0.2.10 dst=198.51.100.5 proto=6 act=detected FTNTFGTattack=test FTNTFGTattackid=42"
            )
            .as_bytes(),
        );
        assert_eq!(fortigate_cef["time"], rfc3164_expected);

        let fortigate_native = run_packaged_parser_json(
            &["parse_fortigate_syslog"],
            None,
            "parse_fortigate_syslog",
            b"<134>eventtime=1777284000123456789 level=notice type=traffic subtype=forward srcip=192.0.2.10 dstip=198.51.100.5 proto=6 action=accept",
        );
        assert_eq!(fortigate_native["time"], 1_777_284_000_123_456_789_i64);

        let juniper_sd = run_packaged_parser_json(
            &["parse_juniper_srx_sd_syslog"],
            Some("workspace.juniper_srx_sd_syslog = { body: ingress }"),
            "parse_juniper_srx_sd_syslog",
            b"<14>1 2026-04-30T01:23:45.123456789Z srx01 RT_FLOW - RT_FLOW_SESSION_CREATE [junos@2636 source-address=\"192.0.2.10\" source-port=\"54321\" destination-address=\"198.51.100.5\" destination-port=\"443\" protocol-id=\"6\" policy-name=\"allow-web\"]",
        );
        assert_eq!(juniper_sd["time"], 1_777_512_225_123_456_789_i64);

        let juniper_legacy = run_packaged_parser_json(
            &["parse_juniper_srx_syslog"],
            Some(
                "workspace.juniper_srx_syslog = { body: ingress, timezone: \"UTC\" }",
            ),
            "parse_juniper_srx_syslog",
            format!(
                "<14>{rfc3164_wire} srx01 RT_IDP: IDP_ATTACK_LOG_EVENT: IDP: at 1778905950, SIG Attack log <198.51.100.10/63074->192.0.2.100/445> for TCP protocol and service SERVICE_IDP application SMB by rule Tap of rulebase IPS in policy Tap. attack: id=19519, repeat=0, action=DROP, threat-severity=HIGH, name=Example, NAT <198.51.100.10:0->0.0.0.0:0>"
            )
            .as_bytes(),
        );
        assert_eq!(juniper_legacy["time"], rfc3164_expected);

        let nsp = run_packaged_parser_json(
            &["parse_nsp"],
            Some(
                "workspace.nsp = { body: ingress, hostname: \"nsp01\", timezone: \"+09:00\" }",
            ),
            "parse_nsp",
            b"admin_domain=Default alert_id=12345 alert_type=Signature app_protocol=HTTP confidence=Tentative attack_count=1 attack_id=42 attack_name=Example severity=High alert_signature=SIG attack_time=2026-05-16 10:00:00 category=Exploit dest_ip=192.0.2.10 dest_name=web01 dest_port=80 device_name=nsp01 direction=Inbound confidence= file_name= file_hash= file_type= virus_name= action_status= error_status= protocol=TCP result=Blocked src_ip=198.51.100.5 src_name= src_port=54321",
        );
        let nsp_expected = Utc
            .with_ymd_and_hms(2026, 5, 16, 1, 0, 0)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(nsp["time"], nsp_expected);

        let paloalto_cef = run_packaged_parser_json(
            &["parse_syslog", "parse_cef", "parse_paloalto_cef"],
            Some("workspace.paloalto_cef = { timezone: \"UTC\" }"),
            "parse_syslog | parse_cef | parse_paloalto_cef",
            format!(
                "<134>{rfc3164_wire} fw-pan01 CEF:0|Palo Alto Networks|PAN-OS|10.2.0|end|TRAFFIC|3|src=192.0.2.10 dst=198.51.100.5 proto=tcp act=allow"
            )
            .as_bytes(),
        );
        assert_eq!(paloalto_cef["time"], rfc3164_expected);

        let paloalto_native_wire = paloalto_traffic_wire("2026/04/30 10:23:45");
        let paloalto_native = run_packaged_parser_json(
            &["parse_syslog", "parse_paloalto_syslog"],
            Some("workspace.paloalto_syslog = { timezone: \"Asia/Tokyo\" }"),
            "parse_syslog | parse_paloalto_syslog",
            &paloalto_native_wire,
        );
        assert_eq!(paloalto_native["time"], 1_777_512_225_000_000_000_i64);

        let sysmon = run_packaged_parser_json(
            &["parse_sysmon"],
            Some("workspace.sysmon = { body: parse_json(ingress) }"),
            "parse_sysmon",
            br#"{"EventID":11,"EventTime":"2026-04-30T01:23:45.123456789Z","Computer":"host01","EventData":{"TargetFilename":"C:\\Temp\\x.txt","User":"alice","Image":"C:\\Windows\\cmd.exe","ProcessId":"42"}}"#,
        );
        assert_eq!(sysmon["time"], 1_777_512_225_123_456_789_i64);

        let vpc = run_packaged_parser_json(
            &["parse_aws_vpc_flow"],
            None,
            "parse_aws_vpc_flow",
            b"2 123456789012 eni-0a1b2c3d4e5f6a7b8 192.0.2.10 198.51.100.5 54321 443 6 10 4000 1714000000 1714000060 ACCEPT OK",
        );
        assert_eq!(vpc["time"], 1_714_000_000_000_000_000_i64);
        assert_eq!(vpc["start_time"], vpc["time"]);
        assert_eq!(vpc["end_time"], 1_714_000_060_000_000_000_i64);
    }

    #[test]
    fn offsetless_source_times_use_vendor_defaults_and_timezone_overrides() {
        use chrono::TimeZone;

        if std::env::var_os("LIMPID_SYSTEM_TZ_TEST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("pipeline::tests::offsetless_source_times_use_vendor_defaults_and_timezone_overrides")
                .env("LIMPID_SYSTEM_TZ_TEST_CHILD", "1")
                .env("TZ", "America/New_York")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let bind_default_timezone = run_packaged_parser_json(
            &["parse_bind"],
            Some("workspace.bind = { body: ingress }"),
            "parse_bind",
            b"30-Apr-2026 10:23:45.123 client @0x1 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)",
        );
        let bind_default_expected = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 4, 30, 10, 23, 45)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
            + 123_000_000;
        assert_eq!(bind_default_timezone["time"], bind_default_expected);

        let bind_override = run_packaged_parser_json(
            &["parse_bind"],
            Some("workspace.bind = { body: ingress, timezone: \"UTC\" }"),
            "parse_bind",
            b"30-Apr-2026 10:23:45.123 client @0x1 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)",
        );
        let bind_override_expected = chrono::Utc
            .with_ymd_and_hms(2026, 4, 30, 10, 23, 45)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
            + 123_000_000;
        assert_eq!(bind_override["time"], bind_override_expected);

        let nsp_body = b"admin_domain=Default alert_id=12345 alert_type=Signature app_protocol=HTTP confidence=Tentative attack_count=1 attack_id=42 attack_name=Example severity=High alert_signature=SIG attack_time=2026-05-16 10:00:00 category=Exploit dest_ip=192.0.2.10 dest_name=web01 dest_port=80 device_name=nsp01 direction=Inbound confidence= file_name= file_hash= file_type= virus_name= action_status= error_status= protocol=TCP result=Blocked src_ip=198.51.100.5 src_name= src_port=54321";
        let nsp_default_timezone = run_packaged_parser_json(
            &["parse_nsp"],
            Some("workspace.nsp = { body: ingress }"),
            "parse_nsp",
            nsp_body,
        );
        let nsp_utc_expected = chrono::Utc
            .with_ymd_and_hms(2026, 5, 16, 10, 0, 0)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        let nsp_local_expected = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 5, 16, 10, 0, 0)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(nsp_default_timezone["time"], nsp_local_expected);

        let nsp_override = run_packaged_parser_json(
            &["parse_nsp"],
            Some("workspace.nsp = { body: ingress, timezone: \"UTC\" }"),
            "parse_nsp",
            nsp_body,
        );
        assert_eq!(nsp_override["time"], nsp_utc_expected);

        let nsp_explicit_utc = run_packaged_parser_json(
            &["parse_nsp"],
            Some("workspace.nsp = { body: ingress }"),
            "parse_nsp",
            b"admin_domain=Default alert_id=12345 alert_type=Signature app_protocol=HTTP confidence=Tentative attack_count=1 attack_id=42 attack_name=Example severity=High alert_signature=SIG attack_time=2026-05-16 10:00:00 UTC category=Exploit dest_ip=192.0.2.10 dest_name=web01 dest_port=80 device_name=nsp01 direction=Inbound confidence= file_name= file_hash= file_type= virus_name= action_status= error_status= protocol=TCP result=Blocked src_ip=198.51.100.5 src_name= src_port=54321",
        );
        assert_eq!(nsp_explicit_utc["time"], nsp_utc_expected);

        let paloalto_wire = paloalto_traffic_wire("2026/04/30 10:23:45");
        let paloalto_default_timezone = run_packaged_parser_json(
            &["parse_syslog", "parse_paloalto_syslog"],
            None,
            "parse_syslog | parse_paloalto_syslog",
            &paloalto_wire,
        );
        let paloalto_local_expected = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 4, 30, 10, 23, 45)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(paloalto_default_timezone["time"], paloalto_local_expected);

        let paloalto_override = run_packaged_parser_json(
            &["parse_syslog", "parse_paloalto_syslog"],
            Some("workspace.paloalto_syslog = { timezone: \"UTC\" }"),
            "parse_syslog | parse_paloalto_syslog",
            &paloalto_wire,
        );
        let paloalto_override_expected = chrono::Utc
            .with_ymd_and_hms(2026, 4, 30, 10, 23, 45)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(paloalto_override["time"], paloalto_override_expected);

        for (parser, setup, process, ingress) in [
            (
                &["parse_bind"][..],
                "workspace.bind = { body: ingress, timezone: \"Not/AZone\" }",
                "parse_bind",
                b"30-Apr-2026 10:23:45.123 client @0x1 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)".as_slice(),
            ),
            (
                &["parse_nsp"][..],
                "workspace.nsp = { body: ingress, timezone: \"Not/AZone\" }",
                "parse_nsp",
                nsp_body.as_slice(),
            ),
            (
                &["parse_syslog", "parse_paloalto_syslog"][..],
                "workspace.paloalto_syslog = { timezone: \"Not/AZone\" }",
                "parse_syslog | parse_paloalto_syslog",
                paloalto_wire.as_slice(),
            ),
            (
                &["parse_bind"][..],
                "workspace.bind = { body: ingress, timezone: \"local\" }",
                "parse_bind",
                b"30-Apr-2026 10:23:45.123 client @0x1 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)".as_slice(),
            ),
            (
                &["parse_nsp"][..],
                "workspace.nsp = { body: ingress, timezone: \"local\" }",
                "parse_nsp",
                nsp_body.as_slice(),
            ),
            (
                &["parse_syslog", "parse_paloalto_syslog"][..],
                "workspace.paloalto_syslog = { timezone: \"local\" }",
                "parse_syslog | parse_paloalto_syslog",
                paloalto_wire.as_slice(),
            ),
        ] {
            let result = run_packaged_parser_pipeline(parser, Some(setup), process, ingress);
            assert_eq!(result.termination, PipelineTermination::Errored);
            assert!(result.outputs.is_empty());
            assert_eq!(result.errored.len(), 1);
            let reason = match &result.errored[0] {
                ErroredEventContext::Process { reason, .. } => reason,
                other => panic!("expected process error, got {other:?}"),
            };
            assert!(reason.contains("timezone"), "error was {reason:?}");
        }
    }

    #[test]
    fn rfc3164_source_times_use_vendor_defaults_and_timezone_overrides() {
        use chrono::{Datelike, Duration, TimeZone, Timelike, Utc};

        if std::env::var_os("LIMPID_SYSTEM_TZ_TEST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("pipeline::tests::rfc3164_source_times_use_vendor_defaults_and_timezone_overrides")
                .env("LIMPID_SYSTEM_TZ_TEST_CHILD", "1")
                .env("TZ", "America/New_York")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let source_time = Utc::now() - Duration::days(2);
        let wire_time = source_time.format("%b %e %H:%M:%S").to_string();
        let expected = Utc
            .with_ymd_and_hms(
                source_time.year(),
                source_time.month(),
                source_time.day(),
                source_time.hour(),
                source_time.minute(),
                source_time.second(),
            )
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        let local_expected = chrono_tz::America::New_York
            .with_ymd_and_hms(
                source_time.year(),
                source_time.month(),
                source_time.day(),
                source_time.hour(),
                source_time.minute(),
                source_time.second(),
            )
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();

        let cases = [
            (
                &["parse_syslog", "parse_asa"][..],
                "parse_syslog | parse_asa",
                "workspace.asa = { timezone: \"UTC\" }",
                "workspace.asa = { timezone: \"Not/AZone\" }",
                "workspace.asa = { timezone: \"local\" }",
                format!(
                    "<165>{wire_time} fw-asa01 : %ASA-6-605005: Login permitted from 192.0.2.10/54321 to outside:198.51.100.5/SSH for user admin"
                )
                .into_bytes(),
            ),
            (
                &["parse_syslog", "parse_cef", "parse_fortigate_cef"][..],
                "parse_syslog | parse_cef | parse_fortigate_cef",
                "workspace.fortigate_cef = { timezone: \"UTC\" }",
                "workspace.fortigate_cef = { timezone: \"Not/AZone\" }",
                "workspace.fortigate_cef = { timezone: \"local\" }",
                format!(
                    "<129>{wire_time} fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|cat=utm:ips src=192.0.2.10 dst=198.51.100.5 proto=6 act=detected FTNTFGTattack=test FTNTFGTattackid=42"
                )
                .into_bytes(),
            ),
            (
                &["parse_juniper_srx_syslog"][..],
                "parse_juniper_srx_syslog",
                "workspace.juniper_srx_syslog = { body: ingress, timezone: \"UTC\" }",
                "workspace.juniper_srx_syslog = { body: ingress, timezone: \"Not/AZone\" }",
                "workspace.juniper_srx_syslog = { body: ingress, timezone: \"local\" }",
                format!(
                    "<14>{wire_time} srx01 RT_IDP: IDP_ATTACK_LOG_EVENT: IDP: at 1778905950, SIG Attack log <198.51.100.10/63074->192.0.2.100/445> for TCP protocol and service SERVICE_IDP application SMB by rule Tap of rulebase IPS in policy Tap. attack: id=19519, repeat=0, action=DROP, threat-severity=HIGH, name=Example, NAT <198.51.100.10:0->0.0.0.0:0>"
                )
                .into_bytes(),
            ),
            (
                &["parse_syslog", "parse_cef", "parse_paloalto_cef"][..],
                "parse_syslog | parse_cef | parse_paloalto_cef",
                "workspace.paloalto_cef = { timezone: \"UTC\" }",
                "workspace.paloalto_cef = { timezone: \"Not/AZone\" }",
                "workspace.paloalto_cef = { timezone: \"local\" }",
                format!(
                    "<134>{wire_time} fw-pan01 CEF:0|Palo Alto Networks|PAN-OS|10.2.0|end|TRAFFIC|3|src=192.0.2.10 dst=198.51.100.5 proto=tcp act=allow"
                )
                .into_bytes(),
            ),
        ];

        for (parsers, process, supplied_setup, invalid_setup, local_setup, ingress) in cases {
            let supplied =
                run_packaged_parser_json(parsers, Some(supplied_setup), process, &ingress);
            assert_eq!(supplied["time"], expected, "{process} supplied timezone");

            let default_setup = match process {
                "parse_juniper_srx_syslog" => {
                    Some("workspace.juniper_srx_syslog = { body: ingress }")
                }
                _ => None,
            };
            let defaulted = run_packaged_parser_json(parsers, default_setup, process, &ingress);
            // All RFC 3164 parsers with an undocumented source zone default to
            // the limpid host's system timezone (host-local most-likely
            // assumption); only vendor-documented UTC formats default to UTC.
            let default_expected = local_expected;
            assert_eq!(
                defaulted["time"], default_expected,
                "{process} default timezone"
            );

            for invalid_setup in [invalid_setup, local_setup] {
                let invalid =
                    run_packaged_parser_pipeline(parsers, Some(invalid_setup), process, &ingress);
                assert_eq!(
                    invalid.termination,
                    PipelineTermination::Errored,
                    "{process}"
                );
                assert!(
                    invalid.outputs.is_empty(),
                    "{process} emitted invalid timezone"
                );
                let reason = match &invalid.errored[0] {
                    ErroredEventContext::Process { reason, .. } => reason,
                    other => panic!("expected process error, got {other:?}"),
                };
                assert!(reason.contains("timezone"), "{process}: {reason}");
            }
        }
    }

    #[test]
    fn time_normalization_runs_at_public_leaf_boundaries() {
        let auditd = run_packaged_parser_json(
            &["parse_auditd"],
            Some(
                "workspace.auditd = { body: ingress }; workspace.auditd_class = parse_auditd_classify(workspace.auditd.body)",
            ),
            "parse_auditd_auth_dispatch",
            b"type=USER_LOGIN msg=audit(1710000000.123:1): pid=42 uid=0 auid=1000 ses=1 res=success acct=alice addr=192.0.2.10 terminal=pts/0 exe=/usr/bin/login",
        );
        assert_eq!(auditd["time"], 1_710_000_000_123_000_000_i64);

        let juniper = run_packaged_parser_json(
            &["parse_juniper_srx_sd_syslog"],
            Some(
                "workspace.juniper_srx_sd_syslog = { body: ingress }; workspace.srx_sd_class = parse_juniper_srx_sd_syslog_classify(workspace.juniper_srx_sd_syslog.body)",
            ),
            "parse_juniper_srx_sd_syslog_flow_create",
            b"<14>1 2026-04-30T01:23:45.123456789Z srx01 RT_FLOW - RT_FLOW_SESSION_CREATE [junos@2636 source-address=\"192.0.2.10\" source-port=\"54321\" destination-address=\"198.51.100.5\" destination-port=\"443\" protocol-id=\"6\" policy-name=\"allow-web\"]",
        );
        assert_eq!(juniper["time"], 1_777_512_225_123_456_789_i64);
    }

    #[test]
    fn packaged_parser_public_leaves_reject_invalid_source_severity() {
        let cases = [
            (
                &["parse_asa"][..],
                "workspace.asa = { level: \"8\", body: \"Login permitted from 192.0.2.10/54321 to outside:198.51.100.5/SSH for user admin\" }",
                "parse_asa_605005_login_permitted",
                "invalid ASA payload severity level 8",
            ),
            (
                &["parse_aws_guardduty"][..],
                "workspace.gd = { type: \"Recon:EC2/Test\", severity: 11 }; workspace.gd_type = aws_guardduty_split_type(workspace.gd.type); workspace.gd_tactic = \"Reconnaissance\"",
                "parse_aws_guardduty_finding",
                "invalid GuardDuty severity 11",
            ),
            (
                &["parse_azure_activity"][..],
                "workspace.az = { level: \"TRACE\" }",
                "parse_azure_activity_record",
                "invalid Azure Activity Log level TRACE",
            ),
            (
                &["parse_fortigate_cef"][..],
                "workspace.cef = { severity: 0 }",
                "parse_fortigate_cef_traffic",
                "invalid FortiGate CEF priority 0",
            ),
            (
                &["parse_okta_system"][..],
                "workspace.okta = { severity: \"TRACE\" }",
                "parse_okta_user_authentication",
                "invalid Okta System Log severity TRACE",
            ),
            (
                &["parse_winevent_json"][..],
                "workspace.winevent = { EventType: \"TRACE\" }",
                "parse_winevent_4624_logon_success",
                "invalid NXLog Windows Event severity TRACE",
            ),
        ];

        for (parsers, setup, process, expected_error) in cases {
            let result = run_packaged_parser_pipeline(parsers, Some(setup), process, b"fixture");
            assert_eq!(
                result.termination,
                PipelineTermination::Errored,
                "{process} unexpectedly succeeded"
            );
            assert!(result.outputs.is_empty(), "{process} emitted output");
            assert_eq!(result.errored.len(), 1);
            let reason = match &result.errored[0] {
                ErroredEventContext::Process { reason, .. } => reason,
                other => panic!("expected process error, got {other:?}"),
            };
            assert!(
                reason.contains(expected_error),
                "{process} error was {reason:?}"
            );
        }
    }

    #[test]
    fn packaged_fortigate_dns_uses_rcode_not_error() {
        let with_rcode = run_packaged_parser_json(
            &["parse_fortigate_syslog"],
            None,
            "parse_fortigate_syslog",
            b"<134>eventtime=1777284000123456789 level=notice type=utm subtype=dns action=blocked srcip=192.0.2.10 dstip=198.51.100.5 qname=example.test qtype=A rcode=NXDOMAIN error=SERVFAIL",
        );
        assert_eq!(with_rcode["rcode_id"], 3);

        let without_rcode = run_packaged_parser_json(
            &["parse_fortigate_syslog"],
            None,
            "parse_fortigate_syslog",
            b"<134>eventtime=1777284000123456789 level=notice type=utm subtype=dns action=blocked srcip=192.0.2.10 dstip=198.51.100.5 qname=example.test qtype=A error=NXDOMAIN",
        );
        assert!(without_rcode["rcode_id"].is_null());
    }

    fn run_packaged_otlp_composer(
        cfg: &CompiledConfig,
        funcs: &FunctionRegistry,
        pipeline_name: &str,
        workspace: serde_json::Value,
    ) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        run_packaged_otlp_composer_at(cfg, funcs, pipeline_name, workspace, chrono::Utc::now())
    }

    fn run_packaged_otlp_composer_at(
        cfg: &CompiledConfig,
        funcs: &FunctionRegistry,
        pipeline_name: &str,
        workspace: serde_json::Value,
        received_at: chrono::DateTime<chrono::Utc>,
    ) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        let resource_logs = run_packaged_otlp_resource_logs_at(
            cfg,
            funcs,
            pipeline_name,
            &[],
            workspace,
            received_at,
        );
        resource_logs.scope_logs[0].log_records[0].clone()
    }

    fn run_packaged_otlp_resource_logs_at(
        cfg: &CompiledConfig,
        funcs: &FunctionRegistry,
        pipeline_name: &str,
        ingress: &[u8],
        workspace: serde_json::Value,
        received_at: chrono::DateTime<chrono::Utc>,
    ) -> opentelemetry_proto::tonic::logs::v1::ResourceLogs {
        use crate::dsl::value::OwnedValue;
        use crate::dsl::value_json::json_to_value;
        use crate::event::OwnedEvent;
        use bytes::Bytes;
        use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
        use prost::Message;

        let mut event = OwnedEvent::new(
            Bytes::copy_from_slice(ingress),
            "127.0.0.1:0".parse().expect("valid test source"),
        );
        event.received_at = received_at;
        let OwnedValue::Object(workspace) =
            json_to_value(&workspace).expect("workspace JSON must convert")
        else {
            panic!("workspace fixture must be an object");
        };
        event.workspace = workspace.into_iter().collect();

        let pipeline = cfg
            .pipelines
            .get(pipeline_name)
            .expect("test pipeline must exist");
        let result = run_pipeline(
            pipeline,
            &event,
            cfg,
            funcs,
            None,
            None,
            OutputCapturePolicy::CaptureAll,
            &mut bumpalo::Bump::new(),
        )
        .expect("composer pipeline must run");
        assert_eq!(
            result.termination,
            PipelineTermination::Finished,
            "pipeline errors: {:?}",
            result.errored
        );
        assert_eq!(result.outputs.len(), 1);

        ResourceLogs::decode(result.outputs[0].1.egress.as_ref())
            .expect("composer output must decode as ResourceLogs")
    }

    fn otlp_string_attribute<'a>(
        attributes: &'a [opentelemetry_proto::tonic::common::v1::KeyValue],
        key: &str,
    ) -> Option<&'a str> {
        use opentelemetry_proto::tonic::common::v1::any_value;

        attributes.iter().find_map(|attribute| {
            if attribute.key != key {
                return None;
            }
            match attribute
                .value
                .as_ref()
                .and_then(|value| value.value.as_ref())
            {
                Some(any_value::Value::StringValue(value)) => Some(value.as_str()),
                _ => None,
            }
        })
    }

    fn otlp_int_attribute(
        attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
        key: &str,
    ) -> Option<i64> {
        use opentelemetry_proto::tonic::common::v1::any_value;

        attributes.iter().find_map(|attribute| {
            if attribute.key != key {
                return None;
            }
            match attribute
                .value
                .as_ref()
                .and_then(|value| value.value.as_ref())
            {
                Some(any_value::Value::IntValue(value)) => Some(*value),
                _ => None,
            }
        })
    }

    fn otlp_string_array_attribute<'a>(
        attributes: &'a [opentelemetry_proto::tonic::common::v1::KeyValue],
        key: &str,
    ) -> Option<Vec<&'a str>> {
        use opentelemetry_proto::tonic::common::v1::any_value;

        attributes.iter().find_map(|attribute| {
            if attribute.key != key {
                return None;
            }
            let Some(any_value::Value::ArrayValue(array)) = attribute
                .value
                .as_ref()
                .and_then(|value| value.value.as_ref())
            else {
                return None;
            };
            array
                .values
                .iter()
                .map(|value| match value.value.as_ref() {
                    Some(any_value::Value::StringValue(value)) => Some(value.as_str()),
                    _ => None,
                })
                .collect()
        })
    }

    fn assert_otlp_attribute_contract(
        attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
    ) {
        use std::collections::HashSet;

        let mut keys = HashSet::new();
        for attribute in attributes {
            assert!(
                keys.insert(attribute.key.as_str()),
                "duplicate OTLP key: {}",
                attribute.key
            );
            assert!(!attribute.key.starts_with("lsis."));
            assert!(!attribute.key.starts_with("metadata."));
            assert!(!attribute.key.starts_with("ocsf."));
            assert!(!matches!(
                attribute.key.as_str(),
                "class_uid"
                    | "category_uid"
                    | "activity_id"
                    | "type_uid"
                    | "status_id"
                    | "severity_id"
                    | "disposition_id"
            ));
        }
    }

    fn collect_expr_parsed_reads(expr: &Expr, reads: &mut std::collections::BTreeSet<String>) {
        if let ExprKind::Ident(parts) = &expr.kind
            && parts.starts_with(&["workspace".into(), "lsis".into(), "parsed".into()])
            && parts.len() > 3
        {
            reads.insert(parts[3..].join("."));
        }
        crate::dsl::ast::walk_children(expr, |child| collect_expr_parsed_reads(child, reads));
    }

    fn collect_statement_parsed_reads(
        statements: &[ProcessStatement],
        reads: &mut std::collections::BTreeSet<String>,
    ) {
        for statement in statements {
            match statement {
                ProcessStatement::Assign(_, expr)
                | ProcessStatement::LetBinding(_, expr)
                | ProcessStatement::ExprStmt(expr) => collect_expr_parsed_reads(expr, reads),
                ProcessStatement::Error(Some(expr)) => collect_expr_parsed_reads(expr, reads),
                ProcessStatement::If(chain) => {
                    for (condition, body) in &chain.branches {
                        collect_expr_parsed_reads(condition, reads);
                        for branch in body {
                            if let BranchBody::Process(statement) = branch {
                                collect_statement_parsed_reads(
                                    std::slice::from_ref(statement),
                                    reads,
                                );
                            }
                        }
                    }
                    if let Some(body) = &chain.else_body {
                        for branch in body {
                            if let BranchBody::Process(statement) = branch {
                                collect_statement_parsed_reads(
                                    std::slice::from_ref(statement),
                                    reads,
                                );
                            }
                        }
                    }
                }
                ProcessStatement::Switch(scrutinee, arms) => {
                    collect_expr_parsed_reads(scrutinee, reads);
                    for arm in arms {
                        if let Some(pattern) = &arm.pattern {
                            collect_expr_parsed_reads(pattern, reads);
                        }
                        for branch in &arm.body {
                            if let BranchBody::Process(statement) = branch {
                                collect_statement_parsed_reads(
                                    std::slice::from_ref(statement),
                                    reads,
                                );
                            }
                        }
                    }
                }
                ProcessStatement::TryCatch(try_body, catch_body) => {
                    collect_statement_parsed_reads(try_body, reads);
                    collect_statement_parsed_reads(catch_body, reads);
                }
                ProcessStatement::ProcessCall(_)
                | ProcessStatement::Drop
                | ProcessStatement::Error(None) => {}
            }
        }
    }

    fn collect_written_shape(
        expr: &Expr,
        prefix: &str,
        config: &CompiledConfig,
        writes: &mut std::collections::BTreeSet<String>,
        active_functions: &mut std::collections::HashSet<String>,
    ) {
        match &expr.kind {
            ExprKind::HashLit(fields) => {
                for (key, value) in fields {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_written_shape(value, &path, config, writes, active_functions);
                }
            }
            ExprKind::SwitchExpr { arms, .. } => {
                for arm in arms {
                    collect_written_shape(&arm.body, prefix, config, writes, active_functions);
                }
            }
            ExprKind::FuncCall { name, .. } if config.functions.contains_key(name) => {
                if !active_functions.insert(name.clone()) {
                    return;
                }
                let function = &config.functions[name];
                let return_expr = match &function.body.ret.kind {
                    ExprKind::Ident(parts) if parts.len() == 1 => function
                        .body
                        .lets
                        .iter()
                        .rev()
                        .find(|binding| binding.name == parts[0])
                        .map(|binding| &binding.value)
                        .unwrap_or(&function.body.ret),
                    _ => &function.body.ret,
                };
                collect_written_shape(return_expr, prefix, config, writes, active_functions);
                active_functions.remove(name);
            }
            ExprKind::Ident(parts) if parts.len() == 1 => {
                writes.insert(format!("{prefix}.*"));
            }
            _ if !prefix.is_empty() => {
                writes.insert(prefix.to_string());
            }
            _ => {}
        }
    }

    fn collect_reachable_parsed_writes(
        process_name: &str,
        config: &CompiledConfig,
        visited_processes: &mut std::collections::HashSet<String>,
        writes: &mut std::collections::BTreeSet<String>,
    ) {
        if !visited_processes.insert(process_name.to_string()) {
            return;
        }
        let process = config
            .processes
            .get(process_name)
            .unwrap_or_else(|| panic!("missing packaged process {process_name}"));

        fn walk(
            statements: &[ProcessStatement],
            config: &CompiledConfig,
            visited_processes: &mut std::collections::HashSet<String>,
            writes: &mut std::collections::BTreeSet<String>,
        ) {
            for statement in statements {
                match statement {
                    ProcessStatement::Assign(AssignTarget::Workspace(path), expr)
                        if path.starts_with(&["lsis".into(), "parsed".into()]) =>
                    {
                        let prefix = path[2..].join(".");
                        collect_written_shape(
                            expr,
                            &prefix,
                            config,
                            writes,
                            &mut std::collections::HashSet::new(),
                        );
                    }
                    ProcessStatement::ProcessCall(name) => {
                        collect_reachable_parsed_writes(name, config, visited_processes, writes)
                    }
                    ProcessStatement::If(chain) => {
                        for (_, body) in &chain.branches {
                            for branch in body {
                                if let BranchBody::Process(statement) = branch {
                                    walk(
                                        std::slice::from_ref(statement),
                                        config,
                                        visited_processes,
                                        writes,
                                    );
                                }
                            }
                        }
                        if let Some(body) = &chain.else_body {
                            for branch in body {
                                if let BranchBody::Process(statement) = branch {
                                    walk(
                                        std::slice::from_ref(statement),
                                        config,
                                        visited_processes,
                                        writes,
                                    );
                                }
                            }
                        }
                    }
                    ProcessStatement::Switch(_, arms) => {
                        for arm in arms {
                            for branch in &arm.body {
                                if let BranchBody::Process(statement) = branch {
                                    walk(
                                        std::slice::from_ref(statement),
                                        config,
                                        visited_processes,
                                        writes,
                                    );
                                }
                            }
                        }
                    }
                    ProcessStatement::TryCatch(try_body, catch_body) => {
                        walk(try_body, config, visited_processes, writes);
                        walk(catch_body, config, visited_processes, writes);
                    }
                    _ => {}
                }
            }
        }

        walk(&process.body, config, visited_processes, writes);
    }

    #[test]
    fn packaged_otlp_adapter_parsed_reads_are_written_by_source_parsers() {
        let config = compile_packaged_otlp_composers().unwrap();
        let mut adapters = config
            .processes
            .keys()
            .filter(|name| name.ends_with("_to_otlp"))
            .cloned()
            .collect::<Vec<_>>();
        adapters.sort();
        assert_eq!(
            adapters.len(),
            31,
            "packaged source adapter inventory drift"
        );

        let mut mismatches = Vec::new();
        for adapter_name in adapters {
            let source_name = adapter_name.trim_end_matches("_to_otlp");
            let parser_name = format!("parse_{source_name}");
            let mut reads = std::collections::BTreeSet::new();
            collect_statement_parsed_reads(&config.processes[&adapter_name].body, &mut reads);

            let mut writes = std::collections::BTreeSet::new();
            collect_reachable_parsed_writes(
                &parser_name,
                &config,
                &mut std::collections::HashSet::new(),
                &mut writes,
            );

            for read in reads {
                let matched = writes.iter().any(|write| {
                    write == &read
                        || write.strip_suffix(".*").is_some_and(|prefix| {
                            read == prefix || read.starts_with(&format!("{prefix}."))
                        })
                });
                if !matched {
                    mismatches.push(format!(
                        "{adapter_name} reads parsed.{read}, but {parser_name} never writes it"
                    ));
                }
            }
        }

        assert!(
            mismatches.is_empty(),
            "packaged OTLP adapter/parser path mismatches:\n{}",
            mismatches.join("\n")
        );
    }

    #[test]
    fn packaged_ocsf_severity_unknown_and_other_normalize_without_canonical_zero() {
        let unknown = run_packaged_parser_json(
            &["parse_ocsf"],
            None,
            "parse_ocsf",
            br#"{"class_uid":4001,"severity_id":0}"#,
        );
        assert!(unknown["severity_id"].is_null());
        assert!(unknown["severity_number"].is_null());

        let other = run_packaged_parser_json(
            &["parse_ocsf"],
            None,
            "parse_ocsf",
            br#"{"class_uid":4001,"severity_id":99}"#,
        );
        assert_eq!(other["severity_id"], 99);
        assert!(other["severity_number"].is_null());

        let error = run_packaged_parser_json(
            &["parse_ocsf"],
            None,
            "parse_ocsf",
            br#"{"class_uid":4001,"severity_id":3}"#,
        );
        assert!(error["severity_id"].is_null());
        assert_eq!(error["severity_number"], 17);
    }

    #[test]
    fn packaged_changed_otlp_routes_preserve_classification_and_fact_paths() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);
        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");

        for (wire, expected_kind, expected_category, expected_type) in [
            (
                b"type=USER_END msg=audit(1710000000.123:43): pid=100 uid=0 auid=1000 ses=1 msg='op=PAM:session_close acct=alice exe=/usr/bin/sshd hostname=? addr=192.0.2.10 terminal=ssh res=success'".as_slice(),
                "event",
                "authentication",
                "end",
            ),
            (
                b"type=SERVICE_STOP msg=audit(1710000000.123:44): pid=1 uid=0 auid=0 ses=1 comm=\"systemd\" exe=\"/usr/lib/systemd/systemd\" unit=\"sshd.service\" res=success".as_slice(),
                "event",
                "process",
                "end",
            ),
            (
                b"type=CONFIG_CHANGE msg=audit(1710000000.123:45): pid=1 uid=0 auid=0 ses=1 op=updated_rules res=success".as_slice(),
                "event",
                "configuration",
                "change",
            ),
            (
                b"type=SOCKADDR msg=audit(1710000000.123:46): saddr=02000035C000020A0000000000000000 src=192.0.2.10 dst=198.51.100.5 res=success".as_slice(),
                "event",
                "network",
                "info",
            ),
            (
                b"type=AVC msg=audit(1710000000.123:47): avc: denied { read } for pid=42 comm=\"cat\" scontext=a tcontext=b tclass=file".as_slice(),
                "alert",
                "intrusion_detection",
                "info",
            ),
        ] {
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                "auditd_otlp",
                wire,
                json!({"auditd": {"body": String::from_utf8_lossy(wire), "hostname": "host-1"}}),
                received_at,
            );
            let attributes = &resource_logs.scope_logs[0].log_records[0].attributes;
            assert_eq!(
                otlp_string_attribute(attributes, "event.kind"),
                Some(expected_kind)
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.category"),
                Some(vec![expected_category])
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.type"),
                Some(vec![expected_type])
            );
        }

        for (category, expected_kind, expected_category, expected_type) in [
            ("Security", "alert", Some("intrusion_detection"), "info"),
            ("Alert", "alert", Some("intrusion_detection"), "info"),
            ("Administrative", "event", Some("configuration"), "change"),
            ("ServiceHealth", "event", None, "info"),
            ("Recommendation", "event", None, "info"),
        ] {
            let body = json!({
                "time": "2026-04-29T10:00:00Z",
                "resourceId": "/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Compute/virtualMachines/vm1",
                "category": category,
                "level": "Informational",
                "operationName": "Example operation",
                "resultType": "Success"
            });
            let wire = serde_json::to_vec(&body).unwrap();
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                "azure_activity_otlp",
                &wire,
                json!({}),
                received_at,
            );
            let attributes = &resource_logs.scope_logs[0].log_records[0].attributes;
            assert_eq!(
                otlp_string_attribute(attributes, "event.kind"),
                Some(expected_kind)
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.category"),
                expected_category.map(|value| vec![value])
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.type"),
                Some(vec![expected_type])
            );
        }

        let checkpoint_wire = b"<134>1 2026-04-30T01:23:45Z - CheckPoint - - [action:\"Accept\"; product:\"VPN-1 & FireWall-1\"; src:\"192.0.2.10\"; dst:\"198.51.100.5\"; proto:\"tcp\"; severity:\"Informational\"; sys_message::\"Allowed connection\"]";
        let checkpoint = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "checkpoint_syslog_otlp",
            checkpoint_wire,
            json!({"checkpoint_syslog": {"body": String::from_utf8_lossy(checkpoint_wire), "hostname": "fallback-fw"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &checkpoint.resource.as_ref().expect("resource").attributes,
                "host.name"
            ),
            Some("fallback-fw")
        );
        assert_eq!(
            otlp_string_attribute(
                &checkpoint.scope_logs[0].log_records[0].attributes,
                "message"
            ),
            Some("Allowed connection")
        );

        let fortigate_dns_wire = b"<129>Apr 27 10:00:00 fw01 CEF:0|Fortinet|Fortigate|v7.4.11|dns|utm:dns|3|cat=utm:dns act=blocked src=192.0.2.10 dst=198.51.100.5 FTNTFGTqname=example.test FTNTFGTqtype=A FTNTFGTrcode=NXDOMAIN";
        let fortigate_dns = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "fortigate_cef_otlp",
            fortigate_dns_wire,
            json!({"fortigate_cef": {"timezone": "UTC"}}),
            received_at,
        );
        let fortigate_dns_attributes = &fortigate_dns.scope_logs[0].log_records[0].attributes;
        assert_eq!(
            otlp_string_attribute(fortigate_dns_attributes, "event.action"),
            Some("blocked")
        );
        assert_eq!(
            otlp_string_attribute(fortigate_dns_attributes, "dns.question.name"),
            Some("example.test")
        );
        assert_eq!(
            otlp_string_attribute(fortigate_dns_attributes, "dns.question.type"),
            Some("A")
        );

        let openssh_wire = b"Disconnected from user alice 192.0.2.10 port 54321";
        let openssh = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "openssh_otlp",
            openssh_wire,
            json!({"openssh": {"body": String::from_utf8_lossy(openssh_wire), "hostname": "host-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_array_attribute(
                &openssh.scope_logs[0].log_records[0].attributes,
                "event.type"
            ),
            Some(vec!["end"])
        );

        let paloalto_cef_wire = b"<134>Apr 27 10:00:00 fw-pan01 CEF:0|Palo Alto Networks|PAN-OS|10.2.0|gp|GLOBALPROTECT|3|act=logout suser=alice src=192.0.2.10";
        let paloalto_cef = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "paloalto_cef_otlp",
            paloalto_cef_wire,
            json!({"paloalto_cef": {"timezone": "UTC"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_array_attribute(
                &paloalto_cef.scope_logs[0].log_records[0].attributes,
                "event.type"
            ),
            Some(vec!["end"])
        );

        let mut pan_fields = vec![""; 28];
        pan_fields[1] = "2026/04/27 10:00:00";
        pan_fields[2] = "012345678";
        pan_fields[3] = "GLOBALPROTECT";
        pan_fields[6] = "2026/04/27 10:00:01";
        pan_fields[8] = "gateway-logout";
        pan_fields[9] = "disconnected";
        pan_fields[12] = "alice";
        pan_fields[15] = "192.0.2.10";
        pan_fields[27] = "Disconnected";
        let paloalto_native_wire =
            format!("<134>Apr 27 10:00:00 fw-pan01 {}", pan_fields.join(","));
        let paloalto_native = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "paloalto_native_otlp",
            paloalto_native_wire.as_bytes(),
            json!({"paloalto_syslog": {"timezone": "UTC"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_array_attribute(
                &paloalto_native.scope_logs[0].log_records[0].attributes,
                "event.type"
            ),
            Some(vec!["end"])
        );

        for (event_id, event_data, expected_name, expected_parent, expected_file) in [
            (
                1,
                json!({"UtcTime":"2026-04-29 10:00:00.123","ProcessId":"42","Image":"C:\\Windows\\app.exe","CommandLine":"app.exe","User":"CORP\\alice","ParentProcessId":"7","ParentImage":"C:\\Windows\\parent.exe"}),
                Some("app.exe"),
                Some("parent.exe"),
                None,
            ),
            (
                3,
                json!({"UtcTime":"2026-04-29 10:00:00.123","ProcessId":"43","Image":"C:\\Windows\\net.exe","User":"CORP\\alice","Protocol":"tcp","SourceIp":"192.0.2.10","SourcePort":"54321","DestinationIp":"198.51.100.5","DestinationPort":"443","Initiated":"true"}),
                Some("net.exe"),
                None,
                None,
            ),
            (
                11,
                json!({"UtcTime":"2026-04-29 10:00:00.123","ProcessId":"44","Image":"C:\\Windows\\file.exe","User":"CORP\\alice","TargetFilename":"C:\\Temp\\created.txt"}),
                Some("file.exe"),
                None,
                Some("created.txt"),
            ),
        ] {
            let body = json!({
                "EventID": event_id,
                "Channel": "Microsoft-Windows-Sysmon/Operational",
                "EventTime": "2026-04-29T10:00:00.123456Z",
                "Computer": "WORKSTATION1",
                "EventData": event_data
            });
            let wire = serde_json::to_vec(&body).unwrap();
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                "sysmon_otlp",
                &wire,
                json!({"sysmon": {"body": body}}),
                received_at,
            );
            let attributes = &resource_logs.scope_logs[0].log_records[0].attributes;
            assert_eq!(
                otlp_string_attribute(attributes, "process.name"),
                expected_name
            );
            assert_eq!(
                otlp_string_attribute(attributes, "process.parent.name"),
                expected_parent
            );
            assert_eq!(
                otlp_string_attribute(attributes, "file.name"),
                expected_file
            );
        }

        let zeek_dns_body = json!({"_path":"dns","ts":1710000000.125,"uid":"D1","id":{"orig_h":"192.0.2.10","orig_p":54321,"resp_h":"198.51.100.53","resp_p":53},"query":"example.test","qtype_name":"A","qclass_name":"C_INTERNET","rcode_name":"NOERROR","answers":["198.51.100.5"]});
        let zeek_dns_wire = serde_json::to_vec(&zeek_dns_body).unwrap();
        let zeek_dns = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "zeek_default_otlp",
            &zeek_dns_wire,
            json!({"zeek": {"body": zeek_dns_body, "hostname": "sensor-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &zeek_dns.scope_logs[0].log_records[0].attributes,
                "dns.question.name"
            ),
            Some("example.test")
        );

        let zeek_http_body = json!({"_path":"http","ts":1710000000.25,"uid":"H1","id":{"orig_h":"192.0.2.10","orig_p":54321,"resp_h":"198.51.100.5","resp_p":80},"method":"GET","host":"example.test","uri":"/index.html","status_code":200});
        let zeek_http_wire = serde_json::to_vec(&zeek_http_body).unwrap();
        for pipeline in ["zeek_default_otlp", "zeek_soc_otlp", "zeek_full_otlp"] {
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                pipeline,
                &zeek_http_wire,
                json!({"zeek": {"body": zeek_http_body.clone(), "hostname": "sensor-1"}}),
                received_at,
            );
            let attributes = &resource_logs.scope_logs[0].log_records[0].attributes;
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.category"),
                Some(vec!["web"])
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.type"),
                Some(vec!["access"])
            );
            assert_eq!(
                otlp_string_attribute(attributes, "url.domain"),
                Some("example.test")
            );
            assert_eq!(
                otlp_string_attribute(attributes, "url.path"),
                Some("/index.html")
            );
        }

        let zeek_notice_body = json!({"_path":"notice","ts":1710000000.5,"uid":"N1","id":{"orig_h":"192.0.2.10","orig_p":1,"resp_h":"198.51.100.5","resp_p":2},"note":"Scan::Port_Scan","msg":"Example notice","sub":"scan","actions":["Notice::ACTION_LOG"]});
        let zeek_notice_wire = serde_json::to_vec(&zeek_notice_body).unwrap();
        for pipeline in ["zeek_default_otlp", "zeek_soc_otlp", "zeek_full_otlp"] {
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                pipeline,
                &zeek_notice_wire,
                json!({"zeek": {"body": zeek_notice_body.clone(), "hostname": "sensor-1"}}),
                received_at,
            );
            let attributes = &resource_logs.scope_logs[0].log_records[0].attributes;
            assert_eq!(
                otlp_string_attribute(attributes, "event.kind"),
                Some("alert")
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.category"),
                Some(vec!["intrusion_detection"])
            );
            assert_eq!(
                otlp_string_array_attribute(attributes, "event.type"),
                Some(vec!["info"])
            );
        }

        let zeek_smb_body = json!({"_path":"smb_files","ts":1710000000.75,"uid":"S1","id":{"orig_h":"192.0.2.10","orig_p":54321,"resp_h":"198.51.100.5","resp_p":445},"action":"SMB::FILE_OPEN","name":"report.txt","path":"\\\\share"});
        let zeek_smb_wire = serde_json::to_vec(&zeek_smb_body).unwrap();
        for pipeline in ["zeek_soc_otlp", "zeek_full_otlp"] {
            let resource_logs = run_packaged_otlp_resource_logs_at(
                &cfg,
                &funcs,
                pipeline,
                &zeek_smb_wire,
                json!({"zeek": {"body": zeek_smb_body.clone(), "hostname": "sensor-1"}}),
                received_at,
            );
            assert_eq!(
                otlp_string_attribute(
                    &resource_logs.scope_logs[0].log_records[0].attributes,
                    "file.name"
                ),
                Some("report.txt")
            );
        }
    }

    #[test]
    fn packaged_compose_otlp_projects_canonical_severity() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);

        for (number, text) in [
            (9, "INFO"),
            (13, "WARNING"),
            (19, "ALERT"),
            (21, "CRITICAL"),
        ] {
            let record = run_packaged_otlp_composer(
                &cfg,
                &funcs,
                "direct_otlp",
                json!({
                    "lsis": {
                        "parsed": {
                            "time": 1,
                            "severity_number": number,
                            "severity": text
                        }
                    }
                }),
            );
            assert_eq!(record.severity_number, number);
            assert_eq!(record.severity_text, text);
        }

        let overridden = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({
                "lsis": {
                    "parsed": { "time": 1, "severity_number": 9, "severity": "INFO" },
                    "shed": { "otlp": { "log_record": { "severity_text": "NOTICE" } } }
                }
            }),
        );
        assert_eq!(overridden.severity_number, 9);
        assert_eq!(overridden.severity_text, "NOTICE");

        let missing = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({ "lsis": { "parsed": { "time": 1 } } }),
        );
        assert_eq!(missing.severity_number, 0);
        assert_eq!(missing.severity_text, "");

        let text_only = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({ "lsis": { "parsed": { "time": 1, "severity": "UNDEFINED" } } }),
        );
        assert_eq!(text_only.severity_number, 0);
        assert_eq!(text_only.severity_text, "UNDEFINED");
        assert!(text_only.body.is_none());

        let number_overridden = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({
                "lsis": {
                    "parsed": { "severity_number": 9 },
                    "shed": { "otlp": { "log_record": { "severity_number": 21 } } }
                }
            }),
        );
        assert_eq!(number_overridden.severity_number, 21);
    }

    #[test]
    fn packaged_ocsf_in_otlp_preserves_parsed_severity() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);

        let source_backed = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "ocsf_in_otlp",
            json!({
                "lsis": {
                    "parsed": {
                        "class_uid": 3002,
                        "activity_id": 1,
                        "time": 1,
                        "severity_number": 19,
                        "severity": "HIGH"
                    }
                }
            }),
        );
        assert_eq!(source_backed.severity_number, 19);
        assert_eq!(source_backed.severity_text, "HIGH");
        let source_backed_body = match source_backed.body.and_then(|body| body.value) {
            Some(any_value::Value::StringValue(body)) => body,
            other => panic!("expected OCSF string body, got {other:?}"),
        };
        assert!(source_backed_body.contains("\"severity_id\":4"));

        let source_less = run_packaged_otlp_composer(
            &cfg,
            &funcs,
            "ocsf_in_otlp",
            json!({
                "lsis": {
                    "parsed": { "class_uid": 3002, "activity_id": 1, "time": 1 }
                }
            }),
        );
        assert_eq!(source_less.severity_number, 0);
        assert_eq!(source_less.severity_text, "");
        let source_less_body = match source_less.body.and_then(|body| body.value) {
            Some(any_value::Value::StringValue(body)) => body,
            other => panic!("expected OCSF string body, got {other:?}"),
        };
        assert!(source_less_body.contains("\"severity_id\":0"));
    }

    #[test]
    fn packaged_compose_otlp_defaults_and_overrides_observed_time() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);

        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");
        let received_ns = 1_784_073_600_123_456_789;
        let event_ns = 1_710_000_000_123_456_789;
        let override_ns = 1_700_000_000_987_654_321;

        let source_backed = run_packaged_otlp_composer_at(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({
                "lsis": {
                    "parsed": {
                        "time": event_ns,
                        "severity_number": 19,
                        "severity": "HIGH"
                    }
                }
            }),
            received_at,
        );
        assert_eq!(source_backed.time_unix_nano, event_ns);
        assert_eq!(source_backed.observed_time_unix_nano, received_ns);

        let overridden = run_packaged_otlp_composer_at(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({
                "lsis": {
                    "parsed": { "time": event_ns },
                    "shed": {
                        "otlp": {
                            "log_record": {
                                "observed_time_unix_nano": override_ns
                            }
                        }
                    }
                }
            }),
            received_at,
        );
        assert_eq!(overridden.time_unix_nano, event_ns);
        assert_eq!(overridden.observed_time_unix_nano, override_ns);

        let source_less = run_packaged_otlp_composer_at(
            &cfg,
            &funcs,
            "ocsf_in_otlp",
            json!({
                "lsis": {
                    "parsed": { "class_uid": 3002, "activity_id": 1 }
                }
            }),
            received_at,
        );
        assert_eq!(source_less.time_unix_nano, 0);
        assert_eq!(source_less.observed_time_unix_nano, received_ns);

        let event_time_overridden = run_packaged_otlp_composer_at(
            &cfg,
            &funcs,
            "direct_otlp",
            json!({
                "lsis": {
                    "parsed": { "time": event_ns },
                    "shed": {
                        "otlp": {
                            "log_record": { "time_unix_nano": override_ns }
                        }
                    }
                }
            }),
            received_at,
        );
        assert_eq!(event_time_overridden.time_unix_nano, override_ns);
        assert_eq!(event_time_overridden.observed_time_unix_nano, received_ns);
    }

    #[test]
    fn packaged_compose_otlp_omits_defaults_and_preserves_anyvalue_body() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);

        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");
        let resource_logs = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "direct_otlp",
            &[],
            json!({
                "lsis": {
                    "shed": {
                        "otlp": {
                            "log_record": {
                                "body": { "int_value": 9_007_199_254_740_993_i64 }
                            }
                        }
                    }
                }
            }),
            received_at,
        );
        assert!(resource_logs.resource.is_none());
        assert!(resource_logs.scope_logs[0].scope.is_none());
        let record = &resource_logs.scope_logs[0].log_records[0];
        assert_eq!(record.time_unix_nano, 0);
        assert_eq!(record.observed_time_unix_nano, 1_784_073_600_123_456_789);
        assert!(record.attributes.is_empty());
        assert!(matches!(
            record.body.as_ref().and_then(|body| body.value.as_ref()),
            Some(any_value::Value::IntValue(9_007_199_254_740_993))
        ));
    }

    #[test]
    fn packaged_fortigate_native_otlp_adapter_projects_traffic_and_ips() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);
        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");

        let traffic_wire = br#"<0>eventtime=1777284000123456789 date=2026-04-27 time=10:00:00 devname="fw01" devid="FGT-001" osname="FortiOS 7.4" level="notice" type="traffic" subtype="forward" srcip=192.0.2.10 srcport=54321 src_port=54322 dstip=198.51.100.5 dstport=443 dst_port=444 proto=6 action="accept" sentbyte=1024 rcvdbyte=512"#;
        let traffic = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "fortigate_native_otlp",
            traffic_wire,
            json!({}),
            received_at,
        );
        let resource_attributes = &traffic.resource.as_ref().expect("resource").attributes;
        assert_eq!(
            otlp_string_attribute(resource_attributes, "observer.vendor"),
            Some("Fortinet")
        );
        assert_eq!(
            otlp_string_attribute(resource_attributes, "observer.product"),
            Some("FortiGate")
        );
        assert_eq!(
            otlp_string_attribute(resource_attributes, "observer.type"),
            Some("firewall")
        );
        assert_eq!(
            otlp_string_attribute(resource_attributes, "observer.serial_number"),
            Some("FGT-001")
        );
        assert_eq!(
            otlp_string_attribute(resource_attributes, "host.name"),
            Some("fw01")
        );
        assert_eq!(
            otlp_string_attribute(resource_attributes, "telemetry.sdk.name"),
            Some("limpid")
        );
        assert!(otlp_string_attribute(resource_attributes, "telemetry.sdk.version").is_some());
        assert!(traffic.scope_logs[0].scope.is_none());
        let traffic_record = &traffic.scope_logs[0].log_records[0];
        assert_eq!(traffic_record.time_unix_nano, 1_777_284_000_123_456_789);
        assert_eq!(traffic_record.severity_number, 10);
        assert_eq!(traffic_record.severity_text, "notice");
        assert_eq!(
            otlp_string_attribute(&traffic_record.attributes, "event.kind"),
            Some("event")
        );
        assert_eq!(
            otlp_string_array_attribute(&traffic_record.attributes, "event.category"),
            Some(vec!["network"])
        );
        assert_eq!(
            otlp_string_array_attribute(&traffic_record.attributes, "event.type"),
            Some(vec!["connection", "allowed"])
        );
        assert_eq!(
            otlp_string_attribute(&traffic_record.attributes, "source.ip"),
            Some("192.0.2.10")
        );
        assert_eq!(
            otlp_int_attribute(&traffic_record.attributes, "source.port"),
            Some(54321)
        );
        assert_eq!(
            traffic_record
                .attributes
                .iter()
                .filter(|attribute| attribute.key == "source.port")
                .count(),
            1
        );
        assert_eq!(
            otlp_int_attribute(&traffic_record.attributes, "destination.port"),
            Some(443)
        );
        assert_eq!(
            traffic_record
                .attributes
                .iter()
                .filter(|attribute| attribute.key == "destination.port")
                .count(),
            1
        );
        assert_eq!(
            otlp_string_attribute(&traffic_record.attributes, "network.transport"),
            Some("tcp")
        );
        assert_eq!(
            otlp_int_attribute(&traffic_record.attributes, "destination.bytes"),
            Some(512)
        );
        assert!(traffic_record.attributes.iter().all(
            |attribute| !attribute.key.starts_with("lsis.")
                && !attribute.key.starts_with("metadata.")
                && !attribute.key.starts_with("ocsf.")
        ));
        assert!(
            traffic_record
                .attributes
                .iter()
                .all(|attribute| attribute.key != "class_uid"
                    && attribute.key != "category_uid"
                    && attribute.key != "activity_id")
        );
        assert!(matches!(
            traffic_record.body.as_ref().and_then(|body| body.value.as_ref()),
            Some(any_value::Value::StringValue(body)) if body.as_bytes() == &traffic_wire[3..]
        ));

        let ips_wire = br#"<191>eventtime=1777284060987654321 date=2026-04-27 time=10:01:00 devname="fw02" devid="FGT-002" level="error" type="utm" subtype="ips" action="blocked" attack="Example exploit" attackid="4242" srcip=192.0.2.20 srcport=40000 dstip=198.51.100.20 dstport=443 proto=6 msg="blocked exploit""#;
        let ips = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "fortigate_native_otlp",
            ips_wire,
            json!({}),
            received_at,
        );
        let ips_record = &ips.scope_logs[0].log_records[0];
        assert_eq!(ips_record.time_unix_nano, 1_777_284_060_987_654_321);
        assert_eq!(ips_record.severity_number, 17);
        assert_eq!(ips_record.severity_text, "error");
        assert_eq!(
            otlp_string_attribute(&ips_record.attributes, "event.kind"),
            Some("alert")
        );
        assert_eq!(
            otlp_string_array_attribute(&ips_record.attributes, "event.category"),
            Some(vec!["intrusion_detection"])
        );
        assert_eq!(
            otlp_string_array_attribute(&ips_record.attributes, "event.type"),
            Some(vec!["denied"])
        );
        assert_eq!(
            otlp_string_attribute(&ips_record.attributes, "fortinet.attack.name"),
            Some("Example exploit")
        );
        assert_eq!(
            otlp_string_attribute(&ips_record.attributes, "fortinet.attack.id"),
            Some("4242")
        );
        assert!(
            ips_record
                .attributes
                .iter()
                .all(|attribute| attribute.key != "threat.technique.name")
        );
    }

    #[test]
    fn packaged_cloud_and_json_otlp_adapters_preserve_source_contracts() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);
        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");

        let guardduty_wire = br#"{"schemaVersion":"2.0","accountId":"123456789012","region":"us-east-1","partition":"aws","id":"finding-1","arn":"arn:aws:guardduty:us-east-1:123456789012:detector/detector-1/finding/finding-1","type":"Recon:EC2/Portscan","resource":{"resourceType":"Instance","instanceDetails":{"instanceId":"i-99999999"}},"service":{"action":{"actionType":"PORT_PROBE"},"archived":false,"count":3,"detectorId":"detector-1","eventFirstSeen":"2026-05-20T03:14:15Z","eventLastSeen":"2026-05-20T04:00:00Z"},"severity":2.0,"title":"Port scan","description":"An instance port was probed.","createdAt":"2026-05-20T03:14:16Z","updatedAt":"2026-05-20T04:00:01Z"}"#;
        let guardduty = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "guardduty_otlp",
            guardduty_wire,
            json!({}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &guardduty.resource.as_ref().expect("resource").attributes,
                "cloud.provider"
            ),
            Some("aws")
        );
        let guardduty_record = &guardduty.scope_logs[0].log_records[0];
        assert_eq!(guardduty_record.severity_number, 13);
        assert_eq!(
            otlp_string_attribute(&guardduty_record.attributes, "event.kind"),
            Some("alert")
        );
        assert_eq!(
            otlp_string_attribute(&guardduty_record.attributes, "threat.tactic.name"),
            Some("Reconnaissance")
        );
        assert_otlp_attribute_contract(&guardduty_record.attributes);

        let vpc_wire = b"5 123456789012 eni-0a1b 192.0.2.10 198.51.100.5 54321 443 6 10 4000 1714000000 1714000060 ACCEPT OK vpc-1 subnet-1 i-1 19 IPv4 192.0.2.10 198.51.100.5 ap-northeast-1 apne1-az1 - - - - egress -";
        let vpc = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "vpc_flow_otlp",
            vpc_wire,
            json!({}),
            received_at,
        );
        let vpc_record = &vpc.scope_logs[0].log_records[0];
        assert_eq!(vpc_record.time_unix_nano, 1_714_000_000_000_000_000);
        assert_eq!(vpc_record.severity_number, 0);
        assert_eq!(
            otlp_int_attribute(&vpc_record.attributes, "event.start"),
            Some(1_714_000_000_000_000_000)
        );
        assert_eq!(
            otlp_int_attribute(&vpc_record.attributes, "event.end"),
            Some(1_714_000_060_000_000_000)
        );
        assert_eq!(
            otlp_string_attribute(&vpc_record.attributes, "network.direction"),
            Some("outbound")
        );
        assert_otlp_attribute_contract(&vpc_record.attributes);

        let azure_wire = br#"{"time":"2026-04-29T10:00:00.1234567Z","resourceId":"/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Compute/virtualMachines/vm1","operationName":"MICROSOFT.COMPUTE/VIRTUALMACHINES/WRITE","category":"Administrative","level":"Informational","resultType":"Success","resultSignature":"Succeeded.","durationMs":1234,"callerIpAddress":"203.0.113.10","correlationId":"corr-1","tenantId":"tenant-1","subscriptionId":"sub-1","caller":"alice@example.com","identity":{"claims":{"oid":"user-1"}},"properties":{"statusCode":"Created"}}"#;
        let azure = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "azure_activity_otlp",
            azure_wire,
            json!({}),
            received_at,
        );
        let azure_record = &azure.scope_logs[0].log_records[0];
        assert_eq!(azure_record.severity_number, 9);
        assert_eq!(azure_record.severity_text, "Informational");
        assert_eq!(
            otlp_string_attribute(&azure_record.attributes, "user.name"),
            Some("alice@example.com")
        );
        assert_otlp_attribute_contract(&azure_record.attributes);

        let cloudtrail_wire = br#"{"eventVersion":"1.08","userIdentity":{"type":"IAMUser","userName":"alice","principalId":"AIDA1","accountId":"123"},"eventTime":"2026-04-29T10:00:00Z","eventSource":"s3.amazonaws.com","eventName":"GetObject","awsRegion":"us-east-1","sourceIPAddress":"203.0.113.10","userAgent":"aws-cli/2.x","readOnly":true,"eventID":"event-1","requestID":"request-1","eventType":"AwsApiCall","managementEvent":true,"recipientAccountId":"123","eventCategory":"Management"}"#;
        let cloudtrail = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cloudtrail_otlp",
            cloudtrail_wire,
            json!({}),
            received_at,
        );
        let cloudtrail_record = &cloudtrail.scope_logs[0].log_records[0];
        assert_eq!(cloudtrail_record.severity_number, 0);
        assert_eq!(
            otlp_string_attribute(&cloudtrail_record.attributes, "event.action"),
            Some("GetObject")
        );
        assert_eq!(
            otlp_string_attribute(&cloudtrail_record.attributes, "user.name"),
            Some("alice")
        );
        assert_otlp_attribute_contract(&cloudtrail_record.attributes);

        let k8s_wire = br#"{"kind":"Event","apiVersion":"audit.k8s.io/v1","level":"RequestResponse","auditID":"audit-1","stage":"ResponseComplete","requestURI":"/api/v1/namespaces/default/pods","verb":"create","user":{"username":"system:serviceaccount:default:deployer","uid":"abc-123","groups":["system:authenticated"]},"sourceIPs":["192.0.2.10"],"userAgent":"kubectl/v1.29.0","objectRef":{"resource":"pods","namespace":"default","apiGroup":"","apiVersion":"v1"},"responseStatus":{"code":201},"requestReceivedTimestamp":"2026-04-29T10:00:00.000000Z","stageTimestamp":"2026-04-29T10:00:00.123456Z"}"#;
        let k8s = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "k8s_audit_otlp",
            k8s_wire,
            json!({}),
            received_at,
        );
        assert!(k8s.scope_logs[0].scope.is_none());
        assert_eq!(
            otlp_string_attribute(
                &k8s.resource.as_ref().expect("resource").attributes,
                "service.name"
            ),
            Some("kube-apiserver")
        );
        let k8s_record = &k8s.scope_logs[0].log_records[0];
        assert_eq!(k8s_record.severity_number, 0);
        assert_eq!(
            otlp_int_attribute(&k8s_record.attributes, "http.response.status_code"),
            Some(201)
        );
        assert_otlp_attribute_contract(&k8s_record.attributes);

        let okta_wire = br#"{"uuid":"okta-event-1","eventType":"user.session.start","published":"2026-04-29T10:00:00.000Z","severity":"INFO","outcome":{"result":"SUCCESS"},"actor":{"id":"00u-admin","type":"User","alternateId":"admin@example.com","displayName":"Admin Example"},"client":{"ipAddress":"192.0.2.10","userAgent":{"rawUserAgent":"Mozilla/5.0"}},"target":[{"type":"User","id":"00u-target","alternateId":"alice@example.com","displayName":"Alice Example"}],"transaction":{"id":"transaction-1"}}"#;
        let okta = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "okta_system_otlp",
            okta_wire,
            json!({}),
            received_at,
        );
        let okta_record = &okta.scope_logs[0].log_records[0];
        assert_eq!(okta_record.severity_number, 9);
        assert_eq!(okta_record.severity_text, "INFO");
        assert_eq!(
            otlp_string_attribute(&okta_record.attributes, "user.name"),
            Some("admin@example.com")
        );
        assert_eq!(
            otlp_string_attribute(&okta_record.attributes, "user.target.name"),
            Some("alice@example.com")
        );
        assert_otlp_attribute_contract(&okta_record.attributes);

        let suricata_wire = br#"{"timestamp":"2026-04-29T10:00:00.123456Z","event_type":"alert","src_ip":"192.0.2.10","src_port":54321,"dest_ip":"198.51.100.5","dest_port":443,"proto":"TCP","app_proto":"tls","flow_id":42,"community_id":"1:example","in_iface":"eth0","alert":{"signature_id":1001,"signature":"Example signature","category":"Attempted Administrator Privilege Gain","severity":1,"action":"blocked"},"tls":{"sni":"example.test","version":"TLS 1.3"}}"#;
        let suricata_body: serde_json::Value = serde_json::from_slice(suricata_wire).unwrap();
        let suricata = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "suricata_otlp",
            suricata_wire,
            json!({"suricata": {"body": suricata_body, "hostname": "sensor-1"}}),
            received_at,
        );
        let suricata_record = &suricata.scope_logs[0].log_records[0];
        assert_eq!(suricata_record.severity_number, 21);
        assert_eq!(
            otlp_string_attribute(&suricata_record.attributes, "rule.name"),
            Some("Example signature")
        );
        assert_eq!(
            otlp_string_attribute(&suricata_record.attributes, "network.transport"),
            Some("tcp")
        );
        assert_otlp_attribute_contract(&suricata_record.attributes);

        for resource_logs in [guardduty, vpc, azure, cloudtrail, k8s, okta, suricata] {
            let record = &resource_logs.scope_logs[0].log_records[0];
            assert!(matches!(
                record.body.as_ref().and_then(|body| body.value.as_ref()),
                Some(any_value::Value::StringValue(_))
            ));
            assert_otlp_attribute_contract(
                &resource_logs
                    .resource
                    .as_ref()
                    .expect("resource")
                    .attributes,
            );
        }
    }

    #[test]
    fn packaged_network_security_otlp_adapters_preserve_source_contracts() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);
        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");

        let asa_wire = b"<166>Apr 27 10:00:00 fw-asa01 : %ASA-6-302013: Built outbound TCP connection 12345 for outside:198.51.100.5/443 (198.51.100.5/443) to inside:10.0.0.5/54321 (10.0.0.5/54321)";
        let asa = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "asa_otlp",
            asa_wire,
            json!({"asa": {"timezone": "UTC"}}),
            received_at,
        );
        let asa_record = &asa.scope_logs[0].log_records[0];
        assert_eq!(asa_record.severity_number, 9);
        assert_eq!(
            otlp_string_attribute(&asa_record.attributes, "network.transport"),
            Some("tcp")
        );
        assert_eq!(
            otlp_string_attribute(&asa_record.attributes, "event.code"),
            Some("302013")
        );

        // Generic transport/format chain: parse_syslog | parse_cef |
        // cef_to_otlp with no vendor stage. CEF header severity 7 sits in
        // the spec's High band (7-8) and must land on ERROR (17); the raw
        // header fields surface as event.code / cef.* attributes.
        let generic_cef_wire = b"<134>Apr 27 10:00:00 host01 CEF:0|ArcSight|Console|6.9|100|alert raised|7|src=192.0.2.10 spt=51234 dst=198.51.100.5 dpt=443 act=blocked msg=Example alert";
        let generic_cef = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cef_otlp",
            generic_cef_wire,
            json!({}),
            received_at,
        );
        let generic_cef_record = &generic_cef.scope_logs[0].log_records[0];
        assert_eq!(generic_cef_record.severity_number, 17);
        assert_eq!(generic_cef_record.severity_text, "7");
        assert_eq!(
            otlp_string_attribute(&generic_cef_record.attributes, "event.code"),
            Some("100")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_record.attributes, "cef.name"),
            Some("alert raised")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_record.attributes, "event.action"),
            Some("blocked")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_record.attributes, "source.ip"),
            Some("192.0.2.10")
        );
        assert_eq!(
            otlp_int_attribute(&generic_cef_record.attributes, "destination.port"),
            Some(443)
        );
        assert_eq!(
            otlp_string_attribute(
                &generic_cef.resource.as_ref().expect("resource").attributes,
                "observer.vendor"
            ),
            Some("ArcSight")
        );
        assert_eq!(
            otlp_string_attribute(
                &generic_cef.resource.as_ref().expect("resource").attributes,
                "host.name"
            ),
            Some("host01")
        );

        // Collision isolation: extension keys named after header fields
        // (`severity=`, `name=`, `ext=`) must not shadow the positional
        // header values — cef.parse isolates them under the nested
        // `extension` sub-object, so the record's severity still comes
        // from the header (7 → ERROR 17), not the injected `severity=9`.
        let generic_cef_collision_wire = b"<134>Apr 27 10:00:00 host01 CEF:0|ArcSight|Console|6.9|100|realname|7|severity=9 name=fakename ext=x src=192.0.2.10";
        let generic_cef_collision = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cef_otlp",
            generic_cef_collision_wire,
            json!({}),
            received_at,
        );
        let generic_cef_collision_record = &generic_cef_collision.scope_logs[0].log_records[0];
        assert_eq!(generic_cef_collision_record.severity_number, 17);
        assert_eq!(generic_cef_collision_record.severity_text, "7");
        assert_eq!(
            otlp_string_attribute(&generic_cef_collision_record.attributes, "cef.severity"),
            Some("7")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_collision_record.attributes, "cef.name"),
            Some("realname")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_collision_record.attributes, "source.ip"),
            Some("192.0.2.10")
        );

        // Header escape handling: `\|` inside a header field is a
        // literal pipe per the CEF spec, not a separator. A split on it
        // would shift severity into the extension blob and hand the
        // name fragment to the severity slot — this wire must still
        // classify from the real header severity 7 → ERROR (17).
        let generic_cef_escape_wire = b"<134>Apr 27 10:00:00 host01 CEF:0|ArcSight|Console|6.9|100|deny\\|drop|7|act=block src=192.0.2.10";
        let generic_cef_escape = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cef_otlp",
            generic_cef_escape_wire,
            json!({}),
            received_at,
        );
        let generic_cef_escape_record = &generic_cef_escape.scope_logs[0].log_records[0];
        assert_eq!(generic_cef_escape_record.severity_number, 17);
        assert_eq!(generic_cef_escape_record.severity_text, "7");
        assert_eq!(
            otlp_string_attribute(&generic_cef_escape_record.attributes, "cef.name"),
            Some("deny|drop")
        );
        assert_eq!(
            otlp_string_attribute(&generic_cef_escape_record.attributes, "event.action"),
            Some("block")
        );

        // CEF also documents string Severity values (Unknown / Low /
        // Medium / High / Very-High). A single band-wide value takes the
        // band's smallest SeverityNumber: "High" → ERROR (17).
        let generic_cef_string_wire = b"<134>Apr 27 10:00:00 host01 CEF:0|ArcSight|Console|6.9|100|alert raised|High|src=192.0.2.10 act=blocked";
        let generic_cef_string = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cef_otlp",
            generic_cef_string_wire,
            json!({}),
            received_at,
        );
        let generic_cef_string_record = &generic_cef_string.scope_logs[0].log_records[0];
        assert_eq!(generic_cef_string_record.severity_number, 17);
        assert_eq!(generic_cef_string_record.severity_text, "High");
        assert_eq!(
            otlp_string_attribute(&generic_cef_string_record.attributes, "cef.severity"),
            Some("High")
        );

        // "Unknown" is spec-valid but makes no importance claim, so
        // severity_number stays unset (proto default 0) while the raw
        // value is preserved as severity_text / cef.severity.
        let generic_cef_unknown_wire = b"<134>Apr 27 10:00:00 host01 CEF:0|ArcSight|Console|6.9|100|alert raised|Unknown|src=192.0.2.10 act=blocked";
        let generic_cef_unknown = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "cef_otlp",
            generic_cef_unknown_wire,
            json!({}),
            received_at,
        );
        let generic_cef_unknown_record = &generic_cef_unknown.scope_logs[0].log_records[0];
        assert_eq!(generic_cef_unknown_record.severity_number, 0);
        assert_eq!(generic_cef_unknown_record.severity_text, "Unknown");
        assert_eq!(
            otlp_string_attribute(&generic_cef_unknown_record.attributes, "cef.severity"),
            Some("Unknown")
        );

        let leef_wire =b"<14>1 2026-04-30T01:23:45Z cpgw01 CheckPoint - - LEEF:2.0|Check Point|VPN-1 & FireWall-1|R81|Accept|cat=Firewall\tsrc=192.0.2.10\tdst=198.51.100.5\tsrcPort=51234\tdstPort=443\tproto=tcp\trule=12\trule_name=Allow-Internet\taction=Accept\tservice=https\tusrName=alice";
        let checkpoint_leef = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "checkpoint_leef_otlp",
            leef_wire,
            json!({"checkpoint_leef": {"body": String::from_utf8_lossy(leef_wire)}}),
            received_at,
        );
        let leef_record = &checkpoint_leef.scope_logs[0].log_records[0];
        assert_eq!(leef_record.severity_number, 0);
        assert_eq!(
            otlp_string_attribute(&leef_record.attributes, "user.name"),
            Some("alice")
        );

        let checkpoint_wire = b"<134>1 2026-04-30T01:23:45Z cpgw01 CheckPoint - - [action:\"Accept\"; product:\"VPN-1 & FireWall-1\"; src:\"192.0.2.10\"; s_port:\"54321\"; dst:\"198.51.100.5\"; service:\"443\"; proto:\"tcp\"; rule_name:\"Allow\"; rule_uid:\"rule-1\"; severity:\"Informational\"]";
        let checkpoint = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "checkpoint_syslog_otlp",
            checkpoint_wire,
            json!({"checkpoint_syslog": {"body": String::from_utf8_lossy(checkpoint_wire)}}),
            received_at,
        );
        let checkpoint_record = &checkpoint.scope_logs[0].log_records[0];
        assert_eq!(checkpoint_record.severity_number, 9);
        assert_eq!(checkpoint_record.severity_text, "Informational");
        assert_eq!(
            otlp_string_attribute(&checkpoint_record.attributes, "checkpoint.rule.id"),
            Some("rule-1")
        );

        let fortigate_cef_wire = b"<129>Apr 27 10:00:00 fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|deviceExternalId=FG-EXAMPLE cat=utm:ips FTNTFGTsubtype=ips FTNTFGTseverity=high src=192.0.2.10 spt=36208 dst=198.51.100.5 dpt=9100 proto=6 act=detected FTNTFGTattack=Example-Signature FTNTFGTattackid=4242 msg=Example-detection";
        let fortigate_cef = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "fortigate_cef_otlp",
            fortigate_cef_wire,
            json!({"fortigate_cef": {"timezone": "UTC"}}),
            received_at,
        );
        let fortigate_cef_record = &fortigate_cef.scope_logs[0].log_records[0];
        assert_eq!(fortigate_cef_record.severity_number, 19);
        assert_eq!(
            otlp_string_attribute(&fortigate_cef_record.attributes, "rule.name"),
            Some("Example-Signature")
        );
        assert_eq!(
            otlp_string_attribute(&fortigate_cef_record.attributes, "event.action"),
            Some("detected")
        );

        let juniper_legacy_wire = b"<14>May 16 13:32:30 srx01 RT_IDP: IDP_ATTACK_LOG_EVENT: IDP: at 1778905950, SIG Attack log <198.51.100.10/63074->192.0.2.100/445> for TCP protocol and service SERVICE_IDP application SMB by rule Tap of rulebase IPS in policy Tap. attack: id=19519, repeat=0, action=DROP, threat-severity=HIGH, name=SMB:CVE-2017-0143-001";
        let juniper_legacy = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "juniper_legacy_otlp",
            juniper_legacy_wire,
            json!({"juniper_srx_syslog": {"body": String::from_utf8_lossy(juniper_legacy_wire), "timezone": "UTC"}}),
            received_at,
        );
        let juniper_legacy_record = &juniper_legacy.scope_logs[0].log_records[0];
        assert_eq!(juniper_legacy_record.severity_number, 19);
        assert_eq!(juniper_legacy_record.severity_text, "HIGH");
        assert_eq!(
            otlp_string_attribute(&juniper_legacy_record.attributes, "rule.id"),
            Some("19519")
        );

        let juniper_sd_wire = b"<134>1 2026-04-30T01:23:45Z srx01 RT_FLOW - RT_FLOW_SESSION_CREATE [junos@2636.1.1.1.2.39 source-address=\"192.0.2.10\" source-port=\"54321\" destination-address=\"198.51.100.5\" destination-port=\"443\" protocol-id=\"6\" policy-name=\"allow-web\" source-zone-name=\"trust\" destination-zone-name=\"untrust\" session-id-32=\"123\" application=\"HTTPS\"]";
        let juniper_structured = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "juniper_structured_otlp",
            juniper_sd_wire,
            json!({"juniper_srx_sd_syslog": {"body": String::from_utf8_lossy(juniper_sd_wire)}}),
            received_at,
        );
        let juniper_sd_record = &juniper_structured.scope_logs[0].log_records[0];
        assert_eq!(juniper_sd_record.severity_number, 0);
        assert_eq!(
            otlp_string_attribute(&juniper_sd_record.attributes, "juniper.session.id"),
            Some("123")
        );

        let juniper_secintel_wire = b"<134>1 2026-04-30T01:23:45Z srx01 RT_SECINTEL - SECINTEL_ACTION_LOG [junos@2636 source-address=\"192.0.2.10\" source-port=\"54321\" destination-address=\"198.51.100.5\" destination-port=\"443\" protocol-id=\"6\" action=\"BLOCK\" feed-name=\"Example feed\" threat-severity=\"7\"]";
        let juniper_secintel = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "juniper_structured_otlp",
            juniper_secintel_wire,
            json!({"juniper_srx_sd_syslog": {"body": String::from_utf8_lossy(juniper_secintel_wire)}}),
            received_at,
        );
        let juniper_secintel_record = &juniper_secintel.scope_logs[0].log_records[0];
        assert_eq!(juniper_secintel_record.severity_number, 19);
        assert_eq!(juniper_secintel_record.severity_text, "");
        assert_eq!(
            otlp_string_attribute(
                &juniper_secintel_record.attributes,
                "juniper.secintel.severity"
            ),
            Some("7")
        );

        let nsp_wire = b"admin_domain=Default alert_id=12345 alert_type=Signature app_protocol=HTTP confidence=Tentative attack_count=1 attack_id=0x40000123 attack_name=SQL-Injection severity=High alert_signature=HTTP-SQL-INJ-001 attack_time=\"2026-05-16 10:00:00\" category=Application dest_ip=192.0.2.10 dest_name=webserver01 dest_port=80 device_name=nsp-sensor-01 direction=Inbound confidence= file_name= file_hash= file_type= virus_name= action_status= error_status= protocol=TCP result=Blocked src_ip=198.51.100.5 src_name= src_port=54321";
        let nsp = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "nsp_otlp",
            nsp_wire,
            json!({"nsp": {"body": String::from_utf8_lossy(nsp_wire), "timezone": "UTC"}}),
            received_at,
        );
        let nsp_record = &nsp.scope_logs[0].log_records[0];
        assert_eq!(nsp_record.severity_number, 19);
        assert_eq!(nsp_record.severity_text, "High");
        assert_eq!(
            otlp_string_attribute(&nsp_record.attributes, "rule.id"),
            Some("0x40000123")
        );
        assert_eq!(
            otlp_string_attribute(&nsp_record.attributes, "event.id"),
            Some("12345")
        );

        let paloalto_cef_wire = b"<134>Apr 27 10:00:00 fw-pan01 CEF:0|Palo Alto Networks|PAN-OS|10.2.0|end|TRAFFIC|3|src=192.0.2.10 spt=54321 dst=198.51.100.5 dpt=443 proto=tcp act=allow in=2048 out=1024 app=ssl suser=alice";
        let paloalto_cef = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "paloalto_cef_otlp",
            paloalto_cef_wire,
            json!({"paloalto_cef": {"timezone": "UTC"}}),
            received_at,
        );
        let paloalto_cef_record = &paloalto_cef.scope_logs[0].log_records[0];
        assert_eq!(paloalto_cef_record.severity_number, 17);
        assert_eq!(
            otlp_string_attribute(&paloalto_cef_record.attributes, "user.name"),
            Some("alice")
        );

        let mut pan_fields = vec![""; 53];
        pan_fields[1] = "2026/04/27 10:00:00";
        pan_fields[2] = "012345678";
        pan_fields[3] = "TRAFFIC";
        pan_fields[4] = "end";
        pan_fields[6] = "2026/04/27 10:00:01";
        pan_fields[7] = "192.0.2.10";
        pan_fields[8] = "198.51.100.5";
        pan_fields[11] = "allow-web";
        pan_fields[12] = "alice";
        pan_fields[14] = "ssl";
        pan_fields[16] = "trust";
        pan_fields[17] = "untrust";
        pan_fields[22] = "98765";
        pan_fields[24] = "54321";
        pan_fields[25] = "443";
        pan_fields[29] = "tcp";
        pan_fields[30] = "allow";
        pan_fields[32] = "1024";
        pan_fields[33] = "2048";
        pan_fields[44] = "10";
        pan_fields[45] = "20";
        pan_fields[52] = "fw-pan01";
        let paloalto_native_wire =
            format!("<134>Apr 27 10:00:00 fw-pan01 {}", pan_fields.join(","));
        let paloalto_native = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "paloalto_native_otlp",
            paloalto_native_wire.as_bytes(),
            json!({"paloalto_syslog": {"timezone": "UTC"}}),
            received_at,
        );
        let paloalto_native_record = &paloalto_native.scope_logs[0].log_records[0];
        assert_eq!(paloalto_native_record.severity_number, 0);
        assert_eq!(
            otlp_string_attribute(&paloalto_native_record.attributes, "rule.name"),
            Some("allow-web")
        );
        assert_eq!(
            otlp_string_attribute(&paloalto_native_record.attributes, "palo_alto.session.id"),
            Some("98765")
        );
        assert_eq!(
            otlp_int_attribute(&paloalto_native_record.attributes, "source.bytes"),
            Some(1024)
        );
        assert_eq!(
            otlp_int_attribute(&paloalto_native_record.attributes, "destination.bytes"),
            Some(2048)
        );

        for resource_logs in [
            asa,
            generic_cef,
            generic_cef_escape,
            generic_cef_collision,
            generic_cef_string,
            generic_cef_unknown,
            checkpoint_leef,
            checkpoint,
            fortigate_cef,
            juniper_legacy,
            juniper_structured,
            juniper_secintel,
            nsp,
            paloalto_cef,
            paloalto_native,
        ] {
            let record = &resource_logs.scope_logs[0].log_records[0];
            assert!(matches!(
                record.body.as_ref().and_then(|body| body.value.as_ref()),
                Some(any_value::Value::StringValue(_))
            ));
            assert_otlp_attribute_contract(&record.attributes);
            assert_otlp_attribute_contract(
                &resource_logs
                    .resource
                    .as_ref()
                    .expect("resource")
                    .attributes,
            );
        }
    }

    #[test]
    fn packaged_host_transport_and_zeek_otlp_adapters_preserve_source_contracts() {
        use crate::functions::{FunctionRegistry, register_builtins, register_user_functions};
        use opentelemetry_proto::tonic::common::v1::any_value;
        use serde_json::json;

        let cfg = compile_packaged_otlp_composers().unwrap();
        let mut funcs = FunctionRegistry::new();
        register_builtins(&mut funcs, crate::runtime::init_tables(&cfg).unwrap());
        register_user_functions(&mut funcs, &cfg);
        let received_at = chrono::DateTime::from_timestamp(1_784_073_600, 123_456_789)
            .expect("valid receive timestamp");

        let auditd_wire = b"type=USER_LOGIN msg=audit(1710000000.123:42): pid=100 uid=0 auid=1000 ses=1 msg='op=login acct=alice exe=/usr/bin/login hostname=? addr=192.0.2.10 terminal=tty1 res=success'";
        let auditd = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "auditd_otlp",
            auditd_wire,
            json!({"auditd": {"body": String::from_utf8_lossy(auditd_wire), "hostname": "host-1"}}),
            received_at,
        );
        assert!(auditd.scope_logs[0].scope.is_none());
        assert_eq!(
            otlp_string_attribute(
                &auditd.resource.as_ref().expect("resource").attributes,
                "service.name"
            ),
            Some("auditd")
        );

        let bind_wire = b"30-Apr-2026 01:23:45.123 client @0x7fef10003330 192.0.2.10#54321 (example.com): query: example.com IN A +E(0) (198.51.100.53)";
        let bind = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "bind_otlp",
            bind_wire,
            json!({"bind": {"body": String::from_utf8_lossy(bind_wire), "hostname": "dns-1", "pid": "123", "timezone": "UTC"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &bind.scope_logs[0].log_records[0].attributes,
                "dns.question.name"
            ),
            Some("example.com")
        );

        let combined_wire = b"192.0.2.10 - alice [29/Apr/2026:13:55:36 +0900] \"GET /index.html HTTP/1.1\" 200 2326 \"https://example.com/\" \"Mozilla/5.0\"";
        let combined = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "combined_log_otlp",
            combined_wire,
            json!({"combined_log": {"body": String::from_utf8_lossy(combined_wire), "hostname": "web-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_int_attribute(
                &combined.scope_logs[0].log_records[0].attributes,
                "http.response.status_code"
            ),
            Some(200)
        );

        let journald_wire = br#"{"MESSAGE":"accepted","PRIORITY":"6","SYSLOG_FACILITY":"4","SYSLOG_IDENTIFIER":"sshd","_PID":"42","_UID":"1000","_HOSTNAME":"host-1","_SYSTEMD_UNIT":"sshd.service","_TRANSPORT":"syslog"}"#;
        let journald = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "journald_otlp",
            journald_wire,
            json!({}),
            received_at,
        );
        let journald_record = &journald.scope_logs[0].log_records[0];
        assert_eq!(journald_record.severity_number, 0);
        assert_eq!(
            otlp_string_array_attribute(&journald_record.attributes, "event.category"),
            Some(vec!["host"])
        );
        assert_eq!(
            otlp_string_array_attribute(&journald_record.attributes, "event.type"),
            Some(vec!["info"])
        );
        assert_eq!(
            otlp_int_attribute(&journald_record.attributes, "log.syslog.severity.code"),
            Some(6)
        );

        let openssh_wire =
            b"Accepted publickey for alice from 192.0.2.10 port 54321 ssh2: RSA SHA256:abcd";
        let openssh = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "openssh_otlp",
            openssh_wire,
            json!({"openssh": {"body": String::from_utf8_lossy(openssh_wire), "hostname": "host-1", "pid": "42", "time": 1710000000123456789i64}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &openssh.scope_logs[0].log_records[0].attributes,
                "user.name"
            ),
            Some("alice")
        );

        let postfix_wire = b"postfix/qmgr[1111]: ABCDEF1234: from=<bob@example.org>, size=3222, nrcpt=1 (queue active)";
        let postfix = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "postfix_otlp",
            postfix_wire,
            json!({"postfix": {"body": String::from_utf8_lossy(postfix_wire), "hostname": "mail-1", "time": 1710000000123456789i64}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &postfix.scope_logs[0].log_records[0].attributes,
                "email.message_id"
            ),
            Some("ABCDEF1234")
        );

        let sudo_wire =
            b"pam_unix(sudo:session): session opened for user root(uid=0) by alice(uid=1000)";
        let sudo = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "sudo_otlp",
            sudo_wire,
            json!({"sudo": {"body": String::from_utf8_lossy(sudo_wire), "hostname": "host-1", "pid": "99", "time": 1710000000123456789i64}}),
            received_at,
        );
        let sudo_record = &sudo.scope_logs[0].log_records[0];
        assert_eq!(
            otlp_string_attribute(&sudo_record.attributes, "user.name"),
            Some("alice")
        );
        assert_eq!(
            otlp_string_attribute(&sudo_record.attributes, "user.target.name"),
            Some("root")
        );

        let syslog_wire = b"<165>1 2026-04-30T01:23:45Z host-1 app 42 ID47 - message";
        let syslog = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "syslog_otlp",
            syslog_wire,
            json!({}),
            received_at,
        );
        let syslog_record = &syslog.scope_logs[0].log_records[0];
        assert_eq!(syslog_record.severity_number, 0);
        assert_eq!(
            otlp_string_array_attribute(&syslog_record.attributes, "event.category"),
            None
        );
        assert_eq!(
            otlp_string_array_attribute(&syslog_record.attributes, "event.type"),
            None
        );
        assert_eq!(
            otlp_int_attribute(&syslog_record.attributes, "log.syslog.severity.code"),
            Some(5)
        );

        let sysmon_body = json!({
            "EventID": 3,
            "Channel": "Microsoft-Windows-Sysmon/Operational",
            "EventTime": "2026-04-29T10:00:00.123456Z",
            "Computer": "WORKSTATION1",
            "EventData": {"UtcTime":"2026-04-29 10:00:00.123","ProcessId":"42","Image":"C:\\\\Windows\\\\app.exe","User":"CORP\\\\alice","Protocol":"tcp","SourceIp":"192.0.2.10","SourcePort":"54321","DestinationIp":"198.51.100.5","DestinationPort":"443","Initiated":"true"}
        });
        let sysmon_wire = serde_json::to_vec(&sysmon_body).unwrap();
        let sysmon = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "sysmon_otlp",
            &sysmon_wire,
            json!({"sysmon": {"body": sysmon_body}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &sysmon.scope_logs[0].log_records[0].attributes,
                "event.code"
            ),
            Some("3")
        );

        let winevent_wire = br#"{"EventID":4624,"Channel":"Security","EventType":"INFO","Severity":"INFO","Hostname":"WORKSTATION1","EventTime":"2026-04-29 10:00:00","TargetUserName":"alice","TargetDomainName":"CORP","TargetUserSid":"S-1-5-21-1001","SubjectUserName":"SYSTEM","SubjectDomainName":"NT AUTHORITY","LogonType":"3","IpAddress":"192.0.2.10","IpPort":"54321"}"#;
        let winevent = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "winevent_otlp",
            winevent_wire,
            json!({}),
            received_at,
        );
        let winevent_record = &winevent.scope_logs[0].log_records[0];
        assert_eq!(winevent_record.severity_number, 9);
        assert_eq!(
            otlp_string_attribute(&winevent_record.attributes, "user.target.name"),
            Some("CORP\\alice")
        );

        let zeek_conn_body = json!({"_path":"conn","ts":1710000000.125,"uid":"C1","id":{"orig_h":"192.0.2.10","orig_p":54321,"resp_h":"198.51.100.5","resp_p":443},"proto":"tcp","service":"ssl","duration":1.5,"orig_bytes":100,"resp_bytes":200,"orig_pkts":2,"resp_pkts":3,"conn_state":"SF"});
        let zeek_conn_wire = serde_json::to_vec(&zeek_conn_body).unwrap();
        let zeek_default = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "zeek_default_otlp",
            &zeek_conn_wire,
            json!({"zeek": {"body": zeek_conn_body, "hostname": "sensor-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &zeek_default.scope_logs[0].log_records[0].attributes,
                "event.code"
            ),
            Some("conn")
        );

        let zeek_ssh_body = json!({"_path":"ssh","ts":1710000000.25,"uid":"C2","id":{"orig_h":"192.0.2.10","orig_p":54321,"resp_h":"198.51.100.5","resp_p":22},"auth_success":true,"client":"OpenSSH","server":"OpenSSH","cipher_alg":"aes256-gcm"});
        let zeek_ssh_wire = serde_json::to_vec(&zeek_ssh_body).unwrap();
        let zeek_soc = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "zeek_soc_otlp",
            &zeek_ssh_wire,
            json!({"zeek": {"body": zeek_ssh_body, "hostname": "sensor-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &zeek_soc.scope_logs[0].log_records[0].attributes,
                "event.code"
            ),
            Some("ssh")
        );

        let zeek_custom_body = json!({"_path":"custom_protocol","ts":1710000000.5,"uid":"C3","id":{"orig_h":"192.0.2.10","orig_p":1000,"resp_h":"198.51.100.5","resp_p":2000},"proto":"tcp","custom":"kept"});
        let zeek_custom_wire = serde_json::to_vec(&zeek_custom_body).unwrap();
        let zeek_full = run_packaged_otlp_resource_logs_at(
            &cfg,
            &funcs,
            "zeek_full_otlp",
            &zeek_custom_wire,
            json!({"zeek": {"body": zeek_custom_body, "hostname": "sensor-1"}}),
            received_at,
        );
        assert_eq!(
            otlp_string_attribute(
                &zeek_full.scope_logs[0].log_records[0].attributes,
                "event.code"
            ),
            Some("custom_protocol")
        );

        for resource_logs in [
            auditd,
            bind,
            combined,
            journald,
            openssh,
            postfix,
            sudo,
            syslog,
            sysmon,
            winevent,
            zeek_default,
            zeek_soc,
            zeek_full,
        ] {
            let record = &resource_logs.scope_logs[0].log_records[0];
            assert!(matches!(
                record.body.as_ref().and_then(|body| body.value.as_ref()),
                Some(any_value::Value::StringValue(_))
            ));
            assert_otlp_attribute_contract(&record.attributes);
            assert_otlp_attribute_contract(
                &resource_logs
                    .resource
                    .as_ref()
                    .expect("resource")
                    .attributes,
            );
        }
    }
}

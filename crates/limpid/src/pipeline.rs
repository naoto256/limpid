//! Pipeline engine: compiles DSL definitions into an executable pipeline
//! and runs events through process chains.

use std::collections::HashMap;

use anyhow::{Result, bail};
use serde_json::Value;
use tracing::trace;

use crate::dsl::ast::*;
use crate::dsl::eval::{eval_expr, is_truthy, values_match};
use crate::dsl::exec::{ExecResult, ProcessRegistry, exec_process_body};
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::modules::{ModuleRegistry, ProcessError};
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
    pub global_blocks: HashMap<String, Vec<Property>>,
}

impl CompiledConfig {
    pub fn from_config(config: Config) -> Result<Self> {
        let mut inputs = HashMap::new();
        let mut outputs = HashMap::new();
        let mut processes = HashMap::new();
        let mut pipelines = HashMap::new();
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
            global_blocks,
        };
        Ok(compiled)
    }

    /// Validate cross-references: all referenced inputs, outputs, and processes exist.
    pub fn validate(&self, builtins: &ModuleRegistry) -> Result<()> {
        for (name, pipeline) in &self.pipelines {
            for stmt in &pipeline.body {
                self.validate_pipeline_stmt(name, stmt, builtins)?;
            }
        }
        Ok(())
    }

    fn validate_pipeline_stmt(&self, pipeline_name: &str, stmt: &PipelineStatement, builtins: &ModuleRegistry) -> Result<()> {
        match stmt {
            PipelineStatement::Input(input_name) => {
                if !self.inputs.contains_key(input_name) {
                    bail!("pipeline '{}': references unknown input '{}'", pipeline_name, input_name);
                }
            }
            PipelineStatement::Output(output_name) => {
                if !self.outputs.contains_key(output_name) {
                    bail!("pipeline '{}': references unknown output '{}'", pipeline_name, output_name);
                }
            }
            PipelineStatement::ProcessChain(chain) => {
                for element in chain {
                    if let ProcessChainElement::Named(proc_name, _) = element
                        && !self.processes.contains_key(proc_name)
                            && !builtins.is_builtin_process(proc_name)
                        {
                            bail!(
                                "pipeline '{}': references unknown process '{}'",
                                pipeline_name, proc_name
                            );
                        }
                }
            }
            PipelineStatement::If(if_chain) => {
                for (_, body) in &if_chain.branches {
                    for item in body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s, builtins)?;
                        }
                    }
                }
                if let Some(else_body) = &if_chain.else_body {
                    for item in else_body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s, builtins)?;
                        }
                    }
                }
            }
            PipelineStatement::Switch(_, arms) => {
                for arm in arms {
                    for item in &arm.body {
                        if let BranchBody::Pipeline(s) = item {
                            self.validate_pipeline_stmt(pipeline_name, s, builtins)?;
                        }
                    }
                }
            }
            PipelineStatement::Drop | PipelineStatement::Finish => {}
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
}

/// Result of running an event through a pipeline.
pub struct PipelineRunResult {
    pub trace: Vec<TraceEntry>,
    pub outputs: Vec<(String, Event)>,
    pub termination: PipelineTermination,
}

/// A process registry backed by compiled DSL process definitions.
struct DslProcessRegistry<'a> {
    processes: &'a HashMap<String, ProcessDef>,
    builtins: &'a ModuleRegistry,
    funcs: &'a FunctionRegistry,
    tap: Option<&'a TapRegistry>,
}

impl<'a> DslProcessRegistry<'a> {
    fn new(
        processes: &'a HashMap<String, ProcessDef>,
        builtins: &'a ModuleRegistry,
        funcs: &'a FunctionRegistry,
        tap: Option<&'a TapRegistry>,
    ) -> Self {
        Self { processes, builtins, funcs, tap }
    }
}

impl ProcessRegistry for DslProcessRegistry<'_> {
    fn call(
        &self,
        name: &str,
        args: &[Value],
        event: Event,
    ) -> std::result::Result<Option<Event>, ProcessError> {
        // 1. Built-in process modules
        if self.builtins.is_builtin_process(name) {
            trace!("process '{}' (builtin): executing", name);
            let result = self.builtins.call_process(name, args, event);
            match &result {
                Ok(e) => {
                    trace!("process '{}': ok, fields={:?}", name, e.fields.keys().collect::<Vec<_>>());
                    self.emit_tap(name, e);
                }
                Err(e) => trace!("process '{}': error: {}", name, e),
            }
            return result.map(Some);
        }

        // 2. User-defined DSL processes
        if let Some(process_def) = self.processes.get(name) {
            trace!("process '{}' (user-defined): executing", name);
            return match exec_process_body(&process_def.body, event, self, self.funcs) {
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

        // 3. Unknown process — warn and pass through
        tracing::warn!("unknown process '{}', passing event through unchanged", name);
        Ok(Some(event))
    }
}

impl DslProcessRegistry<'_> {
    fn emit_tap(&self, process_name: &str, event: &Event) {
        if let Some(tap) = self.tap {
            let key = format!("process {}", process_name);
            tap.try_emit(&key, event);
        }
    }
}

/// Run a single event through a pipeline definition.
pub fn run_pipeline(
    pipeline: &PipelineDef,
    event: Event,
    config: &CompiledConfig,
    builtins: &ModuleRegistry,
    funcs: &FunctionRegistry,
    tap: Option<&TapRegistry>,
) -> Result<PipelineRunResult> {
    let registry = DslProcessRegistry::new(&config.processes, builtins, funcs, tap);
    let mut trace_entries = Vec::new();
    let mut outputs = Vec::new();

    // Log initial state
    trace_entries.push(TraceEntry {
        stage: "input".into(),
        label: String::new(),
        detail: format!("raw: {}", String::from_utf8_lossy(&event.raw)),
    });

    let (_, termination) = exec_pipeline_body(&pipeline.body, event, &registry, funcs, &mut trace_entries, &mut outputs)?;

    Ok(PipelineRunResult {
        trace: trace_entries,
        outputs,
        termination,
    })
}

/// Execute a pipeline body (sequence of pipeline statements).
/// Returns (remaining event if any, how the pipeline terminated).
fn exec_pipeline_body(
    stmts: &[PipelineStatement],
    mut event: Event,
    registry: &DslProcessRegistry,
    funcs: &FunctionRegistry,
    trace: &mut Vec<TraceEntry>,
    outputs: &mut Vec<(String, Event)>,
) -> Result<(Option<Event>, PipelineTermination)> {
    for stmt in stmts {
        match exec_pipeline_stmt(stmt, event, registry, funcs, trace, outputs)? {
            (Some(e), _) => event = e,
            (None, term) => return Ok((None, term)),
        }
    }
    Ok((Some(event), PipelineTermination::Finished))
}

fn exec_pipeline_stmt(
    stmt: &PipelineStatement,
    event: Event,
    registry: &DslProcessRegistry,
    funcs: &FunctionRegistry,
    trace: &mut Vec<TraceEntry>,
    outputs: &mut Vec<(String, Event)>,
) -> Result<(Option<Event>, PipelineTermination)> {
    let cont = |event| Ok((Some(event), PipelineTermination::Finished));
    let dropped = || Ok((None, PipelineTermination::Dropped));
    let finished = || Ok((None, PipelineTermination::Finished));

    match stmt {
        PipelineStatement::Input(_) => {
            cont(event)
        }

        PipelineStatement::ProcessChain(chain) => {
            let mut current = event;
            for element in chain {
                match element {
                    ProcessChainElement::Named(name, args) => {
                        let evaluated_args: Vec<Value> = args
                            .iter()
                            .map(|a| eval_expr(a, &current, funcs))
                            .collect::<Result<Vec<_>>>()?;

                        let backup = current.clone();
                        match registry.call(name, &evaluated_args, current) {
                            Ok(Some(e)) => {
                                trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: if args.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{}({})", name, evaluated_args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "))
                                    },
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            Ok(None) => {
                                trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                            Err(e) => {
                                tracing::warn!("process '{}': {} — event passed through unchanged", name, e);
                                trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: name.clone(),
                                    detail: format!("error: {} (ignored)", e),
                                });
                                current = backup;
                            }
                        }
                    }
                    ProcessChainElement::Inline(body) => {
                        match exec_process_body(body, current, registry, funcs)? {
                            ExecResult::Continue(e) => {
                                trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "ok".into(),
                                });
                                current = e;
                            }
                            ExecResult::Dropped => {
                                trace.push(TraceEntry {
                                    stage: "process".into(),
                                    label: "(inline)".into(),
                                    detail: "dropped".into(),
                                });
                                return dropped();
                            }
                        }
                    }
                }
            }
            cont(current)
        }

        PipelineStatement::Output(name) => {
            trace!(target: "limpid::pipeline", "output → {}", name);
            trace.push(TraceEntry {
                stage: "output".into(),
                label: format!("→ {}", name),
                detail: String::new(),
            });
            outputs.push((name.clone(), event.clone()));
            cont(event)
        }

        PipelineStatement::Drop => {
            trace!(target: "limpid::pipeline", "drop");
            trace.push(TraceEntry {
                stage: "drop".into(),
                label: String::new(),
                detail: String::new(),
            });
            dropped()
        }

        PipelineStatement::Finish => {
            trace!(target: "limpid::pipeline", "finish");
            trace.push(TraceEntry {
                stage: "finish".into(),
                label: String::new(),
                detail: String::new(),
            });
            finished()
        }

        PipelineStatement::If(if_chain) => {
            exec_pipeline_if(if_chain, event, registry, funcs, trace, outputs)
        }

        PipelineStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr(discriminant, &event, funcs)?;
            for arm in arms {
                if arm.pattern.is_none() {
                    return exec_pipeline_branch_body(&arm.body, event, registry, funcs, trace, outputs);
                }
                let pattern_val = eval_expr(arm.pattern.as_ref().unwrap(), &event, funcs)?;
                if values_match(&disc_val, &pattern_val) {
                    return exec_pipeline_branch_body(&arm.body, event, registry, funcs, trace, outputs);
                }
            }
            cont(event)
        }
    }
}

fn exec_pipeline_if(
    if_chain: &IfChain,
    event: Event,
    registry: &DslProcessRegistry,
    funcs: &FunctionRegistry,
    trace: &mut Vec<TraceEntry>,
    outputs: &mut Vec<(String, Event)>,
) -> Result<(Option<Event>, PipelineTermination)> {
    for (condition, body) in &if_chain.branches {
        let cond_val = eval_expr(condition, &event, funcs)?;
        if is_truthy(&cond_val) {
            return exec_pipeline_branch_body(body, event, registry, funcs, trace, outputs);
        }
    }
    if let Some(else_body) = &if_chain.else_body {
        return exec_pipeline_branch_body(else_body, event, registry, funcs, trace, outputs);
    }
    Ok((Some(event), PipelineTermination::Finished))
}

fn exec_pipeline_branch_body(
    body: &[BranchBody],
    mut event: Event,
    registry: &DslProcessRegistry,
    funcs: &FunctionRegistry,
    trace: &mut Vec<TraceEntry>,
    outputs: &mut Vec<(String, Event)>,
) -> Result<(Option<Event>, PipelineTermination)> {
    for item in body {
        match item {
            BranchBody::Pipeline(stmt) => {
                match exec_pipeline_stmt(stmt, event, registry, funcs, trace, outputs)? {
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

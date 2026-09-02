//! Single execution-IR compiler and immutable runtime blueprint.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::dsl::ast::{
    AssignTarget, BranchBody, Expr, FunctionDef, InputDef, OutputDef, PipelineStatement,
    ProcessChainElement, ProcessStatement, Property,
};

use super::CompiledConfig;

#[cfg(test)]
std::thread_local! {
    static PIPELINE_BY_ID_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BOUND_PIPELINE_EXECUTION_CONSTRUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProcessBodyId(u32);

impl ProcessBodyId {
    fn from_index(index: usize) -> Result<Self> {
        Ok(Self(u32::try_from(index).map_err(|_| {
            anyhow::anyhow!("process body count exceeds the execution IR limit")
        })?))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EdgeSlot(u32);

impl EdgeSlot {
    fn from_index(index: usize) -> Result<Self> {
        Ok(Self(u32::try_from(index).map_err(|_| {
            anyhow::anyhow!("process edge count exceeds the execution IR limit")
        })?))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MetricNodeId(u32);

impl MetricNodeId {
    fn from_index(index: usize) -> Result<Self> {
        Ok(Self(u32::try_from(index).map_err(|_| {
            anyhow::anyhow!("metric node count exceeds the execution IR limit")
        })?))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OutputTimerSlot(u32);

impl OutputTimerSlot {
    fn from_index(index: usize) -> Result<Self> {
        Ok(Self(u32::try_from(index).map_err(|_| {
            anyhow::anyhow!("output timer count exceeds the execution IR limit")
        })?))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PipelineId(u32);

impl PipelineId {
    fn from_index(index: usize) -> Result<Self> {
        Ok(Self(u32::try_from(index).map_err(|_| {
            anyhow::anyhow!("pipeline count exceeds the execution IR limit")
        })?))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessEdge {
    pub(crate) name: String,
    pub(crate) target: ProcessTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessTarget {
    Known(ProcessBodyId),
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessBodyCode {
    pub(crate) name: String,
    pub(crate) code: Vec<ProcessCode>,
    pub(crate) edges: Vec<ProcessEdge>,
}

#[derive(Clone, Debug)]
pub(crate) enum ProcessCode {
    Assign(AssignTarget, Expr),
    LetBinding(String, Expr),
    Call {
        name: String,
        edge_slot: EdgeSlot,
    },
    Drop,
    Error(Option<Expr>),
    If {
        branches: Vec<(Expr, Vec<ProcessCode>)>,
        else_body: Option<Vec<ProcessCode>>,
    },
    Switch {
        discriminant: Expr,
        arms: Vec<(Option<Expr>, Vec<ProcessCode>)>,
    },
    TryCatch {
        try_body: Vec<ProcessCode>,
        catch_body: Vec<ProcessCode>,
    },
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessSite {
    pub(crate) name: String,
    pub(crate) kind: SiteKind,
    pub(crate) body: ProcessBodyId,
    pub(crate) metric_node: MetricNodeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SiteKind {
    Named,
    Inline,
}

#[derive(Clone, Debug)]
pub(crate) enum PipelineCode {
    Input(Vec<String>),
    ProcessChain(Vec<ProcessSite>),
    Output {
        name: String,
        timer_slot: OutputTimerSlot,
    },
    Drop,
    Finish,
    Error(Option<Expr>),
    If {
        branches: Vec<(Expr, Vec<PipelineCode>)>,
        else_body: Option<Vec<PipelineCode>>,
    },
    Switch {
        discriminant: Expr,
        arms: Vec<(Option<Expr>, Vec<PipelineCode>)>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct MetricNodeDescriptor {
    pub(crate) parent: Option<MetricNodeId>,
    pub(crate) step: u32,
    pub(crate) process_path: String,
    pub(crate) process_name: String,
    pub(crate) target: ProcessTarget,
    pub(crate) children: Vec<MetricNodeId>,
}

#[derive(Clone, Debug)]
pub(crate) struct PipelineBlueprint {
    pub(crate) name: String,
    pub(crate) code: Vec<PipelineCode>,
    pub(crate) flow: PipelineFlow,
    /// Runtime subscription authority intentionally preserves the legacy
    /// top-level-first `input` contract. `flow.inputs` is the recursive union
    /// exposed by control-list JSON; unifying the two is a separate decision.
    pub(crate) subscription_inputs: Vec<String>,
    pub(crate) metric_nodes: Vec<MetricNodeDescriptor>,
    pub(crate) output_timers: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PipelineFlow {
    pub(crate) inputs: Vec<String>,
    pub(crate) processes: Vec<String>,
    pub(crate) outputs: Vec<String>,
}

/// Immutable, unbound authority produced before any registry, counter,
/// socket, task, table, or module resource is created.
#[derive(Clone)]
pub(crate) struct UnsealedRuntimeBlueprint {
    pub(crate) node_id: Option<String>,
    pub(crate) node_key: Option<String>,
    pub(crate) inputs: HashMap<String, InputDef>,
    pub(crate) outputs: HashMap<String, OutputDef>,
    pub(crate) functions: HashMap<String, FunctionDef>,
    pub(crate) global_blocks: HashMap<String, Vec<Property>>,
    pub(crate) process_bodies: Vec<ProcessBodyCode>,
    pub(crate) process_names: BTreeMap<String, ProcessBodyId>,
    /// Declarative IR ownership only. The Arc lets per-start bound execution
    /// descriptors share these sealed bytes without cloning pipeline code; it
    /// is not a registry, counter, task, socket, table, or resource handle.
    pub(crate) pipelines: BTreeMap<String, Arc<PipelineBlueprint>>,
    pub(crate) pipeline_ids: BTreeMap<String, PipelineId>,
    pub(crate) pipeline_order: Vec<String>,
}

/// Sealed form. C2 validates every id, slot, range, cycle, identity, and
/// metric series descriptor before constructing this value.
#[derive(Clone)]
pub(crate) struct RuntimeBlueprint {
    unsealed: UnsealedRuntimeBlueprint,
}

/// Per-start metric binding. The immutable blueprint remains free of registry
/// and counter handles; every runtime start builds this value from its fresh
/// registry before acquiring modules, sockets, tables, or tasks.
pub(crate) struct BoundRuntimeBlueprint {
    pub(crate) blueprint: Arc<RuntimeBlueprint>,
    pipelines: Vec<Arc<BoundPipelineExecution>>,
}

#[derive(Clone)]
pub(crate) struct BoundPipelineMetrics {
    pub(crate) process_counters: Vec<crate::metrics::ProcessCounters>,
    pub(crate) output_timers: Vec<crate::metrics::PipelineOutputTimer>,
}

/// Startup-resolved execution authority for one pipeline. Workers share this
/// immutable descriptor, so event dispatch never re-enters identity or metric
/// maps and never repeats binding-shape validation.
pub(crate) struct BoundPipelineExecution {
    blueprint: Arc<RuntimeBlueprint>,
    pipeline: Arc<PipelineBlueprint>,
    metrics: BoundPipelineMetrics,
}

pub(crate) fn compile_runtime_blueprint(config: &CompiledConfig) -> Result<Arc<RuntimeBlueprint>> {
    Ok(Arc::new(UnsealedRuntimeBlueprint::compile(config)?.seal()?))
}

impl RuntimeBlueprint {
    pub(crate) fn process_bodies(&self) -> &[ProcessBodyCode] {
        &self.unsealed.process_bodies
    }

    pub(crate) fn process_body_inventory(&self) -> impl Iterator<Item = (&str, SiteKind)> {
        self.unsealed
            .process_bodies
            .iter()
            .enumerate()
            .map(|(index, body)| {
                let kind = if self
                    .unsealed
                    .process_names
                    .values()
                    .any(|id| id.index() == index)
                {
                    SiteKind::Named
                } else {
                    SiteKind::Inline
                };
                (body.name.as_str(), kind)
            })
    }

    #[cfg(test)]
    pub(crate) fn pipeline(&self, name: &str) -> Option<&PipelineBlueprint> {
        self.unsealed.pipelines.get(name).map(Arc::as_ref)
    }

    pub(crate) fn pipeline_id(&self, name: &str) -> Option<PipelineId> {
        self.unsealed.pipeline_ids.get(name).copied()
    }

    #[cfg(test)]
    #[allow(dead_code)] // Mutant seam: a restored event-path lookup increments the observer.
    pub(crate) fn pipeline_by_id(&self, id: PipelineId) -> Option<&PipelineBlueprint> {
        #[cfg(test)]
        PIPELINE_BY_ID_CALLS.with(|calls| calls.set(calls.get() + 1));
        self.unsealed
            .pipeline_order
            .get(id.index())
            .and_then(|name| self.unsealed.pipelines.get(name))
            .map(Arc::as_ref)
    }

    #[cfg(test)]
    pub(crate) fn pipeline_arc(&self, id: PipelineId) -> Option<&Arc<PipelineBlueprint>> {
        self.unsealed
            .pipeline_order
            .get(id.index())
            .and_then(|name| self.unsealed.pipelines.get(name))
    }

    #[cfg(test)]
    pub(crate) fn reset_pipeline_by_id_calls_for_testing(&self) {
        PIPELINE_BY_ID_CALLS.with(|calls| calls.set(0));
    }

    #[cfg(test)]
    pub(crate) fn pipeline_by_id_calls_for_testing(&self) -> usize {
        PIPELINE_BY_ID_CALLS.with(std::cell::Cell::get)
    }

    pub(crate) fn pipelines(&self) -> impl Iterator<Item = (PipelineId, &PipelineBlueprint)> {
        self.unsealed
            .pipeline_order
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                Some((
                    PipelineId::from_index(index).ok()?,
                    self.unsealed.pipelines.get(name)?.as_ref(),
                ))
            })
    }

    pub(crate) fn node_id(&self) -> Option<&str> {
        self.unsealed.node_id.as_deref()
    }

    pub(crate) fn node_key(&self) -> Option<&str> {
        self.unsealed.node_key.as_deref()
    }

    pub(crate) fn inputs(&self) -> &HashMap<String, InputDef> {
        &self.unsealed.inputs
    }

    pub(crate) fn outputs(&self) -> &HashMap<String, OutputDef> {
        &self.unsealed.outputs
    }

    pub(crate) fn functions(&self) -> &HashMap<String, FunctionDef> {
        &self.unsealed.functions
    }

    pub(crate) fn global_blocks(&self) -> &HashMap<String, Vec<Property>> {
        &self.unsealed.global_blocks
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        registry: &crate::metrics::Registry,
    ) -> Result<BoundRuntimeBlueprint> {
        let mut pipelines = Vec::with_capacity(self.unsealed.pipeline_order.len());
        for pipeline_name in &self.unsealed.pipeline_order {
            let pipeline =
                self.unsealed.pipelines.get(pipeline_name).ok_or_else(|| {
                    anyhow::anyhow!("pipeline '{pipeline_name}' is missing at bind")
                })?;
            let process_counters = pipeline
                .metric_nodes
                .iter()
                .map(|node| {
                    crate::metrics::ProcessCounters::register(
                        registry,
                        pipeline_name,
                        node.step as usize,
                        &node.process_path,
                        &node.process_name,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            let output_timers = pipeline
                .output_timers
                .iter()
                .map(|output| {
                    crate::metrics::PipelineOutputTimer::register(registry, pipeline_name, output)
                })
                .collect::<Result<Vec<_>, _>>()?;
            pipelines.push(Arc::new(BoundPipelineExecution::new(
                Arc::clone(self),
                Arc::clone(pipeline),
                BoundPipelineMetrics {
                    process_counters,
                    output_timers,
                },
            )?));
        }
        Ok(BoundRuntimeBlueprint {
            blueprint: Arc::clone(self),
            pipelines,
        })
    }
}

impl BoundRuntimeBlueprint {
    pub(crate) fn pipeline_execution(
        &self,
        id: PipelineId,
    ) -> Option<&Arc<BoundPipelineExecution>> {
        self.pipelines.get(id.index())
    }

    #[cfg(test)]
    pub(crate) fn pipeline_metrics(&self, id: PipelineId) -> Option<&BoundPipelineMetrics> {
        self.pipeline_execution(id).map(|bound| bound.metrics())
    }
}

impl BoundPipelineExecution {
    fn new(
        blueprint: Arc<RuntimeBlueprint>,
        pipeline: Arc<PipelineBlueprint>,
        metrics: BoundPipelineMetrics,
    ) -> Result<Self> {
        if metrics.process_counters.len() != pipeline.metric_nodes.len()
            || metrics.output_timers.len() != pipeline.output_timers.len()
        {
            bail!("pipeline '{}' metric binding shape mismatch", pipeline.name);
        }
        #[cfg(test)]
        BOUND_PIPELINE_EXECUTION_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
        Ok(Self {
            blueprint,
            pipeline,
            metrics,
        })
    }

    pub(crate) fn blueprint(&self) -> &RuntimeBlueprint {
        &self.blueprint
    }

    pub(crate) fn pipeline(&self) -> &PipelineBlueprint {
        &self.pipeline
    }

    #[cfg(test)]
    pub(crate) fn pipeline_arc(&self) -> &Arc<PipelineBlueprint> {
        &self.pipeline
    }

    pub(crate) fn metrics(&self) -> &BoundPipelineMetrics {
        &self.metrics
    }
}

#[cfg(test)]
fn reset_bound_pipeline_execution_constructions_for_testing() {
    BOUND_PIPELINE_EXECUTION_CONSTRUCTIONS.with(|count| count.set(0));
}

#[cfg(test)]
fn bound_pipeline_execution_constructions_for_testing() -> usize {
    BOUND_PIPELINE_EXECUTION_CONSTRUCTIONS.with(std::cell::Cell::get)
}

impl UnsealedRuntimeBlueprint {
    pub(crate) fn compile(config: &CompiledConfig) -> Result<Self> {
        let mut compiler = BlueprintCompiler::new(config)?;
        compiler.compile_named_processes(config)?;
        let mut pipelines = BTreeMap::new();
        let mut pipeline_names: Vec<&String> = config.pipelines.keys().collect();
        pipeline_names.sort();
        for name in pipeline_names {
            let pipeline = config
                .pipelines
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("pipeline '{name}' disappeared during compile"))?;
            let compiled = compiler.compile_pipeline(pipeline)?;
            pipelines.insert(name.clone(), Arc::new(compiled));
        }
        let pipeline_order: Vec<String> = pipelines.keys().cloned().collect();
        let pipeline_ids = pipeline_order
            .iter()
            .enumerate()
            .map(|(index, name)| Ok((name.clone(), PipelineId::from_index(index)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            node_id: config.node_id.clone(),
            node_key: config.node_key.clone(),
            inputs: config.inputs.clone(),
            outputs: config.outputs.clone(),
            functions: config.functions.clone(),
            global_blocks: config.global_blocks.clone(),
            process_bodies: compiler.process_bodies,
            process_names: compiler.process_names,
            pipelines,
            pipeline_ids,
            pipeline_order,
        })
    }

    pub(crate) fn seal(self) -> Result<RuntimeBlueprint> {
        self.validate_process_identity_and_edges()?;
        for pipeline in self.pipelines.values() {
            self.validate_pipeline(pipeline)?;
        }
        if self.pipeline_order.len() != self.pipelines.len()
            || self.pipeline_ids.len() != self.pipelines.len()
        {
            bail!("pipeline identity table length mismatch");
        }
        for (index, name) in self.pipeline_order.iter().enumerate() {
            let expected = PipelineId::from_index(index)?;
            if self.pipeline_ids.get(name) != Some(&expected) || !self.pipelines.contains_key(name)
            {
                bail!("pipeline identity table mismatch for '{name}'");
            }
        }
        Ok(RuntimeBlueprint { unsealed: self })
    }

    fn validate_process_identity_and_edges(&self) -> Result<()> {
        for (name, body_id) in &self.process_names {
            let body = self
                .process_bodies
                .get(body_id.index())
                .ok_or_else(|| anyhow::anyhow!("process body id for '{name}' is out of range"))?;
            if body.name != *name {
                bail!("process body identity mismatch for '{name}'");
            }
        }
        for body in &self.process_bodies {
            let mut seen_edges = HashMap::<&str, EdgeSlot>::new();
            for (edge_index, edge) in body.edges.iter().enumerate() {
                if let ProcessTarget::Known(target) = edge.target
                    && self.process_bodies.get(target.index()).is_none()
                {
                    bail!("process '{}' edge target is out of range", body.name);
                }
                let slot = EdgeSlot::from_index(edge_index)?;
                if seen_edges.insert(&edge.name, slot).is_some() {
                    bail!("process '{}' has duplicate edge '{}'", body.name, edge.name);
                }
            }
            Self::validate_process_code(body, &body.code)?;
        }
        self.validate_process_cycles()
    }

    fn validate_process_code(body: &ProcessBodyCode, code: &[ProcessCode]) -> Result<()> {
        for statement in code {
            match statement {
                ProcessCode::Call { name, edge_slot } => {
                    let edge = body.edges.get(edge_slot.index()).ok_or_else(|| {
                        anyhow::anyhow!("process '{}' call edge slot is out of range", body.name)
                    })?;
                    if edge.name != *name {
                        bail!("process '{}' call edge identity mismatch", body.name);
                    }
                }
                ProcessCode::If {
                    branches,
                    else_body,
                } => {
                    for (_, branch) in branches {
                        Self::validate_process_code(body, branch)?;
                    }
                    if let Some(branch) = else_body {
                        Self::validate_process_code(body, branch)?;
                    }
                }
                ProcessCode::Switch { arms, .. } => {
                    for (_, arm) in arms {
                        Self::validate_process_code(body, arm)?;
                    }
                }
                ProcessCode::TryCatch {
                    try_body,
                    catch_body,
                } => {
                    Self::validate_process_code(body, try_body)?;
                    Self::validate_process_code(body, catch_body)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_process_cycles(&self) -> Result<()> {
        fn visit(
            blueprint: &UnsealedRuntimeBlueprint,
            id: ProcessBodyId,
            active: &mut Vec<ProcessBodyId>,
            done: &mut std::collections::HashSet<ProcessBodyId>,
        ) -> Result<()> {
            if done.contains(&id) {
                return Ok(());
            }
            if let Some(position) = active.iter().position(|active_id| *active_id == id) {
                let mut names: Vec<String> = active[position..]
                    .iter()
                    .map(|active_id| blueprint.process_bodies[active_id.index()].name.clone())
                    .collect();
                names.push(blueprint.process_bodies[id.index()].name.clone());
                bail!("process call cycle: {}", names.join(" -> "));
            }
            active.push(id);
            let body = blueprint
                .process_bodies
                .get(id.index())
                .ok_or_else(|| anyhow::anyhow!("process body id is out of range"))?;
            for edge in &body.edges {
                if let ProcessTarget::Known(target) = edge.target {
                    visit(blueprint, target, active, done)?;
                }
            }
            active.pop();
            done.insert(id);
            Ok(())
        }

        let mut done = std::collections::HashSet::new();
        for index in 0..self.process_bodies.len() {
            visit(
                self,
                ProcessBodyId::from_index(index)?,
                &mut Vec::new(),
                &mut done,
            )?;
        }
        Ok(())
    }

    fn validate_pipeline(&self, pipeline: &PipelineBlueprint) -> Result<()> {
        if pipeline.metric_nodes.len() > u32::MAX as usize {
            bail!(
                "pipeline '{}' metric node count exceeds the IR limit",
                pipeline.name
            );
        }
        for (index, node) in pipeline.metric_nodes.iter().enumerate() {
            let node_id = MetricNodeId::from_index(index)?;
            if node.step
                != u32::try_from(index + 1).map_err(|_| anyhow::anyhow!("metric step overflow"))?
            {
                bail!("pipeline '{}' metric step sequence mismatch", pipeline.name);
            }
            match node.target {
                ProcessTarget::Known(body_id) => {
                    let body = self.process_bodies.get(body_id.index()).ok_or_else(|| {
                        anyhow::anyhow!("pipeline '{}' metric body is out of range", pipeline.name)
                    })?;
                    if node.children.len() != body.edges.len() {
                        bail!("pipeline '{}' metric child count mismatch", pipeline.name);
                    }
                    for (slot, child_id) in node.children.iter().enumerate() {
                        let child =
                            pipeline.metric_nodes.get(child_id.index()).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "pipeline '{}' metric child id is out of range",
                                    pipeline.name
                                )
                            })?;
                        let edge = &body.edges[slot];
                        if child.parent != Some(node_id)
                            || child.target != edge.target
                            || child.process_name != edge.name
                            || child.process_path != format!("{}/{}", node.process_path, edge.name)
                        {
                            bail!(
                                "pipeline '{}' metric child identity mismatch",
                                pipeline.name
                            );
                        }
                    }
                }
                ProcessTarget::Unknown if !node.children.is_empty() => {
                    bail!(
                        "pipeline '{}' unknown metric node has children",
                        pipeline.name
                    );
                }
                ProcessTarget::Unknown => {}
            }
        }
        let expected_subscription = pipeline
            .code
            .iter()
            .find_map(|statement| match statement {
                PipelineCode::Input(names) => Some(names.as_slice()),
                _ => None,
            })
            .unwrap_or_default();
        if pipeline.subscription_inputs != expected_subscription {
            bail!(
                "pipeline '{}' subscription input identity mismatch",
                pipeline.name
            );
        }
        self.validate_pipeline_code(pipeline, &pipeline.code)
    }

    fn validate_pipeline_code(
        &self,
        pipeline: &PipelineBlueprint,
        code: &[PipelineCode],
    ) -> Result<()> {
        for statement in code {
            match statement {
                PipelineCode::ProcessChain(sites) => {
                    for site in sites {
                        let is_named_body = self.process_names.values().any(|id| *id == site.body);
                        if (site.kind == SiteKind::Named) != is_named_body {
                            bail!("pipeline '{}' process site kind mismatch", pipeline.name);
                        }
                        let node = pipeline
                            .metric_nodes
                            .get(site.metric_node.index())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "pipeline '{}' root metric node is out of range",
                                    pipeline.name
                                )
                            })?;
                        if node.parent.is_some()
                            || node.target != ProcessTarget::Known(site.body)
                            || node.process_name != site.name
                            || node.process_path != format!("/{}", site.name)
                        {
                            bail!("pipeline '{}' root metric identity mismatch", pipeline.name);
                        }
                    }
                }
                PipelineCode::Output { name, timer_slot }
                    if pipeline.output_timers.get(timer_slot.0 as usize) != Some(name) =>
                {
                    bail!(
                        "pipeline '{}' output timer identity mismatch",
                        pipeline.name
                    );
                }
                PipelineCode::Output { .. } => {}
                PipelineCode::If {
                    branches,
                    else_body,
                } => {
                    for (_, body) in branches {
                        self.validate_pipeline_code(pipeline, body)?;
                    }
                    if let Some(body) = else_body {
                        self.validate_pipeline_code(pipeline, body)?;
                    }
                }
                PipelineCode::Switch { arms, .. } => {
                    for (_, body) in arms {
                        self.validate_pipeline_code(pipeline, body)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn first_call_slot_mut_for_testing(&mut self, process: &str) -> Option<&mut EdgeSlot> {
        fn find(code: &mut [ProcessCode]) -> Option<&mut EdgeSlot> {
            for statement in code {
                match statement {
                    ProcessCode::Call { edge_slot, .. } => return Some(edge_slot),
                    ProcessCode::If {
                        branches,
                        else_body,
                    } => {
                        for (_, body) in branches {
                            if let Some(slot) = find(body) {
                                return Some(slot);
                            }
                        }
                        if let Some(body) = else_body
                            && let Some(slot) = find(body)
                        {
                            return Some(slot);
                        }
                    }
                    ProcessCode::Switch { arms, .. } => {
                        for (_, body) in arms {
                            if let Some(slot) = find(body) {
                                return Some(slot);
                            }
                        }
                    }
                    ProcessCode::TryCatch {
                        try_body,
                        catch_body,
                    } => {
                        if let Some(slot) = find(try_body).or_else(|| find(catch_body)) {
                            return Some(slot);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        let id = *self.process_names.get(process)?;
        find(&mut self.process_bodies.get_mut(id.index())?.code)
    }
}

struct BlueprintCompiler {
    process_bodies: Vec<ProcessBodyCode>,
    process_names: BTreeMap<String, ProcessBodyId>,
}

impl BlueprintCompiler {
    fn new(config: &CompiledConfig) -> Result<Self> {
        let mut names: Vec<&String> = config.processes.keys().collect();
        names.sort();
        let mut process_names = BTreeMap::new();
        for name in names {
            let id = ProcessBodyId::from_index(process_names.len())?;
            process_names.insert(name.clone(), id);
        }
        Ok(Self {
            process_bodies: Vec::with_capacity(process_names.len()),
            process_names,
        })
    }

    fn compile_named_processes(&mut self, config: &CompiledConfig) -> Result<()> {
        let names: Vec<(String, ProcessBodyId)> = self
            .process_names
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect();
        for (name, expected_id) in names {
            let def = config
                .processes
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("process '{name}' disappeared during compile"))?;
            let body = self.compile_process_body(name.clone(), &def.body)?;
            let actual_id = ProcessBodyId::from_index(self.process_bodies.len())?;
            if actual_id != expected_id {
                bail!("process body id assignment is not deterministic");
            }
            self.process_bodies.push(body);
        }
        Ok(())
    }

    fn compile_process_body(
        &self,
        name: String,
        statements: &[ProcessStatement],
    ) -> Result<ProcessBodyCode> {
        let mut edges = Vec::new();
        let mut slots = HashMap::new();
        let code = self.compile_process_statements(statements, &mut edges, &mut slots)?;
        Ok(ProcessBodyCode { name, code, edges })
    }

    fn compile_process_statements(
        &self,
        statements: &[ProcessStatement],
        edges: &mut Vec<ProcessEdge>,
        slots: &mut HashMap<String, EdgeSlot>,
    ) -> Result<Vec<ProcessCode>> {
        statements
            .iter()
            .map(|statement| self.compile_process_statement(statement, edges, slots))
            .collect()
    }

    fn compile_process_branch(
        &self,
        body: &[BranchBody],
        edges: &mut Vec<ProcessEdge>,
        slots: &mut HashMap<String, EdgeSlot>,
    ) -> Result<Vec<ProcessCode>> {
        body.iter()
            .map(|item| match item {
                BranchBody::Process(statement) => {
                    self.compile_process_statement(statement, edges, slots)
                }
                BranchBody::Pipeline(_) => bail!("pipeline statement found in process body"),
            })
            .collect()
    }

    fn compile_process_statement(
        &self,
        statement: &ProcessStatement,
        edges: &mut Vec<ProcessEdge>,
        slots: &mut HashMap<String, EdgeSlot>,
    ) -> Result<ProcessCode> {
        Ok(match statement {
            ProcessStatement::Assign(target, expression) => {
                ProcessCode::Assign(target.clone(), expression.clone())
            }
            ProcessStatement::LetBinding(name, expression) => {
                ProcessCode::LetBinding(name.clone(), expression.clone())
            }
            ProcessStatement::ProcessCall(name) => {
                let edge_slot = match slots.get(name) {
                    Some(slot) => *slot,
                    None => {
                        let target = self
                            .process_names
                            .get(name)
                            .copied()
                            .map(ProcessTarget::Known)
                            .unwrap_or(ProcessTarget::Unknown);
                        let slot = EdgeSlot::from_index(edges.len())?;
                        edges.push(ProcessEdge {
                            name: name.clone(),
                            target,
                        });
                        slots.insert(name.clone(), slot);
                        slot
                    }
                };
                ProcessCode::Call {
                    name: name.clone(),
                    edge_slot,
                }
            }
            ProcessStatement::Drop => ProcessCode::Drop,
            ProcessStatement::Error(expression) => ProcessCode::Error(expression.clone()),
            ProcessStatement::If(chain) => ProcessCode::If {
                branches: chain
                    .branches
                    .iter()
                    .map(|(condition, body)| {
                        Ok((
                            condition.clone(),
                            self.compile_process_branch(body, edges, slots)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_body: chain
                    .else_body
                    .as_deref()
                    .map(|body| self.compile_process_branch(body, edges, slots))
                    .transpose()?,
            },
            ProcessStatement::Switch(discriminant, arms) => ProcessCode::Switch {
                discriminant: discriminant.clone(),
                arms: arms
                    .iter()
                    .map(|arm| {
                        Ok((
                            arm.pattern.clone(),
                            self.compile_process_branch(&arm.body, edges, slots)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
            ProcessStatement::TryCatch(try_body, catch_body) => ProcessCode::TryCatch {
                try_body: self.compile_process_statements(try_body, edges, slots)?,
                catch_body: self.compile_process_statements(catch_body, edges, slots)?,
            },
            ProcessStatement::ExprStmt(expression) => ProcessCode::Expr(expression.clone()),
        })
    }

    fn compile_inline(&mut self, body: &[ProcessStatement]) -> Result<ProcessBodyId> {
        let id = ProcessBodyId::from_index(self.process_bodies.len())?;
        let name = format!("(inline@{})", id.index());
        let code = self.compile_process_body(name, body)?;
        self.process_bodies.push(code);
        Ok(id)
    }

    fn compile_pipeline(
        &mut self,
        pipeline: &crate::dsl::ast::PipelineDef,
    ) -> Result<PipelineBlueprint> {
        let mut output_timers = Vec::new();
        let mut output_slots = HashMap::new();
        let mut code = self.compile_pipeline_statements(
            &pipeline.body,
            &mut output_timers,
            &mut output_slots,
        )?;
        let mut metric_nodes = Vec::new();
        let mut next_step = 1u32;
        Self::attach_pipeline_metric_nodes(
            &self.process_bodies,
            &mut code,
            &mut metric_nodes,
            &mut next_step,
        )?;
        let subscription_inputs = code
            .iter()
            .find_map(|statement| match statement {
                PipelineCode::Input(names) => Some(names.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let flow = Self::collect_pipeline_flow(&code);
        Ok(PipelineBlueprint {
            name: pipeline.name.clone(),
            code,
            flow,
            subscription_inputs,
            metric_nodes,
            output_timers,
        })
    }

    fn collect_pipeline_flow(code: &[PipelineCode]) -> PipelineFlow {
        fn visit(code: &[PipelineCode], flow: &mut PipelineFlow) {
            for statement in code {
                match statement {
                    PipelineCode::Input(names) => {
                        for name in names {
                            if !flow.inputs.contains(name) {
                                flow.inputs.push(name.clone());
                            }
                        }
                    }
                    PipelineCode::ProcessChain(sites) => {
                        for site in sites {
                            if site.kind == SiteKind::Named && !flow.processes.contains(&site.name)
                            {
                                flow.processes.push(site.name.clone());
                            }
                        }
                    }
                    PipelineCode::Output { name, .. } if !flow.outputs.contains(name) => {
                        flow.outputs.push(name.clone());
                    }
                    PipelineCode::Output { .. } => {}
                    PipelineCode::If {
                        branches,
                        else_body,
                    } => {
                        for (_, branch) in branches {
                            visit(branch, flow);
                        }
                        if let Some(branch) = else_body {
                            visit(branch, flow);
                        }
                    }
                    PipelineCode::Switch { arms, .. } => {
                        for (_, arm) in arms {
                            visit(arm, flow);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut flow = PipelineFlow::default();
        visit(code, &mut flow);
        flow
    }

    fn compile_pipeline_statements(
        &mut self,
        statements: &[PipelineStatement],
        output_timers: &mut Vec<String>,
        output_slots: &mut HashMap<String, OutputTimerSlot>,
    ) -> Result<Vec<PipelineCode>> {
        statements
            .iter()
            .map(|statement| {
                self.compile_pipeline_statement(statement, output_timers, output_slots)
            })
            .collect()
    }

    fn compile_pipeline_branch(
        &mut self,
        body: &[BranchBody],
        output_timers: &mut Vec<String>,
        output_slots: &mut HashMap<String, OutputTimerSlot>,
    ) -> Result<Vec<PipelineCode>> {
        body.iter()
            .map(|item| match item {
                BranchBody::Pipeline(statement) => {
                    self.compile_pipeline_statement(statement, output_timers, output_slots)
                }
                BranchBody::Process(_) => bail!("process statement found in pipeline body"),
            })
            .collect()
    }

    fn compile_pipeline_statement(
        &mut self,
        statement: &PipelineStatement,
        output_timers: &mut Vec<String>,
        output_slots: &mut HashMap<String, OutputTimerSlot>,
    ) -> Result<PipelineCode> {
        Ok(match statement {
            PipelineStatement::Input(names) => PipelineCode::Input(names.clone()),
            PipelineStatement::ProcessChain(chain) => {
                let mut sites = Vec::with_capacity(chain.len());
                for element in chain {
                    let (name, kind, body) = match element {
                        ProcessChainElement::Named(name) => (
                            name.clone(),
                            SiteKind::Named,
                            self.process_names.get(name).copied().ok_or_else(|| {
                                anyhow::anyhow!("pipeline references unknown process '{name}'")
                            })?,
                        ),
                        ProcessChainElement::Inline(body) => (
                            "(inline)".to_owned(),
                            SiteKind::Inline,
                            self.compile_inline(body)?,
                        ),
                    };
                    sites.push(ProcessSite {
                        name,
                        kind,
                        body,
                        metric_node: MetricNodeId(u32::MAX),
                    });
                }
                PipelineCode::ProcessChain(sites)
            }
            PipelineStatement::Output(name) => {
                let timer_slot = match output_slots.get(name) {
                    Some(slot) => *slot,
                    None => {
                        let slot = OutputTimerSlot::from_index(output_timers.len())?;
                        output_timers.push(name.clone());
                        output_slots.insert(name.clone(), slot);
                        slot
                    }
                };
                PipelineCode::Output {
                    name: name.clone(),
                    timer_slot,
                }
            }
            PipelineStatement::Drop => PipelineCode::Drop,
            PipelineStatement::Finish => PipelineCode::Finish,
            PipelineStatement::Error(expression) => PipelineCode::Error(expression.clone()),
            PipelineStatement::If(chain) => PipelineCode::If {
                branches: chain
                    .branches
                    .iter()
                    .map(|(condition, body)| {
                        Ok((
                            condition.clone(),
                            self.compile_pipeline_branch(body, output_timers, output_slots)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_body: chain
                    .else_body
                    .as_deref()
                    .map(|body| self.compile_pipeline_branch(body, output_timers, output_slots))
                    .transpose()?,
            },
            PipelineStatement::Switch(discriminant, arms) => PipelineCode::Switch {
                discriminant: discriminant.clone(),
                arms: arms
                    .iter()
                    .map(|arm| {
                        Ok((
                            arm.pattern.clone(),
                            self.compile_pipeline_branch(&arm.body, output_timers, output_slots)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    fn attach_pipeline_metric_nodes(
        process_bodies: &[ProcessBodyCode],
        code: &mut [PipelineCode],
        nodes: &mut Vec<MetricNodeDescriptor>,
        next_step: &mut u32,
    ) -> Result<()> {
        for statement in code {
            match statement {
                PipelineCode::ProcessChain(sites) => {
                    for site in sites {
                        site.metric_node = Self::add_metric_node(
                            process_bodies,
                            nodes,
                            next_step,
                            None,
                            ProcessTarget::Known(site.body),
                            &site.name,
                            format!("/{}", site.name),
                            &mut Vec::new(),
                        )?;
                    }
                }
                PipelineCode::If {
                    branches,
                    else_body,
                } => {
                    for (_, body) in branches {
                        Self::attach_pipeline_metric_nodes(process_bodies, body, nodes, next_step)?;
                    }
                    if let Some(body) = else_body {
                        Self::attach_pipeline_metric_nodes(process_bodies, body, nodes, next_step)?;
                    }
                }
                PipelineCode::Switch { arms, .. } => {
                    for (_, body) in arms {
                        Self::attach_pipeline_metric_nodes(process_bodies, body, nodes, next_step)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_metric_node(
        process_bodies: &[ProcessBodyCode],
        nodes: &mut Vec<MetricNodeDescriptor>,
        next_step: &mut u32,
        parent: Option<MetricNodeId>,
        target: ProcessTarget,
        name: &str,
        process_path: String,
        ancestors: &mut Vec<(ProcessBodyId, String)>,
    ) -> Result<MetricNodeId> {
        let id = MetricNodeId::from_index(nodes.len())?;
        let step = *next_step;
        *next_step = (*next_step)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("metric step overflow"))?;
        nodes.push(MetricNodeDescriptor {
            parent,
            step,
            process_path: process_path.clone(),
            process_name: name.to_owned(),
            target,
            children: Vec::new(),
        });
        let ProcessTarget::Known(body_id) = target else {
            return Ok(id);
        };
        let body = process_bodies
            .get(body_id.index())
            .ok_or_else(|| anyhow::anyhow!("process body id is out of range"))?;
        if let Some(position) = ancestors
            .iter()
            .position(|(ancestor, _)| *ancestor == body_id)
        {
            let mut path: Vec<String> = ancestors[position..]
                .iter()
                .map(|(_, ancestor)| ancestor.clone())
                .collect();
            path.push(name.to_owned());
            bail!("process call cycle: {}", path.join(" -> "));
        }
        ancestors.push((body_id, name.to_owned()));
        let mut children = Vec::with_capacity(body.edges.len());
        for edge in &body.edges {
            children.push(Self::add_metric_node(
                process_bodies,
                nodes,
                next_step,
                Some(id),
                edge.target,
                &edge.name,
                format!("{process_path}/{}", edge.name),
                ancestors,
            )?);
        }
        ancestors.pop();
        nodes[id.index()].children = children;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::dsl::parser::parse_config;

    fn compile(source: &str) -> UnsealedRuntimeBlueprint {
        let config =
            CompiledConfig::from_config(parse_config(source).expect("parse blueprint fixture"))
                .expect("compile blueprint fixture");
        UnsealedRuntimeBlueprint::compile(&config).expect("compile unsealed blueprint")
    }

    fn pipeline_mut<'a>(
        blueprint: &'a mut UnsealedRuntimeBlueprint,
        name: &str,
    ) -> &'a mut PipelineBlueprint {
        Arc::make_mut(blueprint.pipelines.get_mut(name).expect("pipeline"))
    }

    fn topology_fixture() -> &'static str {
        r#"
def process leaf { egress = ingress }
def process parent_one { process leaf }
def process parent_two { process leaf }
def process repeated { egress = ingress }
def process dispatch {
    if true { process leaf } else { process leaf }
    switch "first" {
        "first" { process leaf }
        default { process leaf }
    }
    try { process leaf } catch { process leaf }
}
def process branch_then { egress = ingress }
def process branch_else { egress = ingress }
def process arm_first { egress = ingress }
def process arm_default { egress = ingress }
def pipeline topology {
    process parent_one | parent_two
    process repeated
    process repeated
    process dispatch
    if true { process branch_then } else { process branch_else }
    switch "first" {
        "first" { process arm_first }
        default { process arm_default }
    }
    process { drop }
    finish
}
"#
    }

    fn metric_series(registry: &crate::metrics::Registry, name: &str) -> Vec<serde_json::Value> {
        let snapshot = serde_json::to_value(registry.snapshot()).expect("serialize snapshot");
        snapshot["metrics"]
            .as_array()
            .expect("metrics array")
            .iter()
            .find(|family| family["name"] == name)
            .unwrap_or_else(|| panic!("missing metric family {name}"))["series"]
            .as_array()
            .expect("series array")
            .clone()
    }

    fn descriptor_series(pipeline: &PipelineBlueprint) -> BTreeSet<(u32, String, String)> {
        pipeline
            .metric_nodes
            .iter()
            .map(|node| {
                (
                    node.step,
                    node.process_path.clone(),
                    node.process_name.clone(),
                )
            })
            .collect()
    }

    fn seal_error(blueprint: UnsealedRuntimeBlueprint) -> String {
        match blueprint.seal() {
            Ok(_) => panic!("corrupt blueprint unexpectedly sealed"),
            Err(error) => error.to_string(),
        }
    }

    fn series_value(
        registry: &crate::metrics::Registry,
        family: &str,
        expected: &serde_json::Value,
    ) -> u64 {
        metric_series(registry, family)
            .into_iter()
            .find(|series| series["labels"] == *expected)
            .unwrap_or_else(|| panic!("missing {family} series for {expected}"))["value"]
            .as_u64()
            .expect("counter value")
    }

    fn histogram_count(
        registry: &crate::metrics::Registry,
        family: &str,
        expected: &serde_json::Value,
    ) -> u64 {
        metric_series(registry, family)
            .into_iter()
            .find(|series| series["labels"] == *expected)
            .unwrap_or_else(|| panic!("missing {family} series for {expected}"))["count"]
            .as_u64()
            .expect("histogram count")
    }

    fn production_prefix(source: &str) -> &str {
        source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or(source)
    }

    fn declared_fields(source: &str, declaration: &str) -> Vec<String> {
        fn field_name(line: &str) -> Option<&str> {
            let line = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub(super) "))
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            let (name, _) = line.split_once(':')?;
            let mut chars = name.chars();
            let first = chars.next()?;
            (first == '_' || first.is_ascii_alphabetic())
                .then_some(())
                .filter(|_| chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()))?;
            Some(name)
        }

        let body = source
            .split(declaration)
            .nth(1)
            .unwrap_or_else(|| panic!("missing declaration {declaration}"))
            .split("\n}\n")
            .next()
            .expect("struct body");
        let mut fields = Vec::new();
        let mut nesting = 0_i32;
        let mut pending = None;
        for raw in body.lines() {
            let line = raw.trim();
            if nesting == 0 && field_name(line).is_some() {
                pending = Some(line.to_owned());
            }
            for ch in line.chars() {
                match ch {
                    '<' | '(' | '[' => nesting += 1,
                    '>' | ')' | ']' => nesting = (nesting - 1).max(0),
                    _ => {}
                }
            }
            if nesting == 0
                && line.ends_with(',')
                && let Some(field) = pending.take()
            {
                fields.push(field);
            }
        }
        fields
    }

    #[test]
    fn compiler_seals_edge_slots_and_path_specific_metric_frames() {
        let source = topology_fixture();
        let sealed = compile(source).seal().expect("seal valid blueprint");
        let pipeline = sealed.pipeline("topology").expect("topology pipeline");

        let descriptors = descriptor_series(pipeline);
        assert_eq!(
            descriptors.len(),
            pipeline.metric_nodes.len(),
            "every sealed metric node must have one distinct series descriptor"
        );
        assert!(
            descriptors
                .iter()
                .all(|(_, path, name)| !path.is_empty() && !name.is_empty())
        );

        let dispatch = sealed
            .process_bodies()
            .iter()
            .find(|body| body.name == "dispatch")
            .expect("dispatch body");
        assert_eq!(
            dispatch.edges.len(),
            1,
            "same parent/name must share one EdgeSlot"
        );
        assert_eq!(dispatch.edges[0].name, "leaf");

        let repeated_roots: Vec<&MetricNodeDescriptor> = pipeline
            .metric_nodes
            .iter()
            .filter(|node| node.parent.is_none() && node.process_name == "repeated")
            .collect();
        assert_eq!(
            repeated_roots.len(),
            2,
            "root lexical sites remain distinct"
        );
        assert_ne!(repeated_roots[0].step, repeated_roots[1].step);

        let parent_leaf_nodes: Vec<&MetricNodeDescriptor> = pipeline
            .metric_nodes
            .iter()
            .filter(|node| node.process_name == "leaf")
            .collect();
        assert_eq!(
            parent_leaf_nodes.len(),
            3,
            "different parents require distinct metric frames"
        );
        assert!(parent_leaf_nodes.iter().all(|node| node.parent.is_some()));

        let dispatch_node = pipeline
            .metric_nodes
            .iter()
            .find(|node| node.process_name == "dispatch")
            .expect("dispatch metric node");
        let edge_slot = dispatch
            .edges
            .iter()
            .position(|edge| edge.name == "leaf")
            .map(EdgeSlot::from_index)
            .transpose()
            .expect("edge slot range")
            .expect("leaf edge slot");
        let current = &pipeline.metric_nodes[dispatch_node.children[edge_slot.index()].index()];
        assert_eq!(current.process_path, "/dispatch/leaf");
        assert_eq!(
            current.parent.map(MetricNodeId::index),
            Some(dispatch_node.step as usize - 1)
        );
    }

    #[test]
    fn compiler_keeps_semantic_process_body_count_independent_of_roots_and_paths() {
        let source = r#"
def process leaf { drop }
def process middle { process leaf; process leaf }
def process root { process middle }
def pipeline first {
    process root
    process root
    if true { process middle } else { process middle }
    process { process leaf }
    finish
}
def pipeline second {
    process root | middle
    process { process leaf }
    finish
}
"#;
        let sealed = compile(source).seal().expect("seal body-count fixture");
        assert_eq!(
            sealed.process_bodies().len(),
            5,
            "three named definitions plus two lexical inline sites; roots and paths must not clone bodies"
        );
        assert_eq!(
            sealed
                .process_bodies()
                .iter()
                .filter(|body| body.name == "leaf")
                .count(),
            1
        );
        assert!(
            sealed.pipeline("first").expect("first").metric_nodes.len()
                > sealed.process_bodies().len(),
            "only metric topology grows with path count"
        );
    }

    #[test]
    fn nested_unknown_process_seals_with_a_path_specific_metric_series() {
        let blueprint = compile(
            r#"
def process parent { process missing }
def pipeline p { process parent; finish }
"#,
        )
        .seal()
        .expect("nested unknown calls remain runtime passthroughs");
        let pipeline = blueprint.pipeline("p").expect("pipeline p");
        let parent = blueprint
            .process_bodies()
            .iter()
            .find(|body| body.name == "parent")
            .expect("parent body");
        assert_eq!(parent.edges[0].target, ProcessTarget::Unknown);
        assert_eq!(pipeline.metric_nodes.len(), 2);
        let missing = &pipeline.metric_nodes[1];
        assert_eq!(missing.parent, Some(MetricNodeId::from_index(0).unwrap()));
        assert_eq!(missing.process_path, "/parent/missing");
        assert_eq!(missing.process_name, "missing");
        assert_eq!(missing.target, ProcessTarget::Unknown);
        assert!(missing.children.is_empty());
    }

    #[test]
    fn root_unknown_process_remains_a_configuration_error() {
        let parsed = parse_config("def pipeline p { process missing; finish }")
            .expect("parse root unknown fixture");
        let config = CompiledConfig::from_config(parsed).expect("compile root fixture");
        let error = match UnsealedRuntimeBlueprint::compile(&config) {
            Ok(_) => panic!("root unknown process unexpectedly compiled into the execution IR"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("unknown process 'missing'"), "{error}");
    }

    #[test]
    fn compiler_preserves_pipeline_flow_order_and_shares_output_timer_slots() {
        let sealed = compile(
            r#"
def pipeline flow {
    input beta, alpha
    output sink
    if true { output sink } else { finish }
    switch "x" {
        "x" { drop }
        default { error "bad" }
    }
    output other
    finish
}
"#,
        )
        .seal()
        .expect("seal flow fixture");
        let pipeline = sealed.pipeline("flow").expect("flow pipeline");
        assert_eq!(pipeline.output_timers, ["sink", "other"]);
        assert!(
            matches!(&pipeline.code[0], PipelineCode::Input(names) if names == &["beta", "alpha"])
        );
        assert!(
            matches!(&pipeline.code[1], PipelineCode::Output { name, timer_slot } if name == "sink" && timer_slot.0 == 0)
        );
        let PipelineCode::If {
            branches,
            else_body,
        } = &pipeline.code[2]
        else {
            panic!("third statement must remain If");
        };
        assert!(
            matches!(&branches[0].1[0], PipelineCode::Output { name, timer_slot } if name == "sink" && timer_slot.0 == 0)
        );
        assert!(matches!(
            &else_body.as_ref().expect("else")[0],
            PipelineCode::Finish
        ));
        assert!(
            matches!(&pipeline.code[3], PipelineCode::Switch { arms, .. } if matches!(arms[0].1[0], PipelineCode::Drop) && matches!(arms[1].1[0], PipelineCode::Error(_)))
        );
        assert!(
            matches!(&pipeline.code[4], PipelineCode::Output { name, timer_slot } if name == "other" && timer_slot.0 == 1)
        );
        assert!(matches!(&pipeline.code[5], PipelineCode::Finish));
    }

    #[test]
    fn seal_rejects_slot_range_cycle_identity_and_series_corruption() {
        let source = r#"
def process leaf { drop }
def process root { process leaf }
def pipeline p { process root; finish }
"#;

        let mut bad_slot = compile(source);
        *bad_slot
            .first_call_slot_mut_for_testing("root")
            .expect("root call") = EdgeSlot(u32::MAX);
        assert!(seal_error(bad_slot).contains("call edge slot is out of range"));

        let mut cycle = compile(source);
        let root = cycle.process_names["root"];
        cycle.process_bodies[root.index()].edges[0].target = ProcessTarget::Known(root);
        assert!(seal_error(cycle).contains("process call cycle"));

        let mut bad_child = compile(source);
        pipeline_mut(&mut bad_child, "p").metric_nodes[0].children[0] = MetricNodeId(u32::MAX);
        assert!(seal_error(bad_child).contains("metric child id is out of range"));

        let mut bad_identity = compile(source);
        pipeline_mut(&mut bad_identity, "p").metric_nodes[1].process_name = "wrong".to_owned();
        assert!(seal_error(bad_identity).contains("metric child identity mismatch"));

        let mut bad_target = compile(source);
        pipeline_mut(&mut bad_target, "p").metric_nodes[1].target = ProcessTarget::Unknown;
        assert!(seal_error(bad_target).contains("metric child identity mismatch"));

        let mut bad_site_kind = compile(source);
        let PipelineCode::ProcessChain(sites) = &mut pipeline_mut(&mut bad_site_kind, "p").code[0]
        else {
            panic!("first statement must be a process chain");
        };
        sites[0].kind = SiteKind::Inline;
        assert!(seal_error(bad_site_kind).contains("process site kind mismatch"));

        let mut bad_subscription = compile("def pipeline p { input a, b; finish }");
        pipeline_mut(&mut bad_subscription, "p")
            .subscription_inputs
            .clear();
        assert!(seal_error(bad_subscription).contains("subscription input identity mismatch"));

        let mut unknown_with_child = compile(
            "def process parent { process missing } def pipeline p { process parent; finish }",
        );
        pipeline_mut(&mut unknown_with_child, "p").metric_nodes[1]
            .children
            .push(MetricNodeId(0));
        assert!(seal_error(unknown_with_child).contains("unknown metric node has children"));

        let mut bad_series = compile(source);
        pipeline_mut(&mut bad_series, "p").metric_nodes[1].step = 1;
        assert!(seal_error(bad_series).contains("metric step sequence mismatch"));

        let mut bad_root_selection = compile(source);
        let PipelineCode::ProcessChain(sites) =
            &mut pipeline_mut(&mut bad_root_selection, "p").code[0]
        else {
            panic!("first statement must be a process chain");
        };
        sites[0].metric_node = MetricNodeId(u32::MAX);
        assert!(
            seal_error(bad_root_selection).contains("root metric node is out of range"),
            "corrupt runtime frame selection must fail during seal"
        );

        let mut bad_timer_selection = compile("def pipeline timer { output sink; finish }");
        let PipelineCode::Output { timer_slot, .. } =
            &mut pipeline_mut(&mut bad_timer_selection, "timer").code[0]
        else {
            panic!("first statement must be output");
        };
        *timer_slot = OutputTimerSlot(u32::MAX);
        assert!(
            seal_error(bad_timer_selection).contains("output timer identity mismatch"),
            "corrupt output selection must fail during seal"
        );

        if usize::BITS > 32 {
            let overflow = (u32::MAX as usize) + 1;
            assert!(ProcessBodyId::from_index(overflow).is_err());
            assert!(EdgeSlot::from_index(overflow).is_err());
            assert!(MetricNodeId::from_index(overflow).is_err());
            assert!(OutputTimerSlot::from_index(overflow).is_err());
        }
    }

    #[test]
    fn bind_uses_fresh_registry_and_descriptor_order_without_blueprint_handles() {
        let source = r#"
def process leaf { drop }
def process parent { process leaf }
def pipeline p {
    process parent
    process leaf
    output sink
    if true { output sink } else { output other }
    finish
}
"#;
        let config = CompiledConfig::from_config(parse_config(source).expect("parse bind fixture"))
            .expect("compile bind fixture");
        let blueprint = Arc::new(
            UnsealedRuntimeBlueprint::compile(&config)
                .expect("compile blueprint")
                .seal()
                .expect("seal blueprint"),
        );
        let registry_one = crate::metrics::Registry::new();
        let registry_two = crate::metrics::Registry::new();
        reset_bound_pipeline_execution_constructions_for_testing();
        let bound_one = blueprint.bind(&registry_one).expect("first fresh bind");
        assert_eq!(bound_pipeline_execution_constructions_for_testing(), 1);
        let bound_two = blueprint.bind(&registry_two).expect("second fresh bind");
        assert_eq!(bound_pipeline_execution_constructions_for_testing(), 2);
        assert!(Arc::ptr_eq(&bound_one.blueprint, &blueprint));
        assert!(Arc::ptr_eq(&bound_two.blueprint, &blueprint));

        let pipeline_id = blueprint.pipeline_id("p").expect("pipeline id");
        let descriptor = blueprint.pipeline("p").expect("pipeline descriptor");
        let first = bound_one
            .pipeline_execution(pipeline_id)
            .expect("first bound pipeline");
        let second = bound_two
            .pipeline_execution(pipeline_id)
            .expect("second bound pipeline");
        assert!(!Arc::ptr_eq(first, second));
        assert!(Arc::ptr_eq(first.pipeline_arc(), second.pipeline_arc()));
        assert_eq!(
            first.metrics().process_counters.len(),
            descriptor.metric_nodes.len()
        );
        assert_eq!(
            second.metrics().process_counters.len(),
            descriptor.metric_nodes.len()
        );
        assert_eq!(
            first.metrics().output_timers.len(),
            descriptor.output_timers.len()
        );
        assert_eq!(
            second.metrics().output_timers.len(),
            descriptor.output_timers.len()
        );
        assert_eq!(descriptor.output_timers, ["sink", "other"]);

        for (index, counter) in first.metrics().process_counters.iter().enumerate() {
            for _ in 0..=index {
                counter.start();
            }
        }
        for (index, node) in descriptor.metric_nodes.iter().enumerate() {
            let labels = serde_json::json!({
                "pipeline": "p",
                "step": node.step.to_string(),
                "process_path": node.process_path,
                "process_name": node.process_name,
            });
            assert_eq!(
                series_value(&registry_one, "limpid_process_events_in_total", &labels),
                (index + 1) as u64,
                "handle array order must be descriptor order"
            );
            assert_eq!(
                series_value(&registry_two, "limpid_process_events_in_total", &labels),
                0,
                "fresh registries must own independent counters"
            );
        }

        first.metrics().output_timers[0].observe_between(
            crate::time::UnixNanos::new(10),
            crate::time::UnixNanos::new(20),
        );
        assert_eq!(
            histogram_count(
                &registry_one,
                "limpid_pipeline_processing_seconds",
                &serde_json::json!({ "pipeline": "p", "output": "sink" }),
            ),
            1
        );
        assert_eq!(
            histogram_count(
                &registry_one,
                "limpid_pipeline_processing_seconds",
                &serde_json::json!({ "pipeline": "p", "output": "other" }),
            ),
            0
        );

        let duplicate = match blueprint.bind(&registry_one) {
            Ok(_) => panic!("duplicate series unexpectedly rebound"),
            Err(error) => error.to_string(),
        };
        assert!(
            duplicate.contains("duplicate"),
            "unexpected bind error: {duplicate}"
        );

        let production = production_prefix(include_str!("blueprint.rs"));
        assert_eq!(
            declared_fields(production, "pub(crate) struct RuntimeBlueprint"),
            ["unsealed: UnsealedRuntimeBlueprint,"]
        );
        assert_eq!(
            declared_fields(production, "pub(crate) struct UnsealedRuntimeBlueprint"),
            [
                "pub(crate) node_id: Option<String>,",
                "pub(crate) node_key: Option<String>,",
                "pub(crate) inputs: HashMap<String, InputDef>,",
                "pub(crate) outputs: HashMap<String, OutputDef>,",
                "pub(crate) functions: HashMap<String, FunctionDef>,",
                "pub(crate) global_blocks: HashMap<String, Vec<Property>>,",
                "pub(crate) process_bodies: Vec<ProcessBodyCode>,",
                "pub(crate) process_names: BTreeMap<String, ProcessBodyId>,",
                "pub(crate) pipelines: BTreeMap<String, Arc<PipelineBlueprint>>,",
                "pub(crate) pipeline_ids: BTreeMap<String, PipelineId>,",
                "pub(crate) pipeline_order: Vec<String>,",
            ],
            "unsealed authority may retain declarative data only"
        );
        let private_handle_mutant = production.replacen(
            "pub(crate) struct UnsealedRuntimeBlueprint {",
            "pub(crate) struct UnsealedRuntimeBlueprint {\n    registry: Registry,",
            1,
        );
        assert_ne!(
            declared_fields(
                &private_handle_mutant,
                "pub(crate) struct UnsealedRuntimeBlueprint"
            ),
            declared_fields(production, "pub(crate) struct UnsealedRuntimeBlueprint"),
            "the field allowlist must independently detect a private live-handle field"
        );
        let continuation_mutant = production.replacen(
            "pub(crate) struct UnsealedRuntimeBlueprint {",
            "pub(crate) struct UnsealedRuntimeBlueprint {\n    callbacks: Vec<\n        Registry: Clone,\n    >,",
            1,
        );
        let continuation_fields = declared_fields(
            &continuation_mutant,
            "pub(crate) struct UnsealedRuntimeBlueprint",
        );
        assert!(
            continuation_fields
                .iter()
                .any(|field| field == "callbacks: Vec<")
        );
        assert!(
            continuation_fields
                .iter()
                .all(|field| field != "Registry: Clone,"),
            "generic continuations must not be mistaken for private fields"
        );
        for (declaration, expected) in [
            (
                "pub(crate) struct ProcessSite",
                vec![
                    "pub(crate) name: String,",
                    "pub(crate) kind: SiteKind,",
                    "pub(crate) body: ProcessBodyId,",
                    "pub(crate) metric_node: MetricNodeId,",
                ],
            ),
            (
                "pub(crate) struct ProcessEdge",
                vec![
                    "pub(crate) name: String,",
                    "pub(crate) target: ProcessTarget,",
                ],
            ),
            (
                "pub(crate) struct ProcessBodyCode",
                vec![
                    "pub(crate) name: String,",
                    "pub(crate) code: Vec<ProcessCode>,",
                    "pub(crate) edges: Vec<ProcessEdge>,",
                ],
            ),
            (
                "pub(crate) struct MetricNodeDescriptor",
                vec![
                    "pub(crate) parent: Option<MetricNodeId>,",
                    "pub(crate) step: u32,",
                    "pub(crate) process_path: String,",
                    "pub(crate) process_name: String,",
                    "pub(crate) target: ProcessTarget,",
                    "pub(crate) children: Vec<MetricNodeId>,",
                ],
            ),
            (
                "pub(crate) struct PipelineBlueprint",
                vec![
                    "pub(crate) name: String,",
                    "pub(crate) code: Vec<PipelineCode>,",
                    "pub(crate) flow: PipelineFlow,",
                    "pub(crate) subscription_inputs: Vec<String>,",
                    "pub(crate) metric_nodes: Vec<MetricNodeDescriptor>,",
                    "pub(crate) output_timers: Vec<String>,",
                ],
            ),
            (
                "pub(crate) struct PipelineFlow",
                vec![
                    "pub(crate) inputs: Vec<String>,",
                    "pub(crate) processes: Vec<String>,",
                    "pub(crate) outputs: Vec<String>,",
                ],
            ),
        ] {
            assert_eq!(declared_fields(production, declaration), expected);
        }
        for forbidden in [
            "Registry",
            "ProcessCounters",
            "PipelineOutputTimer",
            "JoinHandle",
            "TcpStream",
            "UdpSocket",
            "TableStore",
        ] {
            assert!(
                !production
                    .split("pub(crate) struct ProcessEdge")
                    .nth(1)
                    .expect("nested blueprint storage")
                    .split("pub(crate) struct BoundRuntimeBlueprint")
                    .next()
                    .expect("unbound storage boundary")
                    .contains(forbidden),
                "unbound nested storage retained a live handle: {forbidden}"
            );
        }
    }

    #[test]
    fn bound_pipeline_execution_rejects_shape_corruption_before_runtime() {
        let config = CompiledConfig::from_config(
            parse_config("def pipeline p { process { drop }; output sink; finish }")
                .expect("parse shape fixture"),
        )
        .expect("compile shape fixture");
        let blueprint = compile_runtime_blueprint(&config).expect("compile blueprint");
        let pipeline_id = blueprint.pipeline_id("p").expect("pipeline id");
        let pipeline = blueprint.pipeline_arc(pipeline_id).expect("pipeline arc");
        let error = BoundPipelineExecution::new(
            Arc::clone(&blueprint),
            Arc::clone(pipeline),
            BoundPipelineMetrics {
                process_counters: Vec::new(),
                output_timers: Vec::new(),
            },
        )
        .err()
        .expect("shape corruption must fail at bind time");
        assert!(error.to_string().contains("metric binding shape mismatch"));
    }

    #[test]
    fn deep_process_calls_switch_metric_frames_through_edge_slots() {
        let execution = include_str!("execution.rs");
        assert!(
            execution.contains("let child_node = *current_node")
                && execution.contains(".children\n                    .get(edge_slot.index())"),
            "nested calls must switch the current metric frame through the compiled EdgeSlot"
        );
        let ir_process = &execution[execution
            .find("fn exec_ir_process_code")
            .expect("single IR process executor")
            ..execution
                .find("fn exec_ir_pipeline_body")
                .expect("single IR pipeline executor")];
        assert!(
            !ir_process.contains(concat!("metric_", "stmts: Option<&[")),
            "execution must not zip semantic statements with a metric shadow body"
        );
    }

    #[test]
    fn pipeline_control_flow_is_owned_by_the_single_ir_in_source_order() {
        let blueprint = production_prefix(include_str!("blueprint.rs"));
        for required in [
            "pub(crate) struct RuntimeBlueprint",
            "pub(crate) enum PipelineCode",
            "pub(crate) enum ProcessCode",
            "PipelineCode::If",
            "PipelineCode::Switch",
            "ProcessCode::TryCatch",
        ] {
            assert!(
                blueprint.contains(required),
                "single IR is missing required control-flow owner: {required}"
            );
        }
    }

    #[test]
    fn daemon_workers_and_grouping_are_blueprint_identity_driven() {
        let worker = include_str!("../runtime/pipeline_worker.rs");
        let production_worker = production_prefix(worker);
        let fields = production_worker
            .split("pub(super) struct PipelineWorker")
            .nth(1)
            .expect("pipeline worker declaration")
            .split("}\n\n")
            .next()
            .expect("pipeline worker fields");
        let production_fields = fields.replace(
            "#[cfg(test)]\n    pub(super) serial_test_gate: Option<Arc<tokio::sync::Barrier>>,",
            "",
        );
        assert!(production_fields.contains("Arc<crate::pipeline::BoundPipelineExecution>"));
        for forbidden in ["PipelineDef", "ProcessDef", "Option<"] {
            assert!(
                !production_fields.contains(forbidden),
                "daemon worker retained legacy execution authority: {forbidden}"
            );
        }
        assert!(production_worker.contains("run_pipeline_blueprint_resolved_at"));

        let config = CompiledConfig::from_config(
            parse_config(
                "def pipeline z { input first, second; finish } def pipeline a { input first; finish }",
            )
            .expect("parse grouping fixture"),
        )
        .expect("compile grouping fixture");
        let blueprint = compile_runtime_blueprint(&config).expect("compile grouping blueprint");
        let (_, a) = blueprint.pipelines().next().expect("sorted first pipeline");
        assert_eq!(a.name, "a");
        assert_eq!(a.flow.inputs, ["first"]);
        let (_, z) = blueprint
            .pipelines()
            .nth(1)
            .expect("sorted second pipeline");
        assert_eq!(z.name, "z");
        assert_eq!(z.flow.inputs, ["first", "second"]);

        let startup = include_str!("../runtime/startup.rs");
        assert!(startup.contains("for (pipeline_id, pipeline) in blueprint.pipelines()"));
        assert!(startup.contains("for input_name in routing_inputs(pipeline)"));
        assert!(startup.contains("&pipeline.subscription_inputs"));
        assert!(!startup.contains("for pipeline_def in config.pipelines.values()"));

        let context = production_prefix(include_str!("../runtime/pipeline_worker.rs"));
        let context_fields = declared_fields(context, "pub(super) struct PipelineContext");
        assert!(
            !context_fields.iter().any(|field| field.contains("config")),
            "test-only CompiledConfig retention must not survive in PipelineContext"
        );
    }

    #[test]
    fn subscription_inputs_preserve_the_first_top_level_input_contract() {
        let config = CompiledConfig::from_config(
            parse_config(
                r#"
def pipeline p {
    input a
    input b
    if true { input c; finish } else { finish }
}
def pipeline fan_in { input a, b, c; finish }
"#,
            )
            .expect("parse routing fixture"),
        )
        .expect("compile routing fixture");
        let blueprint = compile_runtime_blueprint(&config).expect("compile routing blueprint");
        let p = blueprint.pipeline("p").expect("pipeline p");
        assert_eq!(
            p.flow.inputs,
            ["a", "b", "c"],
            "control flow is a recursive union"
        );
        assert_eq!(
            p.subscription_inputs,
            ["a"],
            "runtime routing must use only the first top-level input statement"
        );
        let fan_in = blueprint.pipeline("fan_in").expect("fan-in pipeline");
        assert_eq!(fan_in.subscription_inputs, ["a", "b", "c"]);
    }

    #[test]
    fn daemon_hot_path_borrows_one_pipeline_identity_without_name_allocation() {
        let worker = production_prefix(include_str!("../runtime/pipeline_worker.rs"));
        let inner = worker
            .split("pub(super) async fn run_pipeline_with_outputs_inner")
            .nth(1)
            .expect("daemon pipeline runner")
            .split("async fn enqueue_pipeline_outputs")
            .next()
            .expect("runner body");
        for forbidden in ["pipeline.name.clone()", "pipeline.name.to_string()"] {
            assert!(
                !inner.contains(forbidden),
                "daemon success path allocates the pipeline name via {forbidden}"
            );
        }
        assert_eq!(
            inner.matches("pipeline_by_id(").count(),
            0,
            "the caller must resolve pipeline identity once and lend it to the runner"
        );

        let process_event = worker
            .split("pub(super) async fn process_event_at")
            .nth(1)
            .expect("process_event_at")
            .split("pub(super) async fn write_errored_to_dlq")
            .next()
            .expect("process_event_at body");
        assert_eq!(
            process_event.matches("pipeline_by_id(").count(),
            0,
            "event dispatch must use the startup-resolved pipeline descriptor"
        );
        assert_eq!(
            process_event.matches("pipeline_id(").count(),
            0,
            "event dispatch must not re-resolve a pipeline name through the id map"
        );
        assert_eq!(
            process_event.matches("pipeline_metrics_resolved(").count(),
            0,
            "event dispatch must not resolve metric handles by pipeline name"
        );
        assert!(!process_event.contains("metric binding shape mismatch"));
        assert_eq!(
            process_event.matches("Arc::clone(").count(),
            0,
            "event dispatch must borrow the worker descriptor without a refcount bump"
        );

        let execution = include_str!("execution.rs");
        let resolved = execution
            .split("pub(crate) fn run_pipeline_blueprint_resolved_at")
            .nth(1)
            .expect("resolved pipeline runner")
            .split("struct IrProcessRegistry")
            .next()
            .expect("resolved runner body");
        for forbidden in [
            "pipeline_id(",
            "pipeline_by_id(",
            "pipeline_metrics_resolved(",
            "metric binding shape mismatch",
            "process_counters.len()",
            "output_timers.len()",
            "Arc::clone(",
        ] {
            assert!(
                !resolved.contains(forbidden),
                "resolved event runner retained hot-path work: {forbidden}"
            );
        }
    }

    #[test]
    fn final_source_shape_has_no_legacy_metric_shadow_or_plain_dispatch() {
        let exec = include_str!("../dsl/exec.rs");
        let pipeline = include_str!("execution.rs");
        let worker = include_str!("../runtime/pipeline_worker.rs");
        let runtime = include_str!("../runtime.rs");
        for forbidden in [
            concat!("ProcessMetric", "Statement"),
            concat!("ProcessRegistry", "Dispatch::", "Plain"),
            concat!("exec_process_body", "_with_", "metric_", "plan"),
        ] {
            assert!(
                !exec.contains(forbidden),
                "legacy process surface remains: {forbidden}"
            );
        }
        for forbidden in [
            concat!("PipelineMetric", "Statement"),
            concat!("ProcessMetric", "Statement"),
            concat!("PipelineProcess", "Metrics"),
            concat!("RawPipelineProcess", "Metrics"),
            concat!("ProcessRegistry", "Dispatch"),
            concat!("metric_", "stmts"),
            concat!("metric_", "plan"),
            concat!("run_pipeline_with_", "process_metrics"),
        ] {
            assert!(
                !pipeline.contains(forbidden),
                "legacy pipeline surface remains: {forbidden}"
            );
            assert!(
                !worker.contains(forbidden),
                "legacy worker surface remains: {forbidden}"
            );
            assert!(
                !runtime.contains(forbidden),
                "legacy runtime surface remains: {forbidden}"
            );
        }
    }
}

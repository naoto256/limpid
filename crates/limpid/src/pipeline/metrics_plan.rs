use super::*;

/// Pre-compiled metric plan for a pipeline's process invocations.
/// The plan is validated once at startup, so event execution uses
/// only O(1) token lookups. Validation rejects shape or identity
/// mismatches before worker construction; defensive checked access
/// returns an internal error instead of silently attributing an
/// invocation to the wrong metric series.
pub(crate) struct PipelineProcessMetrics {
    pub(super) nodes: Vec<ProcessMetricNode>,
    pub(super) statements: Vec<PipelineMetricStatement>,
    #[cfg(test)]
    selection_trap: std::sync::Arc<MetricNodeSelectionTrapState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;

    #[test]
    fn compiled_plan_rejects_recursive_process_graphs() {
        let config = CompiledConfig::from_config(
            parse_config(
                r#"
def process recurse { process recurse }
def pipeline p { process recurse; finish }
"#,
            )
            .unwrap(),
        )
        .unwrap();
        let pipeline = config.pipelines.get("p").unwrap();
        let registry = crate::metrics::Registry::new();
        let error = match PipelineProcessMetrics::register(pipeline, &config.processes, &registry) {
            Ok(_) => panic!("recursive metric plan unexpectedly compiled"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("process call cycle"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn parsed_branch_bodies_keep_their_enclosing_statement_kind() {
        let config = CompiledConfig::from_config(
            parse_config(
                r#"
def process nested {
    if true { drop } else { drop }
}
def pipeline p {
    if true { process nested } else { finish }
}
"#,
            )
            .unwrap(),
        )
        .unwrap();

        let pipeline = config.pipelines.get("p").unwrap();
        let PipelineStatement::If(pipeline_if) = &pipeline.body[0] else {
            panic!("expected pipeline if statement");
        };
        assert!(pipeline_if.branches.iter().all(|(_, body)| {
            body.iter()
                .all(|item| matches!(item, BranchBody::Pipeline(_)))
        }));
        assert!(
            pipeline_if
                .else_body
                .as_ref()
                .unwrap()
                .iter()
                .all(|item| matches!(item, BranchBody::Pipeline(_)))
        );

        let process = config.processes.get("nested").unwrap();
        let ProcessStatement::If(process_if) = &process.body[0] else {
            panic!("expected process if statement");
        };
        assert!(process_if.branches.iter().all(|(_, body)| {
            body.iter()
                .all(|item| matches!(item, BranchBody::Process(_)))
        }));
        assert!(
            process_if
                .else_body
                .as_ref()
                .unwrap()
                .iter()
                .all(|item| matches!(item, BranchBody::Process(_)))
        );

        PipelineProcessMetrics::register(
            pipeline,
            &config.processes,
            &crate::metrics::Registry::new(),
        )
        .expect("valid parsed branches must register");
    }

    #[test]
    fn inline_token_mutant_targets_inline_identity_after_named_step() {
        let config = CompiledConfig::from_config(
            parse_config(
                "def process named { drop } def pipeline p { process named | { drop }; finish }",
            )
            .unwrap(),
        )
        .unwrap();
        let pipeline = config.pipelines.get("p").unwrap();
        let mut raw = PipelineProcessMetrics::compile_raw(
            pipeline,
            &config.processes,
            &crate::metrics::Registry::new(),
        )
        .unwrap();
        let named_token = raw
            .identities
            .iter()
            .position(|identity| {
                matches!(&identity.kind, ProcessMetricNodeKind::Named(name) if name == "named")
            })
            .unwrap();
        let inline_token = raw
            .identities
            .iter()
            .position(|identity| identity.kind == ProcessMetricNodeKind::Inline)
            .unwrap();

        let PipelineMetricStatement::ProcessChain(tokens) = &raw.statements[0] else {
            panic!("expected process chain plan");
        };
        assert_eq!(tokens, &[named_token, inline_token]);

        raw.invalidate_first_inline_token_for_testing();

        let PipelineMetricStatement::ProcessChain(tokens) = &raw.statements[0] else {
            panic!("expected process chain plan");
        };
        assert_eq!(tokens, &[named_token, usize::MAX]);
    }
}

pub(super) struct ProcessMetricNode {
    pub(super) counters: crate::metrics::ProcessCounters,
    pub(super) body_plan: Vec<ProcessMetricStatement>,
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

pub(super) enum PipelineMetricStatement {
    None,
    Output(crate::metrics::PipelineOutputTimer),
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
            output_timers: HashMap::new(),
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

    pub(super) fn select_node(&self, token: usize) -> Option<&ProcessMetricNode> {
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
        let Some(inline_token) = self
            .identities
            .iter()
            .position(|identity| identity.kind == ProcessMetricNodeKind::Inline)
        else {
            return;
        };
        for statement in &mut self.statements {
            if let PipelineMetricStatement::ProcessChain(tokens) = statement
                && let Some(token) = tokens.iter_mut().find(|token| **token == inline_token)
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
            (PipelineStatement::Output(_), PipelineMetricStatement::Output(_)) => {}
            (
                PipelineStatement::Input(_)
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
                    let step = self.next_step;
                    self.next_step += 1;
                    let identity = self.raw.identities.get(*token).ok_or_else(|| {
                        anyhow::anyhow!("compiled nested process token is out of range")
                    })?;
                    if identity.parent != Some(parent)
                        || identity.step != step
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
    output_timers: HashMap<String, crate::metrics::PipelineOutputTimer>,
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
            PipelineStatement::Output(name) => {
                let timer = match self.output_timers.get(name) {
                    Some(timer) => timer.clone(),
                    None => {
                        let timer = crate::metrics::PipelineOutputTimer::register(
                            self.registry,
                            self.pipeline,
                            name,
                        )?;
                        self.output_timers.insert(name.clone(), timer.clone());
                        timer
                    }
                };
                Ok(PipelineMetricStatement::Output(timer))
            }
            PipelineStatement::ProcessChain(chain) => {
                let mut nodes = Vec::with_capacity(chain.len());
                for element in chain {
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
                        None,
                        kind,
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
        parent: Option<usize>,
        kind: ProcessMetricNodeKind,
        name: &str,
        path: String,
        body: Option<&[ProcessStatement]>,
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<usize, crate::metrics::MetricsError> {
        let step = self.next_step;
        self.next_step += 1;
        let id = self.nodes.len();
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
        self.identities
            .push(ProcessMetricNodeIdentity { parent, step, kind });
        self.children.push(HashMap::new());
        ancestors.push((name.to_owned(), id));
        if let Some(body) = body {
            self.nodes[id].body_plan = self.process_body(id, &path, body, ancestors)?;
        }
        ancestors.pop();
        Ok(id)
    }

    fn process_body(
        &mut self,
        parent: usize,
        parent_path: &str,
        body: &[ProcessStatement],
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<Vec<ProcessMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|statement| self.process_statement(parent, parent_path, statement, ancestors))
            .collect()
    }

    fn process_statement(
        &mut self,
        parent: usize,
        parent_path: &str,
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
                        Some(parent),
                        ProcessMetricNodeKind::Named(name.clone()),
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
                    .map(|(_, body)| self.process_branch(parent, parent_path, body, ancestors))
                    .collect::<Result<Vec<_>, _>>()?;
                let else_body = chain
                    .else_body
                    .as_deref()
                    .map(|body| self.process_branch(parent, parent_path, body, ancestors))
                    .transpose()?;
                Ok(ProcessMetricStatement::If {
                    branches,
                    else_body,
                })
            }
            ProcessStatement::Switch(_, arms) => Ok(ProcessMetricStatement::Switch(
                arms.iter()
                    .map(|arm| self.process_branch(parent, parent_path, &arm.body, ancestors))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            ProcessStatement::TryCatch(try_body, catch_body) => {
                Ok(ProcessMetricStatement::TryCatch {
                    try_body: self.process_body(parent, parent_path, try_body, ancestors)?,
                    catch_body: self.process_body(parent, parent_path, catch_body, ancestors)?,
                })
            }
            _ => Ok(ProcessMetricStatement::None),
        }
    }

    fn process_branch(
        &mut self,
        parent: usize,
        parent_path: &str,
        body: &[BranchBody],
        ancestors: &mut Vec<(String, usize)>,
    ) -> Result<Vec<ProcessMetricStatement>, crate::metrics::MetricsError> {
        body.iter()
            .map(|item| match item {
                BranchBody::Process(statement) => {
                    self.process_statement(parent, parent_path, statement, ancestors)
                }
                BranchBody::Pipeline(_) => Ok(ProcessMetricStatement::None),
            })
            .collect()
    }
}

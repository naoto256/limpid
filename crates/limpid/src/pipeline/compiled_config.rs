use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};

use crate::dsl::ast::*;

/// A fully resolved configuration ready for execution.
#[derive(Clone)]
pub struct CompiledConfig {
    pub(crate) node_id: Option<String>,
    pub(crate) node_key: Option<String>,
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

        Ok(Self {
            node_id: config.node_id,
            node_key: config.node_key,
            inputs,
            outputs,
            processes,
            pipelines,
            functions,
            global_blocks,
        })
    }

    /// Validate cross-references: all referenced inputs, outputs, and processes exist.
    pub fn validate(&self) -> Result<()> {
        if self.node_key.is_none()
            && (self
                .outputs
                .values()
                .any(|output| output.properties.type_name() == "ltp")
                || self
                    .inputs
                    .values()
                    .any(|input| input.properties.type_name() == "ltp"))
        {
            bail!("module type 'ltp' requires top-level node_key");
        }
        for (name, input) in &self.inputs {
            if input.properties.type_name() == "ltp" {
                crate::modules::input::ltp::validate_static_properties(
                    name,
                    input.properties.user_properties(),
                )?;
            }
        }
        crate::modules::input::ltp::validate_listener_groups(
            self.inputs
                .iter()
                .filter(|(_, input)| input.properties.type_name() == "ltp")
                .map(|(name, input)| (name.as_str(), input.properties.user_properties())),
        )?;
        for (name, output) in &self.outputs {
            if output.properties.type_name() == "ltp" {
                crate::modules::output::ltp::validate_static_properties(
                    name,
                    output.properties.user_properties(),
                )?;
            }
        }
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
                let mut seen = HashSet::new();
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
                        if let BranchBody::Pipeline(statement) = item {
                            self.validate_pipeline_stmt(pipeline_name, statement)?;
                        }
                    }
                }
                if let Some(else_body) = &if_chain.else_body {
                    for item in else_body {
                        if let BranchBody::Pipeline(statement) = item {
                            self.validate_pipeline_stmt(pipeline_name, statement)?;
                        }
                    }
                }
            }
            PipelineStatement::Switch(_, arms) => {
                for arm in arms {
                    for item in &arm.body {
                        if let BranchBody::Pipeline(statement) = item {
                            self.validate_pipeline_stmt(pipeline_name, statement)?;
                        }
                    }
                }
            }
            PipelineStatement::Drop | PipelineStatement::Finish | PipelineStatement::Error(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;

    fn compile(source: &str) -> Result<CompiledConfig> {
        CompiledConfig::from_config(parse_config(source)?)
    }

    #[test]
    fn preserves_node_key_path_without_normalizing_it() {
        let config = compile(r#"node_key "../identity/node.pem""#).unwrap();
        assert_eq!(config.node_key.as_deref(), Some("../identity/node.pem"));
    }

    #[test]
    fn validates_fan_in_input_references_and_duplicates() {
        let unknown = compile(
            r#"
def input a { type syslog_udp bind "0.0.0.0:5140" }
def output o { type file path "/tmp/x.log" }
def pipeline p { input a, missing; output o; drop }
"#,
        )
        .unwrap()
        .validate()
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("unknown input 'missing'"));

        let duplicate = compile(
            r#"
def input a { type syslog_udp bind "0.0.0.0:5140" }
def output o { type file path "/tmp/x.log" }
def pipeline p { input a, a; output o; drop }
"#,
        )
        .unwrap()
        .validate()
        .unwrap_err()
        .to_string();
        assert!(duplicate.contains("listed more than once"));
    }
}

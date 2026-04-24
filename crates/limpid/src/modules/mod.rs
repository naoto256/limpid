//! Module system: traits, registry, and implementations for input, output,
//! and process modules.
//!
//! `ModuleRegistry` maps type names to factory functions.
//! Runtime resolves type names from DSL config through the registry
//! instead of hardcoded match arms.
//!
//! This is the extension point for future dynamic (.so) module loading.

pub mod input;
pub mod output;
pub mod schema;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::dsl::ast::Property;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::{InputMetrics, OutputMetrics};
use crate::queue::OutputWriter;

/// Common trait for every limpid module (input, output).
///
/// Modules only need to know how to construct themselves from DSL
/// properties. Schema information for the static analyzer is attached
/// to parsers and function signatures (see `check::` and
/// `functions::FunctionSig`), not to modules — inputs and outputs are
/// I/O-pure (ingress bytes in, egress bytes out) and have no data
/// contract to advertise.
///
/// Processes are not modules: v0.3.0 Block 4 removed the native
/// process layer entirely in favour of DSL functions (`syslog.parse`
/// etc.) and user-defined `def process { ... }` blocks. Modules are
/// only inputs and outputs.
pub trait Module: Sized {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self>;
}

/// All modules expose their own metrics.
pub trait HasMetrics {
    type Stats;
    fn metrics(&self) -> Arc<Self::Stats>;
}

#[async_trait::async_trait]
pub trait Input: Module + HasMetrics<Stats = InputMetrics> + Send + 'static {
    async fn run(
        self,
        tx: mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()>;
}

#[async_trait::async_trait]
pub trait Output: Module + HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static {
    async fn write(&self, event: &Event) -> Result<()>;

    /// Called once after construction to hand the output a reference to
    /// the pipeline's `FunctionRegistry`. Outputs that evaluate DSL
    /// expressions at write time (e.g. `${...}` templates in a path)
    /// override this to stash the registry. Default: no-op.
    fn attach_funcs(&mut self, _funcs: Arc<FunctionRegistry>) {}
}

// ---------------------------------------------------------------------------
// Factory return types
// ---------------------------------------------------------------------------

/// Returned by input factory: the spawned task handle + metrics handle.
pub struct CreatedInput {
    pub handle: tokio::task::JoinHandle<()>,
    pub metrics: Arc<InputMetrics>,
}

/// Returned by output factory: the writer + metrics handle.
pub struct CreatedOutput {
    pub writer: Box<dyn OutputWriter>,
    pub metrics: Arc<OutputMetrics>,
}

// ---------------------------------------------------------------------------
// Factory function types
// ---------------------------------------------------------------------------

type InputFactory = Box<
    dyn Fn(
            &str,
            &[Property],
            mpsc::Sender<Event>,
            tokio::sync::watch::Receiver<bool>,
        ) -> Result<CreatedInput>
        + Send
        + Sync,
>;

type OutputFactory =
    Box<dyn Fn(&str, &[Property], Arc<FunctionRegistry>) -> Result<CreatedOutput> + Send + Sync>;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct ModuleRegistry {
    inputs: HashMap<String, InputFactory>,
    outputs: HashMap<String, OutputFactory>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            outputs: HashMap::new(),
        }
    }

    pub fn register_input<F>(&mut self, type_name: &str, factory: F)
    where
        F: Fn(
                &str,
                &[Property],
                mpsc::Sender<Event>,
                tokio::sync::watch::Receiver<bool>,
            ) -> Result<CreatedInput>
            + Send
            + Sync
            + 'static,
    {
        self.inputs.insert(type_name.to_string(), Box::new(factory));
    }

    pub fn register_output<F>(&mut self, type_name: &str, factory: F)
    where
        F: Fn(&str, &[Property], Arc<FunctionRegistry>) -> Result<CreatedOutput>
            + Send
            + Sync
            + 'static,
    {
        self.outputs
            .insert(type_name.to_string(), Box::new(factory));
    }

    pub fn create_input(
        &self,
        type_name: &str,
        name: &str,
        properties: &[Property],
        tx: mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<CreatedInput> {
        let factory = self
            .inputs
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("unknown input type: {}", type_name))?;
        factory(name, properties, tx, shutdown)
    }

    pub fn create_output(
        &self,
        type_name: &str,
        name: &str,
        properties: &[Property],
        funcs: Arc<FunctionRegistry>,
    ) -> Result<CreatedOutput> {
        let factory = self
            .outputs
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("unknown output type: {}", type_name))?;
        factory(name, properties, funcs)
    }
}

// ---------------------------------------------------------------------------
// Built-in module registration
// ---------------------------------------------------------------------------

pub fn register_builtins(registry: &mut ModuleRegistry) {
    // Inputs
    register_input_type::<input::syslog_udp::SyslogUdpInput>(registry, "syslog_udp");
    register_input_type::<input::syslog_tcp::SyslogTcpInput>(registry, "syslog_tcp");
    register_input_type::<input::syslog_tls::SyslogTlsInput>(registry, "syslog_tls");
    register_input_type::<input::tail::TailInput>(registry, "tail");
    register_input_type::<input::unix_socket::UnixSocketInput>(registry, "unix_socket");
    #[cfg(feature = "journal")]
    register_input_type::<input::journal::JournalInput>(registry, "journal");

    // Outputs
    register_output_type::<output::file::FileOutput>(registry, "file");
    register_output_type::<output::unix_socket::UnixSocketOutput>(registry, "unix_socket");
    register_output_type::<output::tcp::TcpOutput>(registry, "tcp");
    register_output_type::<output::http::HttpOutput>(registry, "http");
    register_output_type::<output::udp::UdpOutput>(registry, "udp");
    register_output_type::<output::stdout::StdoutOutput>(registry, "stdout");
    #[cfg(feature = "kafka")]
    register_output_type::<output::kafka::KafkaOutput>(registry, "kafka");

    // No built-in processes — v0.3.0 Block 4 removed the native process
    // layer. Schema-specific parsers are DSL functions (`syslog.parse`,
    // `cef.parse`), format primitives are flat functions (`parse_json`,
    // `parse_kv`, `regex_replace`, …), and custom transforms are
    // user-defined via `def process { ... }`.
}

fn register_input_type<T>(registry: &mut ModuleRegistry, type_name: &str)
where
    T: Input + Send + 'static,
{
    registry.register_input(type_name, |name, properties, tx, shutdown| {
        let input = T::from_properties(name, properties)?;
        let metrics = HasMetrics::metrics(&input);
        let input_name = name.to_string();
        let handle = tokio::spawn(async move {
            if let Err(e) = Input::run(input, tx, shutdown).await {
                tracing::error!("input '{}' failed: {}", input_name, e);
            }
        });
        Ok(CreatedInput { handle, metrics })
    });
}

fn register_output_type<T>(registry: &mut ModuleRegistry, type_name: &str)
where
    T: Output + Sync + 'static,
{
    registry.register_output(type_name, |name, properties, funcs| {
        let mut output = T::from_properties(name, properties)?;
        output.attach_funcs(funcs);
        let metrics = HasMetrics::metrics(&output);
        Ok(CreatedOutput {
            writer: Box::new(OutputWriterWrapper(output)),
            metrics,
        })
    });
}

struct OutputWriterWrapper<T>(T);

#[async_trait::async_trait]
impl<T: Output + Send + Sync + 'static> OutputWriter for OutputWriterWrapper<T> {
    async fn write(&self, event: &Event) -> anyhow::Result<()> {
        Output::write(&self.0, event).await
    }
}

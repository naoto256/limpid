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
pub mod process;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::dsl::ast::Property;
use crate::event::Event;
use crate::metrics::{InputMetrics, OutputMetrics};
use crate::queue::OutputWriter;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process failed: {0}")]
    Failed(String),
}

/// All modules must be constructable from DSL properties.
pub trait FromProperties: Sized {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self>;
}

/// All modules expose their own metrics.
pub trait HasMetrics {
    type Stats;
    fn metrics(&self) -> Arc<Self::Stats>;
}

#[async_trait::async_trait]
pub trait Input: FromProperties + HasMetrics<Stats = InputMetrics> + Send + 'static {
    async fn run(
        self,
        tx: mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()>;
}

#[async_trait::async_trait]
pub trait Output:
    FromProperties + HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static
{
    async fn write(&self, event: &Event) -> Result<()>;
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

type OutputFactory = Box<dyn Fn(&str, &[Property]) -> Result<CreatedOutput> + Send + Sync>;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

type ProcessFn =
    Box<dyn Fn(&[serde_json::Value], Event) -> Result<Event, ProcessError> + Send + Sync>;

pub struct ModuleRegistry {
    inputs: HashMap<String, InputFactory>,
    outputs: HashMap<String, OutputFactory>,
    processes: HashMap<String, ProcessFn>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            outputs: HashMap::new(),
            processes: HashMap::new(),
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
        F: Fn(&str, &[Property]) -> Result<CreatedOutput> + Send + Sync + 'static,
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

    pub fn register_process<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[serde_json::Value], Event) -> Result<Event, ProcessError> + Send + Sync + 'static,
    {
        self.processes.insert(name.to_string(), Box::new(f));
    }

    pub fn is_builtin_process(&self, name: &str) -> bool {
        self.processes.contains_key(name)
    }

    pub fn call_process(
        &self,
        name: &str,
        args: &[serde_json::Value],
        event: Event,
    ) -> Result<Event, ProcessError> {
        let f = self
            .processes
            .get(name)
            .ok_or_else(|| ProcessError::Failed(format!("unknown builtin process: {}", name)))?;
        f(args, event)
    }

    pub fn process_names(&self) -> Vec<&str> {
        self.processes.keys().map(|s| s.as_str()).collect()
    }

    pub fn create_output(
        &self,
        type_name: &str,
        name: &str,
        properties: &[Property],
    ) -> Result<CreatedOutput> {
        let factory = self
            .outputs
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("unknown output type: {}", type_name))?;
        factory(name, properties)
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

    // Processes
    process::register_builtins(registry);
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
    registry.register_output(type_name, |name, properties| {
        let output = T::from_properties(name, properties)?;
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

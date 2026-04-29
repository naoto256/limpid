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

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::event::{BorrowedEvent, Event};
use crate::functions::FunctionRegistry;
use crate::metrics::{InputMetrics, OutputMetrics};
use crate::queue::{OutputWriter, SinkInput};

// ---------------------------------------------------------------------------
// RenderedPayload — type-erased per-event sink payload
// ---------------------------------------------------------------------------
//
// v0.6.0 Step B: outputs participate in the pipeline's hot path by
// rendering a sink-specific payload from a `BorrowedEvent` (no
// `to_owned()` round-trip). The pipeline-internal channel between
// pipeline and sink consumer carries a `RenderedPayload` (this type)
// for memory queues and an `OwnedEvent` for disk-persisted queues.
//
// `Box<dyn Any + Send>` is the simplest dyn-safe transport for a
// heterogeneous payload — each concrete sink defines its own
// `Payload` struct (e.g. `FilePayload { egress, path }`) and downcasts
// inside `write`. Per-event cost is one heap alloc for the box plus
// whatever the payload struct contains internally; same order as the
// previous `to_owned()` workspace clone but without copying the
// workspace `HashMap` on every event.

/// Opaque payload produced by `Output::render` and consumed by
/// `Output::write`. Holds a sink-specific concrete type behind
/// `Box<dyn Any + Send>`.
pub struct RenderedPayload(Box<dyn Any + Send>);

impl RenderedPayload {
    pub fn new<T: Any + Send>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Recover the concrete payload type. Returns an error if the
    /// stored type does not match `T` — this can only happen if the
    /// pipeline misroutes a payload to the wrong sink, which would be
    /// an internal bug.
    pub fn downcast<T: Any>(self) -> Result<T> {
        self.0
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| anyhow::anyhow!("RenderedPayload downcast type mismatch"))
    }
}

impl std::fmt::Debug for RenderedPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RenderedPayload(<opaque>)")
    }
}

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

/// Output sink trait. Intentionally **not** a supertrait of `Module`
/// — `Module::from_properties` requires `Self: Sized` (factory return),
/// which would forbid `dyn Output`. Construction sites add the
/// `Module` bound where they need it (see `register_output_type`),
/// but the `dyn Output` we hand to the pipeline executor (for hot-path
/// `render`) and to the queue consumer (for `write` / `write_owned`)
/// stays object-safe.
#[async_trait::async_trait]
pub trait Output: HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static {
    /// Hot path: build a sink-specific `RenderedPayload` from a borrowed
    /// view of the event. Any DSL evaluation needed (e.g. path templates)
    /// happens against the pipeline's per-event arena, so the payload
    /// can capture `String` / `Bytes` results without paying for a
    /// `to_owned` round-trip on the event's `workspace`.
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        arena: &EventArena<'_>,
    ) -> Result<RenderedPayload>;

    /// Hot path: consume a `RenderedPayload` produced by `render` and
    /// perform the actual I/O. Each sink downcasts the payload to its
    /// own concrete type internally.
    async fn write(&self, payload: RenderedPayload) -> Result<()>;

    /// Cold path (Disk-queue replay, control-socket inject): consume an
    /// `OwnedEvent` directly. Default impl builds a transient arena,
    /// views the event into it, calls `render`, then `write`. Sinks
    /// with a faster owned-path may override.
    ///
    /// The arena/borrowed-event scope is closed before the `await` on
    /// `write` so the resulting future stays `Send` (bumpalo's `Bump`
    /// is !Sync).
    async fn write_owned(&self, event: &Event) -> Result<()> {
        let payload = {
            let bump = bumpalo::Bump::new();
            let arena = EventArena::new(&bump);
            let bevent = event.view_in(&arena);
            self.render(&bevent, &arena)?
        };
        self.write(payload).await
    }

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
///
/// `output` is the `Arc<dyn Output>` so the pipeline executor can call
/// `render` on the hot path; `writer` is a thin `OutputWriter` adapter
/// that the queue consumer uses to call `write` (or `write_owned` for
/// disk-replay paths). Both share the same underlying instance.
pub struct CreatedOutput {
    pub output: Arc<dyn Output>,
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
    register_input_type::<input::otlp::http::OtlpHttpInput>(registry, "otlp_http");
    register_input_type::<input::otlp::grpc::OtlpGrpcInput>(registry, "otlp_grpc");
    register_input_type::<input::unix_socket::UnixSocketInput>(registry, "unix_socket");
    #[cfg(feature = "journal")]
    register_input_type::<input::journal::JournalInput>(registry, "journal");

    // Outputs
    register_output_type::<output::file::FileOutput>(registry, "file");
    register_output_type::<output::unix_socket::UnixSocketOutput>(registry, "unix_socket");
    register_output_type::<output::tcp::TcpOutput>(registry, "tcp");
    register_output_type::<output::http::HttpOutput>(registry, "http");
    register_output_type::<output::otlp::OtlpOutput>(registry, "otlp");
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
    T: Module + Output + Sync + 'static,
{
    registry.register_output(type_name, |name, properties, funcs| {
        let mut output = T::from_properties(name, properties)?;
        output.attach_funcs(funcs);
        let metrics = HasMetrics::metrics(&output);
        let output_arc: Arc<dyn Output> = Arc::new(output);
        Ok(CreatedOutput {
            output: Arc::clone(&output_arc),
            writer: Box::new(OutputWriterWrapper(output_arc)),
            metrics,
        })
    });
}

struct OutputWriterWrapper(Arc<dyn Output>);

#[async_trait::async_trait]
impl OutputWriter for OutputWriterWrapper {
    async fn consume(&self, input: SinkInput) -> anyhow::Result<()> {
        match input {
            SinkInput::Rendered(payload) => self.0.write(payload).await,
            SinkInput::Owned(event) => self.0.write_owned(&event).await,
        }
    }
}

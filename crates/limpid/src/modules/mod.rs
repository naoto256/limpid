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
use crate::dsl::ast::{Expr, ExprKind, Property};
use crate::dsl::schema::{self as property_schema, PropertySpec};
use crate::dsl::span::Span;
use crate::event::{BorrowedEvent, Event};
use crate::functions::FunctionRegistry;
use crate::metrics::{InputMetrics, OutputMetrics};
use crate::queue::{OutputWriter, SinkInput};

// ---------------------------------------------------------------------------
// RenderedPayload — type-erased per-event sink payload
// ---------------------------------------------------------------------------
//
// Outputs participate in the pipeline's hot path (v0.6.0) by
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

// ---------------------------------------------------------------------------
// ModuleProperties — type-enforced property surface for `def input/output`
// ---------------------------------------------------------------------------
//
// Why a dedicated type instead of `&[Property]`:
//
// The DSL `def output foo { type syslog_tcp; peer { host "..."; ... } }` block produces a
// property list that mixes one structural key (`type`, the module selector)
// with the Module's own user properties (`address`, `bind`, `queue { ... }`).
// Every consumer downstream — analyzer schema check, runtime schema check,
// factory closure, Module impl — wants only the *user* properties; `type` is
// the registry's indirection and is not part of any module's declared schema.
//
// 0.7.2 carried this invariant only by convention. Each call site was expected
// to strip `type` before validating against the schema (the analyzer carried a
// private `strip_type_property` helper in `check::module_props` for that
// purpose; both since removed in 0.7.3). The runtime side (`create_input` /
// `create_output`) skipped the strip, and the resulting asymmetry slipped
// past CI: `--check` reported "OK", `cargo run` rejected the same config with
// "unknown property 'type'". Diagnosed and root-caused in v0.7.3.
//
// `ModuleProperties` makes that asymmetry structurally impossible. The parser
// constructs one of these for every `def input` / `def output`; `type` is
// extracted into a typed field at construction time and never re-surfaces in
// the `&[Property]` view that Module code reads. There is no way to forget
// the strip because the strip happened once, at the type boundary.

/// Property surface of a single `def input` / `def output` block.
///
/// Constructed by the parser. Carries the resolved module `type` (the
/// indirection that selects the Module), the span of the `type` value
/// expression for diagnostics, and the remaining user properties that the
/// Module impl actually consumes. There is intentionally no public accessor
/// that returns the raw property list with `type` still in it — the strip is
/// the entire reason this type exists.
#[derive(Debug, Clone)]
pub struct ModuleProperties {
    type_name: String,
    type_span: Option<Span>,
    user: Vec<Property>,
}

/// Error returned by [`ModuleProperties::parse`] when the property surface
/// does not satisfy the structural invariant. These surface at parse time —
/// before any analyzer pass or registry lookup runs — because a `def input` /
/// `def output` without a valid `type` is structurally incomplete in the
/// same sense as an unclosed brace.
#[derive(Debug, Clone)]
pub enum ModulePropertyError {
    /// No `type` key was present in the property list.
    Missing,
    /// The `type` key exists but its value is not a bare identifier (e.g.
    /// `type "syslog_tcp"` as a string literal, or `type { ... }` as a block).
    /// The grammar in principle should reject this earlier, but the
    /// property parser is permissive about value shapes, so we re-check.
    /// `span` is the value (or block-key) span; currently unused by `Display`,
    /// kept so a future `--check` integration can underline the offender.
    NonIdent {
        #[allow(dead_code)]
        span: Option<Span>,
    },
    /// More than one `type` key was supplied. Last-write-wins would mask
    /// the operator's intent, so we reject loudly. `span` points at the
    /// second occurrence — same forward-compatibility caveat as
    /// [`Self::NonIdent`].
    Duplicate {
        #[allow(dead_code)]
        span: Option<Span>,
    },
}

impl std::fmt::Display for ModulePropertyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing required property 'type'"),
            Self::NonIdent { .. } => {
                write!(
                    f,
                    "'type' must be a bare identifier (e.g. `type syslog_tcp`)"
                )
            }
            Self::Duplicate { .. } => write!(f, "'type' specified more than once"),
        }
    }
}

impl std::error::Error for ModulePropertyError {}

impl ModuleProperties {
    /// Build a `ModuleProperties` from the raw property list a parser produced
    /// for a single `def input` / `def output` block. Extracts `type` into a
    /// typed slot and returns the remaining user properties; rejects missing,
    /// duplicated, or non-ident `type`.
    pub fn parse(raw: Vec<Property>) -> Result<Self, ModulePropertyError> {
        let mut type_name: Option<(String, Option<Span>)> = None;
        let mut user = Vec::with_capacity(raw.len());
        for prop in raw {
            let key = match &prop {
                Property::KeyValue { key, .. } | Property::Block { key, .. } => key.as_str(),
            };
            if key != "type" {
                user.push(prop);
                continue;
            }
            match &prop {
                Property::KeyValue {
                    value:
                        Expr {
                            kind: ExprKind::Ident(parts),
                            ..
                        },
                    value_span,
                    ..
                } => {
                    let Some(first) = parts.first() else {
                        return Err(ModulePropertyError::NonIdent { span: *value_span });
                    };
                    if type_name.is_some() {
                        return Err(ModulePropertyError::Duplicate { span: *value_span });
                    }
                    type_name = Some((first.clone(), *value_span));
                }
                Property::KeyValue { value_span, .. } => {
                    return Err(ModulePropertyError::NonIdent { span: *value_span });
                }
                Property::Block { key_span, .. } => {
                    return Err(ModulePropertyError::NonIdent { span: *key_span });
                }
            }
        }
        let (type_name, type_span) = type_name.ok_or(ModulePropertyError::Missing)?;
        Ok(Self {
            type_name,
            type_span,
            user,
        })
    }

    /// The resolved module type identifier (e.g. `"syslog_tcp"`, `"syslog_udp"`).
    /// Always populated by construction.
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Span of the `type` value expression in the source file, when the
    /// parser supplied one. Used by `--check` to point its caret at the
    /// offending `type tcsp` ident on a typo'd type name.
    pub fn type_span(&self) -> Option<Span> {
        self.type_span
    }

    /// All properties the Module impl is allowed to see — i.e. everything
    /// except `type`. Schema validation, analyzer passes, and
    /// `from_properties` impls all consume this view.
    pub fn user_properties(&self) -> &[Property] {
        &self.user
    }

    /// Build directly from a `type` name + user properties without going
    /// through the parser path. Used by tests that hand-construct a
    /// `ModuleProperties` to drive a Module impl; production paths always
    /// go through [`Self::parse`].
    #[cfg(test)]
    pub fn from_parts(type_name: impl Into<String>, user: Vec<Property>) -> Self {
        Self {
            type_name: type_name.into(),
            type_span: None,
            user,
        }
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
/// Processes are not modules: v0.3.0 removed the native
/// process layer entirely in favour of DSL functions (`syslog.parse`
/// etc.) and user-defined `def process { ... }` blocks. Modules are
/// only inputs and outputs.
pub trait Module: Sized {
    /// Declarative schema for the module's property surface. Defaults
    /// to `None` so every existing module continues to compile while
    /// they are migrated one-by-one. Once a module declares
    /// `Some(&SCHEMA)`, the registry validates every config against it
    /// before calling `from_properties`, and the analyzer reports
    /// typos in `--check` against the same definition.
    fn property_schema() -> Option<&'static [PropertySpec]> {
        None
    }

    /// Construct the module from its declared properties. The `type`
    /// indirection has already been consumed by the registry and is
    /// not visible here — implementations only see their own user
    /// properties via [`ModuleProperties::user_properties`]. Schema
    /// validation (if any) has already run; cross-field rules ("at
    /// least one of address or host+port") still belong here — those
    /// are semantic, not shape-level.
    fn from_properties(name: &str, properties: &ModuleProperties) -> Result<Self>;

    /// Validation + construction entry. The runtime's registry path
    /// already validates the schema before invoking the factory, so
    /// `build` is the convenience for direct callers (tests, snippet
    /// libraries, anyone bypassing the registry) that want the same
    /// loud validation surface.
    #[allow(dead_code)] // used by module unit tests; production path
    // validates inside `ModuleRegistry::create_*`
    fn build(name: &str, properties: &ModuleProperties) -> Result<Self> {
        if let Some(spec) = Self::property_schema() {
            let errs = property_schema::validate(properties.user_properties(), spec);
            if !errs.is_empty() {
                anyhow::bail!(format_module_schema_errors(name, &errs));
            }
        }
        Self::from_properties(name, properties)
    }
}

/// Render a list of schema findings as a single multi-line error
/// message suitable for `anyhow::bail!`. The caller has already
/// identified which module the errors are for; we only describe the
/// findings themselves.
#[allow(dead_code)] // reachable through `Module::build` (test entry)
fn format_module_schema_errors(name: &str, errs: &[property_schema::SchemaError]) -> String {
    let mut out = format!("module '{}' has invalid configuration:", name);
    for e in errs {
        out.push_str(&format!("\n  - {}", e));
    }
    out
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
    fn render(&self, event: &BorrowedEvent<'_>, arena: &EventArena<'_>) -> Result<RenderedPayload>;

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

/// Helper for `Output::write_owned` overrides that ship a single Owned
/// event inline, bypassing any internal buffer. Renders the event in a
/// transient arena (same scope discipline as the default impl), awaits
/// the caller-supplied `ship` closure, then updates the
/// `events_written` / `events_failed` counters on the metrics handle.
///
/// The closure returns `Result<u64>` where the `u64` is the number of
/// records the server acknowledged as rejected (OTLP's
/// `partial_success.rejected_log_records`; always 0 for outputs that
/// have no equivalent concept, like `output http`). For a single Owned
/// event a non-zero rejection means the server processed the request
/// but refused our one record — transport-success, data-loss — so
/// we count it as `events_failed` even though the call returns Ok
/// (the queue layer must not retry; the server already said no).
///
/// The batched outputs (`otlp_grpc`, `otlp_http`, `output http`)
/// each need to override `write_owned` so the queue consumer's
/// per-event `Ok`/`Err` contract is honored (disk-replay events
/// would otherwise be merged into a batched flush and the verdict
/// would be lost). They differ only in the `ship` body — payload
/// downcast and the per-module `send_batch` shape — so the
/// scaffolding lives here once.
pub async fn ship_owned_inline<O, F, Fut>(
    output: &O,
    event: &Event,
    metrics: &OutputMetrics,
    ship: F,
) -> Result<()>
where
    O: Output + ?Sized,
    F: FnOnce(RenderedPayload) -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
{
    use std::sync::atomic::Ordering;

    let payload = {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let bevent = event.view_in(&arena);
        output.render(&bevent, &arena)?
    };
    match ship(payload).await {
        Ok(0) => {
            metrics.events_written.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Ok(_rejected) => {
            // Transport said OK, server rejected the single record we
            // sent. Count as failed so dashboards reflect the data
            // loss; return Ok so the queue does not retry — the
            // server already refused this exact bytes.
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(e) => {
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            Err(e)
        }
    }
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
            &ModuleProperties,
            mpsc::Sender<Event>,
            tokio::sync::watch::Receiver<bool>,
        ) -> Result<CreatedInput>
        + Send
        + Sync,
>;

type OutputFactory = Box<
    dyn Fn(&str, &ModuleProperties, Arc<FunctionRegistry>) -> Result<CreatedOutput> + Send + Sync,
>;

struct InputEntry {
    factory: InputFactory,
    schema: Option<&'static [PropertySpec]>,
}

struct OutputEntry {
    factory: OutputFactory,
    schema: Option<&'static [PropertySpec]>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct ModuleRegistry {
    inputs: HashMap<String, InputEntry>,
    outputs: HashMap<String, OutputEntry>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            outputs: HashMap::new(),
        }
    }

    /// Register an input factory along with its declared property
    /// schema. `schema = None` opts the module out of validation
    /// (used during the gradual migration; eventually every built-in
    /// will carry a schema).
    pub fn register_input<F>(
        &mut self,
        type_name: &str,
        schema: Option<&'static [PropertySpec]>,
        factory: F,
    ) where
        F: Fn(
                &str,
                &ModuleProperties,
                mpsc::Sender<Event>,
                tokio::sync::watch::Receiver<bool>,
            ) -> Result<CreatedInput>
            + Send
            + Sync
            + 'static,
    {
        self.inputs.insert(
            type_name.to_string(),
            InputEntry {
                factory: Box::new(factory),
                schema,
            },
        );
    }

    pub fn register_output<F>(
        &mut self,
        type_name: &str,
        schema: Option<&'static [PropertySpec]>,
        factory: F,
    ) where
        F: Fn(&str, &ModuleProperties, Arc<FunctionRegistry>) -> Result<CreatedOutput>
            + Send
            + Sync
            + 'static,
    {
        self.outputs.insert(
            type_name.to_string(),
            OutputEntry {
                factory: Box::new(factory),
                schema,
            },
        );
    }

    /// Schema declared by an input type, if any. Used by the analyzer
    /// to validate `def input` property surfaces during `--check`.
    pub fn input_schema(&self, type_name: &str) -> Option<&'static [PropertySpec]> {
        self.inputs.get(type_name).and_then(|e| e.schema)
    }

    /// Schema declared by an output type, if any.
    pub fn output_schema(&self, type_name: &str) -> Option<&'static [PropertySpec]> {
        self.outputs.get(type_name).and_then(|e| e.schema)
    }

    /// All registered input type names. Used by `--check` to suggest a
    /// fix for an unknown `type` ident on a `def input`.
    pub fn input_type_names(&self) -> impl Iterator<Item = &str> {
        self.inputs.keys().map(|s| s.as_str())
    }

    /// All registered output type names.
    pub fn output_type_names(&self) -> impl Iterator<Item = &str> {
        self.outputs.keys().map(|s| s.as_str())
    }

    pub fn create_input(
        &self,
        name: &str,
        properties: &ModuleProperties,
        tx: mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<CreatedInput> {
        let type_name = properties.type_name();
        let entry = self
            .inputs
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("unknown input type: {}", type_name))?;
        if let Some(spec) = entry.schema {
            let errs = property_schema::validate(properties.user_properties(), spec);
            if !errs.is_empty() {
                anyhow::bail!(format_factory_schema_errors(
                    "input", type_name, name, &errs
                ));
            }
        }
        (entry.factory)(name, properties, tx, shutdown)
    }

    pub fn create_output(
        &self,
        name: &str,
        properties: &ModuleProperties,
        funcs: Arc<FunctionRegistry>,
    ) -> Result<CreatedOutput> {
        let type_name = properties.type_name();
        let entry = self
            .outputs
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("unknown output type: {}", type_name))?;
        if let Some(spec) = entry.schema {
            let errs = property_schema::validate(properties.user_properties(), spec);
            if !errs.is_empty() {
                anyhow::bail!(format_factory_schema_errors(
                    "output", type_name, name, &errs
                ));
            }
        }
        (entry.factory)(name, properties, funcs)
    }
}

fn format_factory_schema_errors(
    surface: &str,
    type_name: &str,
    name: &str,
    errs: &[property_schema::SchemaError],
) -> String {
    let mut out = format!(
        "{} '{}' (type '{}') has invalid configuration:",
        surface, name, type_name
    );
    for e in errs {
        out.push_str(&format!("\n  - {}", e));
    }
    out
}

// ---------------------------------------------------------------------------
// Built-in module registration
// ---------------------------------------------------------------------------

pub fn register_builtins(registry: &mut ModuleRegistry) {
    // Inputs
    register_input_type::<input::syslog_udp::SyslogUdpInput>(registry, "syslog_udp");
    register_input_type::<input::syslog_tcp::SyslogTcpInput>(registry, "syslog_tcp");
    register_input_type::<input::tail::TailInput>(registry, "tail");
    register_input_type::<input::otlp::http::OtlpHttpInput>(registry, "otlp_http");
    register_input_type::<input::otlp::grpc::OtlpGrpcInput>(registry, "otlp_grpc");
    register_input_type::<input::unix_socket::UnixSocketInput>(registry, "unix_socket");
    #[cfg(feature = "journal")]
    register_input_type::<input::journal::JournalInput>(registry, "journal");

    // Outputs
    register_output_type::<output::file::FileOutput>(registry, "file");
    register_output_type::<output::unix_socket::UnixSocketOutput>(registry, "unix_socket");
    register_output_type::<output::syslog_tcp::SyslogTcpOutput>(registry, "syslog_tcp");
    register_output_type::<output::http::HttpOutput>(registry, "http");
    register_output_type::<output::otlp::http::OtlpHttpOutput>(registry, "otlp_http");
    register_output_type::<output::otlp::grpc::OtlpGrpcOutput>(registry, "otlp_grpc");
    register_output_type::<output::syslog_udp::SyslogUdpOutput>(registry, "syslog_udp");
    register_output_type::<output::stdout::StdoutOutput>(registry, "stdout");
    #[cfg(feature = "kafka")]
    register_output_type::<output::kafka::KafkaOutput>(registry, "kafka");

    // No built-in processes — v0.3.0 removed the native process
    // layer. Schema-specific parsers are DSL functions (`syslog.parse`,
    // `cef.parse`), format primitives are flat functions (`parse_json`,
    // `parse_kv`, `regex_replace`, …), and custom transforms are
    // user-defined via `def process { ... }`.
}

fn register_input_type<T>(registry: &mut ModuleRegistry, type_name: &str)
where
    T: Input + Send + 'static,
{
    registry.register_input(
        type_name,
        T::property_schema(),
        |name, properties: &ModuleProperties, tx, shutdown| {
            // The registry has already run schema validation before
            // calling this closure (when a schema is declared); here we
            // only build the concrete value, so `from_properties` is
            // the right entry point.
            let input = T::from_properties(name, properties)?;
            let metrics = HasMetrics::metrics(&input);
            let input_name = name.to_string();
            let handle = tokio::spawn(async move {
                if let Err(e) = Input::run(input, tx, shutdown).await {
                    tracing::error!("input '{}' failed: {}", input_name, e);
                }
            });
            Ok(CreatedInput { handle, metrics })
        },
    );
}

fn register_output_type<T>(registry: &mut ModuleRegistry, type_name: &str)
where
    T: Module + Output + Sync + 'static,
{
    registry.register_output(
        type_name,
        T::property_schema(),
        |name, properties: &ModuleProperties, funcs| {
            let mut output = T::from_properties(name, properties)?;
            output.attach_funcs(funcs);
            let metrics = HasMetrics::metrics(&output);
            let output_arc: Arc<dyn Output> = Arc::new(output);
            Ok(CreatedOutput {
                output: Arc::clone(&output_arc),
                writer: Box::new(OutputWriterWrapper(output_arc)),
                metrics,
            })
        },
    );
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

// ---------------------------------------------------------------------------
// ModuleProperties unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod module_properties_tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind};

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties: vec![],
        }
    }

    #[test]
    fn parse_extracts_type_and_strips_it_from_user_properties() {
        let raw = vec![
            kv("type", ExprKind::Ident(vec!["syslog_tcp".into()])),
            kv("address", ExprKind::StringLit("127.0.0.1:514".into())),
            block("queue"),
        ];
        let mp = ModuleProperties::parse(raw).expect("should parse");
        assert_eq!(mp.type_name(), "syslog_tcp");
        // type is gone from the user view; only address + queue remain
        assert_eq!(mp.user_properties().len(), 2);
        let keys: Vec<&str> = mp
            .user_properties()
            .iter()
            .map(|p| match p {
                Property::KeyValue { key, .. } | Property::Block { key, .. } => key.as_str(),
            })
            .collect();
        assert_eq!(keys, vec!["address", "queue"]);
    }

    #[test]
    fn parse_rejects_missing_type() {
        let raw = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::Missing));
        assert!(err.to_string().contains("missing required property 'type'"));
    }

    #[test]
    fn parse_rejects_non_ident_type_string_literal() {
        // `type "syslog_tcp"` — string instead of bare ident
        let raw = vec![kv("type", ExprKind::StringLit("syslog_tcp".into()))];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_non_ident_type_block() {
        // `type { ... }` — block instead of value
        let raw = vec![block("type")];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_duplicate_type() {
        let raw = vec![
            kv("type", ExprKind::Ident(vec!["syslog_tcp".into()])),
            kv("type", ExprKind::Ident(vec!["syslog_udp".into()])),
        ];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::Duplicate { .. }));
    }

    #[test]
    fn from_parts_preserves_user_properties_verbatim() {
        // Test-only helper short-circuits the parse step; verify the round-trip
        // shape matches what a parser-produced ModuleProperties would expose.
        let props = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let mp = ModuleProperties::from_parts("syslog_tcp", props.clone());
        assert_eq!(mp.type_name(), "syslog_tcp");
        assert_eq!(mp.user_properties().len(), 1);
    }
}

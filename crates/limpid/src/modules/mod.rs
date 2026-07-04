//! Module system: traits, registry, and implementations for input and
//! output modules. (Processes are DSL functions or `def process`
//! blocks evaluated by the pipeline executor — not modules — so they
//! live outside this layer.)
//!
//! `ModuleRegistry` maps type names to factory functions.
//! Runtime resolves type names from DSL config through the registry
//! instead of hardcoded match arms.

pub mod input;
pub mod output;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::dsl::module_props::ModuleProperties;
use crate::dsl::schema::{self as property_schema, PropertySpec};
use crate::event::Event;
use crate::metrics::{InputMetrics, OutputMetrics};

// ---------------------------------------------------------------------------
// BuildContext — build-time dependencies threaded to every module factory
// ---------------------------------------------------------------------------

/// Build-time dependencies provided by the runtime to every Input
/// and Output factory. Constructed once at startup, threaded into
/// every `Module::from_properties` call.
///
/// Fields are `Option<>` where the dependency is optional config
/// (e.g. `error_log` is None when the operator did not configure
/// `control { error_log "..." }`). `funcs` is always present once
/// the function registry is built (= early in runtime startup,
/// before module construction).
///
/// Forward-compatible: future transport-key registry / metrics
/// hooks / etc. land as new fields. Modules consume what they need
/// and ignore the rest.
#[derive(Clone)]
pub struct BuildContext {
    pub funcs: Arc<crate::functions::FunctionRegistry>,
    pub error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    /// Runtime-level shutdown broadcast. Unbatched sinks clone this
    /// receiver and race their retry backoff sleep against it — if
    /// shutdown fires mid-sleep, the sink breaks out of the retry
    /// loop, routes the pending event to DLQ, and returns Ok(())
    /// instead of blocking the queue consumer's select! for up to
    /// `retry.max_wait`. Without this, a steady-state `consume()`
    /// stuck in exponential backoff (1+2+4+8s = 15s under defaults)
    /// outlasts the runtime's 10s shutdown budget and the runtime
    /// task-aborts the consumer, dropping any handles in flight.
    /// Batched sinks have their own actor-local shutdown notify and
    /// do not need this receiver.
    pub shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

/// Shared retry-backoff helper for unbatched sinks: sleep `wait`, but
/// abort the sleep if the runtime shutdown signal fires. Returns
/// `true` if shutdown fired mid-sleep (caller should DLQ-route the
/// pending event and return), `false` if the sleep completed
/// normally (caller should continue the retry loop). Also treats a
/// dropped shutdown sender (RecvError from `wait_for`) as
/// "shutdown fired" — the sender is only dropped when the runtime
/// is tearing down.
pub async fn sleep_or_shutdown(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    wait: std::time::Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(wait) => false,
        _ = shutdown.wait_for(|s| *s) => true,
    }
}

/// Shared per-attempt helper for unbatched sinks: race a single
/// send-attempt future against the runtime shutdown signal. Returns
/// `Some(result)` if the attempt completed (either `Ok` or a
/// transport `Err`), `None` if shutdown fired first — the caller
/// should abandon the in-flight attempt, DLQ-route the pending
/// event, resolve the ack handle as `Recovered`, and return so the
/// queue consumer's `select!` reaches its drain arm inside the
/// runtime shutdown budget. Companion to `sleep_or_shutdown`: that
/// closes the retry backoff window; this closes the single-attempt
/// I/O window (kafka's 30 s `message.timeout.ms`, syslog_tcp's
/// per-peer 5+5+10 s per attempt, unix_socket's unbounded connect
/// and write). A dropped shutdown sender is treated as
/// "shutdown fired", same as `sleep_or_shutdown`.
///
/// Cancelling the attempt future may leave the transport in a
/// partially-sent state (e.g. a half-written TCP frame). That is an
/// intentional trade-off: the alternative is holding the queue
/// consumer past the shutdown deadline and getting task-aborted
/// mid-flight, which drops the ack handle and loses the event
/// silently. A partial-line trailer is documented as the cost of
/// prompt shutdown; downstream syslog / kafka receivers already
/// tolerate resend-on-reconnect.
pub async fn attempt_or_shutdown<F: std::future::Future>(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    attempt: F,
) -> Option<F::Output> {
    tokio::select! {
        r = attempt => Some(r),
        _ = shutdown.wait_for(|s| *s) => None,
    }
}

impl BuildContext {
    /// Test-only ctor with a no-op funcs registry, no error_log, and
    /// a shutdown receiver that never fires. Tests that need to
    /// script a mid-consume shutdown build their own
    /// `watch::channel` and populate `shutdown_signal` directly.
    #[cfg(test)]
    pub fn for_testing() -> Self {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        // Leak the sender so the receiver stays open for the whole
        // test — dropping it would flip the receiver to
        // `RecvError` on the first `wait_for`, which the sinks
        // treat as a shutdown fire (correct in production but not
        // what the ambient test setup wants).
        Box::leak(Box::new(_tx));
        Self {
            funcs: Arc::new(crate::functions::FunctionRegistry::new()),
            error_log: None,
            shutdown_signal: rx,
        }
    }
}

// ---------------------------------------------------------------------------
// RenderError — marker for render-vs-write error disambiguation
// (render errors bypass retry and route directly to recovery)
// ---------------------------------------------------------------------------
//
// `Output::consume` returns `anyhow::Result<()>` and runs render
// internally — the trait carries no separate `render` method.
// Render failures are deterministic on the event, so retrying only
// delays the DLQ landing without changing the outcome.
//
// `RenderError` is the in-band tag that lets sinks signal "render
// failed permanently, skip retries" while keeping `consume`'s return
// type a plain `Result<()>`. Sinks wrap their internal render error in
// `RenderError::new(e)` before returning, and the consumer-side path
// that drives retries / DLQ landing checks
// `anyhow::Error::downcast_ref::<RenderError>()` to bypass the retry
// budget and route straight to the error log.

/// Render-error sentinel. Wraps any underlying `anyhow::Error` raised
/// by a sink's internal render step. Detected by the consumer-side
/// retry / DLQ path via
/// `anyhow::Error::downcast_ref::<RenderError>()` so the retry budget
/// is bypassed and the payload is routed straight to `error_log`.
#[derive(Debug)]
pub struct RenderError(pub anyhow::Error);

impl RenderError {
    pub fn new(e: anyhow::Error) -> Self {
        Self(e)
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Forward to the inner error so the JSONL `reason` field stays
        // operator-friendly; the consumer-side path that catches
        // RenderError already prefixes the `render failed:` framing.
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
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
    fn from_properties(
        name: &str,
        properties: &ModuleProperties,
        ctx: &BuildContext,
    ) -> Result<Self>;

    /// Validation + construction entry. The runtime's registry path
    /// already validates the schema before invoking the factory, so
    /// `build` is the convenience for direct callers (tests, snippet
    /// libraries, anyone bypassing the registry) that want the same
    /// loud validation surface.
    #[allow(dead_code)] // used by module unit tests; production path
    // validates inside `ModuleRegistry::create_*`
    fn build(name: &str, properties: &ModuleProperties, ctx: &BuildContext) -> Result<Self> {
        if let Some(spec) = Self::property_schema() {
            let errs = property_schema::validate(properties.user_properties(), spec);
            if !errs.is_empty() {
                anyhow::bail!(format_factory_schema_errors(
                    "module",
                    properties.type_name(),
                    name,
                    &errs
                ));
            }
        }
        Self::from_properties(name, properties, ctx)
    }
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
/// but the `dyn Output` we hand to the queue consumer stays
/// object-safe.
///
/// After this change the trait is a single per-event entry point: each sink
/// decides internally whether to ship the event inline (file, stdout,
/// syslog_tcp, syslog_udp, kafka, unix_socket) or buffer for a later
/// batched flush (http, otlp_http, otlp_grpc). The earlier
/// `render → write` decomposition is now per-sink implementation
/// detail (private helpers) — the trait no longer constrains its
/// shape.
#[async_trait::async_trait]
pub trait Output: HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static {
    /// Per-event entry point. The output owns the complete delivery
    /// lifecycle: render, batch, retry, route-to-DLQ on failure, and
    /// resolve the ack handle. Until the handle resolves, the queue
    /// treats the event as in-flight and will replay it on restart
    /// (disk queue) or count it lost (memory queue on shutdown).
    ///
    /// - On successful delivery: call `ack.resolve_delivered()`.
    /// - On DLQ recovery (retry exhausted / render error / shutdown
    ///   leftover): call `ack.resolve_recovered()`.
    ///
    /// `Ok(())` does NOT mean the event was delivered — it means the
    /// output accepted ownership of the lifecycle. Actual disposition
    /// is signalled through the handle. For batched outputs, `consume`
    /// returns `Ok(())` after the event has been accepted into the
    /// buffer (with its handle held); the handle resolves on the
    /// eventual flush, not now.
    ///
    /// `Err(e)` indicates a programmer bug — the output failed to
    /// take ownership of the lifecycle. The queue consumer logs the
    /// error and the handle's `Drop` impl fires `Dropped`.
    async fn consume(&self, event: &Event, ack: crate::queue::QueueAckHandle) -> Result<()>;

    /// Drain-time variant of `consume`: called by the queue consumer
    /// for events still buffered in the receiver AFTER the shutdown
    /// signal was observed. Unlike `consume`, this MUST NOT use the
    /// steady-state retry budget — the runtime caps the entire
    /// shutdown sequence at `runtime::Daemon::SHUTDOWN_TIMEOUT` (10s),
    /// and a steady-state retry loop (e.g. `flush_events`'s
    /// exponential backoff path) that outlasts it gets killed
    /// mid-flight, dropping `QueueAckHandle`s unresolved.
    ///
    /// Contract:
    /// - Unbatched outputs: single bounded send attempt (wrap
    ///   transport in `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)`
    ///   when blocking I/O is possible), no retry, no exponential
    ///   backoff `sleep`.
    /// - Batched outputs: push the `(event, ack)` pair into the
    ///   buffer that the post-loop `shutdown()` call will drain
    ///   bounded. Do NOT trigger a steady-state flush (`flush_events`
    ///   with retry budget) from here — buffer only, defer the
    ///   bounded final flush to `shutdown()`.
    /// - On successful delivery (unbatched only): `ack.resolve_delivered()`.
    /// - On failure (transport / timeout / render): route to DLQ via
    ///   `route_event_to_dlq` and `ack.resolve_recovered()`.
    /// - `Ok(())` signals the output took lifecycle ownership; actual
    ///   disposition flows through the handle.
    ///
    /// This is a required method (no default) deliberately: a default
    /// that forwarded to `consume` would silently re-introduce the
    /// steady-state retry path the shutdown contract forbids. Forcing
    /// every output to implement it is the compile-error-driven
    /// guarantee that the drain path stays bounded.
    async fn consume_shutdown(
        &self,
        event: &Event,
        ack: crate::queue::QueueAckHandle,
    ) -> Result<()>;

    /// Called once when the daemon is shutting down, before the queue
    /// consumer exits. The output is responsible for draining any
    /// internal buffer it still holds, making a best-effort attempt to
    /// ship the contents, and resolving every ack handle it has taken
    /// ownership of via `consume`.
    ///
    /// **Contract**: every `QueueAckHandle` the output has taken
    /// ownership of MUST be resolved (to `Delivered` or `Recovered`)
    /// before this method returns. Any handle still parked in a local
    /// buffer or future will be dropped when the runtime-level shutdown
    /// timeout aborts the task, which fires `Dropped` (= silent loss
    /// attributed to a bug). Batched outputs MUST NOT reuse their
    /// steady-state retry budget here — the runtime caps the entire
    /// shutdown sequence at `runtime::Daemon::SHUTDOWN_TIMEOUT` (10s),
    /// and a retry loop that outlasts it will be killed mid-flight.
    /// Use a single bounded attempt (e.g.
    /// `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)`)
    /// and route the unsent leftovers to the DLQ via
    /// `route_shutdown_batch_to_dlq`.
    ///
    /// Per-handle disposition rules mirror the steady-state contract:
    /// - successful final delivery → `resolve_delivered()`
    /// - routed to DLQ (`error_log`) after a failed final flush or a
    ///   per-event render error → `resolve_recovered()`
    /// - no DLQ configured and the flush failed → log loudly and
    ///   `resolve_recovered()` anyway; leaking the handle would let
    ///   `Drop` fire `Dropped` (= silent loss attributed to a bug),
    ///   which is worse than an explicit, logged best-effort recovery.
    ///
    /// `Drop` cannot do this work because it is synchronous and the
    /// sink-side I/O is async. The queue consumer calls `shutdown`
    /// BEFORE waiting for in-flight handles to resolve — handles
    /// parked inside a batched output's buffer can only be resolved
    /// from inside this method, so the reverse ordering would
    /// deadlock.
    ///
    /// After `shutdown` returns the consumer expects every handle the
    /// output ever held to be resolved. Any still-unresolved handle
    /// fires `Dropped` on its way out and is counted as a bug-attributed
    /// silent loss.
    ///
    /// Errors are surfaced for logging but the consumer continues the
    /// shutdown sequence regardless — there is no further retry path
    /// available at this point.
    ///
    /// `error_log` is the operator-configured DLQ writer used by the
    /// shutdown-flush recovery path. `None` falls back to the
    /// warn-and-recover behaviour above. Implementations that hold no
    /// buffer (unbatched sinks) ignore the argument and use the
    /// default no-op impl.
    async fn shutdown(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Per-attempt deadline for the single shutdown flush a batched output
/// is allowed. Deliberately shorter than `runtime::Daemon::SHUTDOWN_TIMEOUT`
/// (10s) so the DLQ drain that follows a failed / timed-out send still
/// has headroom inside the runtime-level shutdown budget. This is a
/// daemon invariant tied to the runtime contract, not an operator knob —
/// if you raise this, raise the runtime shutdown timeout in lockstep.
pub const SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Helper for unbatched `Output::consume_shutdown` implementations: given
/// the result of a single bounded send attempt, finalize the event's
/// disposition under the shutdown contract (no retry, no backoff).
///
/// - `Ok(())` → `events_written++`, ack `Delivered`.
/// - `Err(e)` → DLQ via `route_event_to_dlq`, `events_failed++`, ack
///   `Recovered`. Never `Dropped` — the handle is always resolved.
///
/// Callers are responsible for wrapping their send in
/// `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)` when the
/// transport can block (TCP / TLS / HTTPS / Unix socket) and folding the
/// `Elapsed` error into `Err`. Sync writes (`stdout`) and best-effort
/// fire-and-forget transports may pass the raw result directly.
pub async fn finalize_shutdown_singleton_disposition(
    result: Result<()>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    metrics: &OutputMetrics,
    output_name: &str,
    event: &Event,
    ack: crate::queue::QueueAckHandle,
) {
    use std::sync::atomic::Ordering;
    match result {
        Ok(()) => {
            metrics.events_written.fetch_add(1, Ordering::Relaxed);
            ack.resolve_delivered();
        }
        Err(e) => {
            let reason = format!("shutdown send failed: {}", e);
            route_event_to_dlq(error_log, output_name, event, &reason).await;
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
    }
}

/// Shared shutdown-time disposition for a batch whose single best-effort
/// send attempt failed (transport error or deadline elapsed). Every
/// `(Event, QueueAckHandle)` entry is:
///
/// 1. Counted in `events_failed`.
/// 2. Routed to the DLQ when `error_log` is `Some` (per-record write
///    failure → warn and continue; the cursor must keep advancing).
/// 3. Resolved as `Recovered` so the queue's ack-handle invariant holds
///    and the handle's `Drop` does NOT fire `Dropped` (which would be
///    silent loss attributed to a bug).
///
/// When `error_log` is `None` we log loudly and resolve `Recovered`
/// anyway — keeping the handle parked is strictly worse than an
/// explicit, logged best-effort recovery.
pub async fn route_shutdown_batch_to_dlq(
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    metrics: &OutputMetrics,
    output_name: &str,
    events: Vec<(Event, crate::queue::QueueAckHandle)>,
    flush_err: &anyhow::Error,
) {
    use std::sync::atomic::Ordering;

    if events.is_empty() {
        return;
    }
    if let Some(writer) = error_log {
        let reason = format!("shutdown flush failed: {}", flush_err);
        for (ev, ack) in events {
            let ctx = crate::pipeline::ErroredEventContext::Output {
                timestamp: chrono::Utc::now(),
                pipeline: String::new(),
                site: format!("{} shutdown", output_name),
                reason: reason.clone(),
                output_name: output_name.to_string(),
                event: crate::pipeline::OutputEvent::from_owned(&ev),
            };
            if let Err(write_err) = writer.write(&ctx).await {
                tracing::warn!(
                    "output '{}': error_log write during shutdown failed: {} — dropping event",
                    output_name,
                    write_err
                );
            }
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
    } else {
        tracing::warn!(
            "output '{}': {} events dropped at shutdown (no error_log): {}",
            output_name,
            events.len(),
            flush_err
        );
        for (_, ack) in events {
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
    }
}

/// Per-event DLQ writer shared by every output's `consume` body. Writes
/// one `ErroredEventContext` record carrying the original event and a
/// human-readable reason; warns and continues if the writer itself
/// fails (no recursion / loops). Does NOT touch the ack handle —
/// callers resolve as `Recovered` after this returns.
pub async fn route_event_to_dlq(
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    output_name: &str,
    event: &Event,
    reason: &str,
) {
    if let Some(writer) = error_log {
        let ctx = crate::pipeline::ErroredEventContext::Output {
            timestamp: chrono::Utc::now(),
            pipeline: String::new(),
            site: output_name.to_string(),
            reason: reason.to_string(),
            output_name: output_name.to_string(),
            event: crate::pipeline::OutputEvent::from_owned(event),
        };
        if let Err(write_err) = writer.write(&ctx).await {
            tracing::warn!(
                "output '{}': error_log write failed: {} — dropping event",
                output_name,
                write_err
            );
        }
    } else {
        tracing::error!(
            "output '{}': dropping event (no error_log): {}",
            output_name,
            reason
        );
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

/// Returned by output factory: the constructed sink + metrics handle.
///
/// `output` is the `Arc<dyn Output>` handed to the queue consumer,
/// which calls `Output::consume` directly — there is no intermediate
/// adapter trait. Batched outputs that need the operator-configured
/// `error_log` receive it as a constructor argument via the factory;
/// no post-construction setter remains on the trait.
pub struct CreatedOutput {
    pub output: Arc<dyn Output>,
    pub metrics: Arc<OutputMetrics>,
}

// ---------------------------------------------------------------------------
// Factory function types
// ---------------------------------------------------------------------------

type InputFactory = Box<
    dyn Fn(
            &str,
            &ModuleProperties,
            &BuildContext,
            mpsc::Sender<Event>,
            tokio::sync::watch::Receiver<bool>,
        ) -> Result<CreatedInput>
        + Send
        + Sync,
>;

type OutputFactory =
    Box<dyn Fn(&str, &ModuleProperties, &BuildContext) -> Result<CreatedOutput> + Send + Sync>;

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
                &BuildContext,
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
        F: Fn(&str, &ModuleProperties, &BuildContext) -> Result<CreatedOutput>
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
        ctx: &BuildContext,
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
        (entry.factory)(name, properties, ctx, tx, shutdown)
    }

    pub fn create_output(
        &self,
        name: &str,
        properties: &ModuleProperties,
        ctx: &BuildContext,
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
        (entry.factory)(name, properties, ctx)
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

    // Outputs — every output owns its own retry + DLQ routing.
    // Build-time dependencies (`error_log`, `funcs`) arrive via
    // `BuildContext` in `from_properties`.
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
        |name, properties: &ModuleProperties, ctx, tx, shutdown| {
            // The registry has already run schema validation before
            // calling this closure (when a schema is declared); here we
            // only build the concrete value, so `from_properties` is
            // the right entry point.
            let input = T::from_properties(name, properties, ctx)?;
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
        |name, properties: &ModuleProperties, ctx| {
            let output = T::from_properties(name, properties, ctx)?;
            let metrics = HasMetrics::metrics(&output);
            let output_arc: Arc<dyn Output> = Arc::new(output);
            Ok(CreatedOutput {
                output: output_arc,
                metrics,
            })
        },
    );
}

#[cfg(test)]
mod attempt_or_shutdown_tests {
    use super::attempt_or_shutdown;
    use std::time::Duration;

    /// Happy path: the attempt future completes first, the shutdown
    /// receiver is untouched, and the outcome is returned intact.
    #[tokio::test]
    async fn attempt_completes_before_shutdown_returns_the_outcome() {
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let outcome = attempt_or_shutdown(&mut rx, async { 42u32 }).await;
        assert_eq!(outcome, Some(42));
    }

    /// Shutdown fires while the attempt is still pending: the helper
    /// returns `None` and the attempt future is dropped. Pin this on
    /// wall time so a regression that dropped the shutdown arm would
    /// hang and time out here — the assert bound (200 ms against a
    /// 5 s sleep) is far under any plausible scheduler jitter.
    #[tokio::test]
    async fn shutdown_before_attempt_completes_returns_none() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let helper = async move {
            attempt_or_shutdown(&mut rx, tokio::time::sleep(Duration::from_secs(5)))
                .await
                .map(|_| ())
        };
        let handle = tokio::spawn(helper);
        // Give the helper a chance to enter the select.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(true).unwrap();
        let started = std::time::Instant::now();
        let outcome = handle.await.unwrap();
        let elapsed = started.elapsed();
        assert!(outcome.is_none(), "shutdown must produce None");
        assert!(
            elapsed < Duration::from_millis(200),
            "shutdown must preempt the sleep — took {elapsed:?}"
        );
    }

    /// A dropped shutdown sender flips `wait_for` into a `RecvError`;
    /// the helper treats that as "shutdown fired" (the sender is
    /// only dropped when the runtime is tearing down). Pin the
    /// contract so a future refactor that changed the RecvError
    /// mapping would trip the test.
    #[tokio::test]
    async fn dropped_shutdown_sender_is_treated_as_fired() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        drop(tx);
        let outcome =
            attempt_or_shutdown(&mut rx, tokio::time::sleep(Duration::from_secs(5))).await;
        assert!(outcome.is_none());
    }
}

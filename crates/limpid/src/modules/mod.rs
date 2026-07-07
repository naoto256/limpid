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
    /// Operator-selected confidentiality policy for tracing-side DLQ
    /// fallback emission (unset `error_log`, or `error_log` write
    /// failure). See [`crate::error_log::ErrorLogFallback`]. Default
    /// `Off` — the tracing line is a one-line failure summary and
    /// no event payload leaves the daemon through log aggregation
    /// unless the operator opts in.
    pub error_log_fallback: crate::error_log::ErrorLogFallback,
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

/// Race a **pre-send** operation against the runtime shutdown
/// signal. Returns `Some(result)` if the operation completed
/// (either `Ok` or an `Err` from the operation itself), `None` if
/// shutdown fired first — in the `None` case the caller has done
/// nothing observable and can safely DLQ-route the event as
/// `Recovered`.
///
/// # Contract
///
/// This helper is **only** safe around code that has no wire-level
/// side effect: connect, TLS handshake, DNS lookup, credential
/// refresh, peer rotation, cool-down waits. It is **not** safe
/// around a `write(2)` / `write_all` / `send_to` / `producer.send`
/// call: cancelling those mid-flight leaves the transport in a
/// partially-sent or ambiguous-delivery state, and the DLQ
/// `Recovered` disposition would be a lie (the receiver may have
/// observed part or all of the payload).
///
/// The correct shape for unbatched sinks is:
///
/// ```text
/// loop attempt:
///     match pre_send_or_shutdown(&mut shutdown, connect_and_prepare()).await {
///         Some(Ok(prepared)) => {
///             // Send phase runs to completion — no shutdown
///             // wrapper here. Existing I/O timeouts (per-peer
///             // write timeout, kafka `message.timeout.ms`)
///             // still bound the wait.
///             match send_phase(prepared).await { ... }
///         }
///         Some(Err(e)) => { /* connect failed; retry */ }
///         None => { /* shutdown pre-send; DLQ Recovered — honest */ }
///     }
/// ```
///
/// Shutdown does **not** add a new cancellation point beyond the
/// pre-send phase. The send phase remains cancellable only by its
/// existing timeout sources (e.g. `PEER_WRITE_TIMEOUT`); the
/// resulting partial-write ambiguity is the existing at-least-once
/// contract (retry may duplicate). If the send phase runs past the
/// runtime shutdown budget the task is aborted and the ack handle
/// drops as `Dropped`; on a disk queue that Dropped position holds
/// the cursor and the event replays on next start (see
/// `crates/limpid/src/queue/mod.rs` for the queue-side wedge
/// contract).
///
/// A dropped shutdown sender is treated as "shutdown fired",
/// matching `sleep_or_shutdown`.
pub async fn pre_send_or_shutdown<F: std::future::Future>(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    pre_send: F,
) -> Option<F::Output> {
    tokio::select! {
        r = pre_send => Some(r),
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
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
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
    /// - On failure (retry exhausted / render error / DLQ-adjacent
    ///   path): route through `route_event_to_dlq` and dispatch the
    ///   returned `DlqRouteOutcome` through
    ///   `resolve_ack_from_dlq_outcome`. The dispatcher owns the
    ///   backend-aware terminal disposition: `Recovered` on any queue
    ///   when the DLQ record was durably written, or when `error_log`
    ///   is unset — the operator has declared no durable recovery
    ///   is required and the tracing fallback runs per the
    ///   `error_log_fallback` ladder (payload-free summary by
    ///   default, `Meta` / `Full` only on explicit opt-in) as a
    ///   best-effort operator signal, not a load-bearing recovery
    ///   target. `Dropped` on a disk queue when the configured DLQ
    ///   file write itself failed (which triggers the fail-stop
    ///   wedge so the disk cursor holds for next-start replay), and
    ///   memory queues fold `Dropped` back to `Recovered` internally
    ///   because they have no replay path. Sinks that resolve the
    ///   handle themselves must call one of `resolve_delivered` /
    ///   `resolve_recovered` / `resolve_dropped` directly, but the
    ///   dispatcher is the standard shape — it keeps
    ///   `events_failed` bumped exactly once per failure and picks
    ///   the correct disposition per backend without per-sink
    ///   duplication.
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
    /// - On successful delivery (unbatched only): `ack.resolve_delivered()`,
    ///   or delegate the whole disposition to
    ///   `finalize_shutdown_singleton_disposition`, which owns the
    ///   success bump and the DLQ route for a single bounded attempt.
    /// - On failure (transport / timeout / render): route to DLQ via
    ///   `route_event_to_dlq` and dispatch the returned
    ///   `DlqRouteOutcome` through `resolve_ack_from_dlq_outcome`, so
    ///   disk queues wedge on a configured-DLQ-write failure and
    ///   memory queues fall back to `Recovered`. The tracing-side
    ///   fallback line is shaped by the operator's `error_log_fallback`
    ///   ladder (`Off` = 1-line summary, `Meta` = structured
    ///   metadata, `Full` = full JSONL via `event_record`) and is
    ///   not treated as durable recovery material — the DLQ file
    ///   is the load-bearing recovery target. The helper
    ///   `finalize_shutdown_singleton_disposition` bundles the
    ///   Ok/Err arms into a single call for sinks that do not need
    ///   custom branching.
    /// - Stream / byte-oriented transports whose `Err` cannot be
    ///   proved to have fired *before* the wire byte-boundary
    ///   (e.g. TCP with or without TLS, unix stream sockets — any
    ///   transport where the outer `tokio::time::timeout` or a
    ///   mid-write disconnect can leave a partial frame at the
    ///   peer) MUST route through
    ///   `finalize_shutdown_singleton_disposition_ambiguous`
    ///   instead of the plain variant. The `_ambiguous` variant
    ///   force-Dropped-so-wedges the disk queue for next-start
    ///   replay, avoiding the double-state where the payload
    ///   partially reached the downstream and the DLQ record is
    ///   also replayable; memory queues still fall back to
    ///   `Recovered`.
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
    /// Per-handle disposition rules mirror the steady-state contract
    /// and are dispatched through `resolve_ack_from_dlq_outcome` so
    /// disk and memory queues get the correct terminal disposition:
    /// - successful final delivery → `resolve_delivered()`.
    /// - routed to DLQ via `route_event_to_dlq` (or the batched
    ///   variant `route_shutdown_batch_to_dlq`); the returned
    ///   `DlqRouteOutcome` is handed to `resolve_ack_from_dlq_outcome`,
    ///   which resolves as `Recovered` on any queue when the DLQ
    ///   record was durably written or when `error_log` was unset
    ///   (the operator has declared no durable recovery is required
    ///   and the tracing fallback runs per the `error_log_fallback`
    ///   ladder — payload-free by default, `Meta` / `Full` only on
    ///   explicit opt-in), and as `Dropped` on a disk queue when
    ///   the configured DLQ file write itself failed (disk-queue
    ///   fail-stop wedge holds the cursor for replay on next
    ///   start). Memory queues cannot replay across restarts, so
    ///   the same DLQ-write-failure shape resolves as `Recovered`
    ///   there and the operator relies on `events_errored_unwritable`
    ///   as the alarm signal (the tracing fallback line, whatever
    ///   its ladder-selected shape, is best-effort, not load-bearing).
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

    /// Wedge-exit lifecycle hook: resolve internally parked ack
    /// handles WITHOUT any new delivery attempt.
    ///
    /// Called by the queue consumer instead of [`Output::shutdown`]
    /// when the output is exiting through the fail-stop wedge path —
    /// i.e. a `Dropped-on-disk` disposition has been observed, meaning
    /// the sink hit a bug boundary (typically DLQ write itself failed
    /// on a disk-backed queue) and the operator contract is that no
    /// further work goes through this codepath in this daemon
    /// generation. The wedge is a hard trust boundary; replaying
    /// buffered handles through a still-buggy sink would risk the
    /// same Dropped outcome and prolong the wedge.
    ///
    /// Contract:
    ///
    /// - **No new delivery attempt.** Implementations MUST NOT enter
    ///   `policy.send` / `flush_events` / any transport call. This is
    ///   the lifecycle distinction from [`Output::shutdown`], whose
    ///   contract explicitly allows one bounded flush.
    /// - **Resolve internally parked handles only.** Any
    ///   `QueueAckHandle` still held inside the sink's own buffer
    ///   must be resolved before this method returns, otherwise the
    ///   consumer's ack drain hangs on messages that will never
    ///   arrive and the runtime falls back to its 10s wall-clock
    ///   shutdown timeout.
    /// - **Ambiguous outcome.** Buffered events go through
    ///   [`route_shutdown_batch_ambiguous_to_dlq`] — the wedge is
    ///   itself a failure boundary and the wire state is undefined,
    ///   so per-batch disposition forces `Dropped` regardless of
    ///   the DLQ write result. On a disk queue that keeps the
    ///   cursor at the wedged batch's position for replay; on a
    ///   memory queue the fold to `Recovered` inside
    ///   `resolve_ack_from_dlq_outcome` prevents the ack drain
    ///   deadlock at the cost of losing the buffered events (no
    ///   replay path exists across restarts anyway).
    ///
    /// Unbatched sinks hold no buffer, so the default no-op impl is
    /// correct for them — every handle they took has already been
    /// resolved on the steady-state path by the time the wedge
    /// signal reaches the consumer.
    ///
    /// `error_log` is the operator-configured DLQ writer used by the
    /// wedge-exit DLQ route. `None` falls back to the ambiguous
    /// helper's tracing branch (see [`route_shutdown_batch_ambiguous_to_dlq`]).
    async fn shutdown_wedged(
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
/// - `Err(e)` → DLQ via `route_event_to_dlq`, `events_failed++`, and
///   dispatch the ack through `resolve_ack_from_dlq_outcome`: `Recovered`
///   when the DLQ write succeeded, or when no `error_log` is configured
///   and the operator has declared no durable recovery is required —
///   the tracing fallback runs per the `error_log_fallback` ladder
///   (`Off` = payload-free summary by default, `Meta` / `Full` on
///   explicit opt-in), not as a guaranteed recovery path. `Dropped`
///   fires when a configured DLQ file was present but the write to
///   it failed. The `Dropped` arm on a disk queue triggers the
///   disk-queue fail-stop wedge so the cursor holds for a replay on
///   next start rather than silently advancing past an event whose
///   DLQ record was never durably written.
///
/// Callers are responsible for wrapping their send in
/// `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)` when the
/// transport can block (TCP / TLS / HTTPS / Unix socket) and folding the
/// `Elapsed` error into `Err`. Sync writes (`stdout`) and best-effort
/// fire-and-forget transports may pass the raw result directly.
pub async fn finalize_shutdown_singleton_disposition(
    result: Result<()>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
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
            let position = ack.position();
            let outcome = route_event_to_dlq(
                error_log,
                error_log_fallback,
                metrics,
                output_name,
                event,
                position,
                &reason,
            )
            .await;
            resolve_ack_from_dlq_outcome(ack, outcome, metrics);
        }
    }
}

/// Same shape as [`finalize_shutdown_singleton_disposition`] but for
/// transports whose `Err` is ambiguous with respect to the
/// side-effect boundary: the payload may have already been observed
/// downstream by the time the failure fired (partial wire state,
/// connection reset mid-frame, `tokio::time::timeout` firing after
/// the first byte was already on the wire — the reason
/// `write_frame` in `persistent_conn` explicitly documents that
/// mid-write cancellation can leave partial wire state).
///
/// The steady-state `Recovered` disposition would fabricate an
/// at-least-once guarantee the transport does not support: the
/// payload might have reached the downstream AND land in the DLQ
/// for replay, producing a double delivery. This variant forces
/// the failure disposition to `Dropped` regardless of the DLQ-write
/// outcome:
///
/// - Disk queue: `Dropped` triggers the fail-stop wedge; the cursor
///   holds and the event replays on next start after operator
///   intervention. That is safe: a genuinely-delivered event will
///   be a duplicate on replay, which the downstream can dedupe or
///   an operator can reconcile with the DLQ record.
/// - Memory queue: `Dropped` folds to `Recovered` inside
///   `resolve_ack_from_dlq_outcome` (memory has no replay path);
///   the loss is unavoidable but the DLQ record still captured
///   what would have shipped.
///
/// `Ok(())` still counts as `Delivered` with the usual metric bump.
///
/// The DLQ record is written for operator visibility (even though
/// the outcome is forced to `Dropped`), so reconciliation between
/// the wedged position and the downstream is possible from the
/// DLQ file when configured. The tracing-side fallback line is
/// shaped by the operator's `error_log_fallback` ladder — a
/// `journalctl | jq` reconciliation path only exists on the
/// explicit `Full` opt-in; `Off` (default) and `Meta` do not
/// carry the payload bytes reconciliation would need. The DLQ
/// file remains the load-bearing reconciliation source.
pub async fn finalize_shutdown_singleton_disposition_ambiguous(
    result: Result<()>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
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
            let reason = format!(
                "shutdown send failed after side-effect boundary (ambiguous wire state): {}",
                e
            );
            // Write the DLQ record for the operator's audit trail —
            // even though we're forcing `Dropped`, the record makes
            // reconciliation possible after next-start replay.
            let position = ack.position();
            let _ = route_event_to_dlq(
                error_log,
                error_log_fallback,
                metrics,
                output_name,
                event,
                position,
                &reason,
            )
            .await;
            resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Dropped, metrics);
        }
    }
}

/// Shared shutdown-time disposition for a batch whose single best-effort
/// send attempt failed (transport error or deadline elapsed). Every
/// `(Event, QueueAckHandle)` entry is:
///
/// 1. Routed to the DLQ when `error_log` is `Some`. A per-record DLQ
///    write success resolves the ack as `Recovered`; a per-record DLQ
///    write failure bumps `events_errored_unwritable` and hands the
///    ack through `resolve_ack_from_dlq_outcome`, which on a disk
///    queue resolves as `Dropped` (the disk-queue fail-stop wedge
///    holds the cursor for replay on next start) and on a memory
///    queue resolves as `Recovered` (memory queues cannot replay).
/// 2. Counted in `events_failed` — the bump is owned by
///    `resolve_ack_from_dlq_outcome` so the count is authoritative and
///    matches the disposition it just committed.
///
/// When `error_log` is `None` the operator has declared no durable
/// recovery is required. The emission goes through
/// `emit_dlq_tracing_fallback` which enforces the operator's
/// `error_log_fallback` ladder — payload-free summary by default,
/// structured metadata on `Meta`, or full JSONL via `event_record`
/// on the explicit `Full` opt-in. Each ack still resolves through
/// the same helper with an explicit `Recovered` outcome so the
/// failure count and the resolve go through the same choke point
/// as the DLQ path; the tracing line is a best-effort operator
/// signal, not a load-bearing recovery target.
pub async fn route_shutdown_batch_to_dlq(
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: &OutputMetrics,
    output_name: &str,
    events: Vec<(Event, crate::queue::QueueAckHandle)>,
    flush_err: &anyhow::Error,
) {
    use std::sync::atomic::Ordering;

    if events.is_empty() {
        return;
    }
    let reason = format!("shutdown flush failed: {}", flush_err);
    if let Some(writer) = error_log {
        for (ev, ack) in events {
            let position = ack.position();
            let ctx = crate::pipeline::ErroredEventContext::Output {
                timestamp: chrono::Utc::now(),
                pipeline: String::new(),
                site: format!("{} shutdown", output_name),
                reason: reason.clone(),
                output_name: output_name.to_string(),
                event: crate::pipeline::OutputEvent::from_owned(&ev),
            };
            let outcome = match writer.write(&ctx).await {
                Ok(()) => DlqRouteOutcome::Recovered,
                Err(write_err) => {
                    emit_dlq_tracing_fallback(
                        /* error_log_configured */ true,
                        error_log_fallback,
                        &ctx,
                        Some(position),
                        Some(&write_err),
                    );
                    metrics
                        .events_errored_unwritable
                        .fetch_add(1, Ordering::Relaxed);
                    DlqRouteOutcome::Dropped
                }
            };
            resolve_ack_from_dlq_outcome(ack, outcome, metrics);
        }
    } else {
        // No DLQ configured — payload-free tracing per ladder
        // row-A. Disposition still folds to `Recovered` so the
        // ack drain progresses; the operator has no recovery
        // trail beyond the one-line summary, which is exactly
        // what the unset config declares.
        for (ev, ack) in events {
            let position = ack.position();
            let ctx = crate::pipeline::ErroredEventContext::Output {
                timestamp: chrono::Utc::now(),
                pipeline: String::new(),
                site: format!("{} shutdown", output_name),
                reason: reason.clone(),
                output_name: output_name.to_string(),
                event: crate::pipeline::OutputEvent::from_owned(&ev),
            };
            emit_dlq_tracing_fallback(
                /* error_log_configured */ false,
                error_log_fallback,
                &ctx,
                Some(position),
                None,
            );
            resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Recovered, metrics);
        }
    }
}

/// Same shape as [`route_shutdown_batch_to_dlq`] but for batched-sink
/// failures whose wire state is ambiguous: `policy.send(...)` may have
/// committed part or all of the batch to the peer before the
/// cancellation / timeout fired. Two call sites for this today:
///
/// - `BatchedSink::flush_events` steady-state retry loop, `None` arm:
///   `wait_until_shutdown` wins over an in-flight `policy.send(...)`,
///   so the send future is dropped mid-flight. HTTP / OTLP transports
///   have already flushed request bytes into the kernel by the time
///   the future returns to the runtime.
/// - `BatchedSink::flush_events_at_shutdown` `Elapsed` arm:
///   `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)` fires
///   after the send call has been running for 3 s — the same
///   partial-wire risk.
///
/// The plain [`route_shutdown_batch_to_dlq`] resolves the failure as
/// `Recovered` on a successful DLQ write, which advances the disk
/// queue's cursor past the batch. Combined with a partial-wire
/// success this can double-deliver: the downstream receives some
/// records, the DLQ record replays via `limpidctl inject output`, and
/// the operator has no signal that the two paths overlap.
///
/// This variant writes the DLQ record for the operator's audit trail
/// (reconciliation between wedged position and downstream is only
/// possible when the payload survives durably) but forces the
/// failure disposition to `Dropped` regardless of the DLQ-write
/// outcome:
///
/// - Disk queue: the fail-stop wedge holds the cursor at the batch's
///   position on next start, giving the operator a chance to
///   reconcile the DLQ record against the downstream before the
///   same batch is retried.
/// - Memory queue: `Dropped` folds to `Recovered` inside
///   `resolve_ack_from_dlq_outcome` (no replay path exists so the
///   loss is unavoidable, but the DLQ record still captures what
///   would have shipped).
///
/// The `no error_log` branch mirrors the plain helper: dispatch
/// through `emit_dlq_tracing_fallback` (payload-free summary by
/// default under the operator's declared no-recovery contract;
/// `Meta` / `Full` shape only on explicit opt-in), then resolve
/// the ack. Even without a file DLQ the disposition is forced to
/// `Dropped` so operators do not silently lose the ambiguity
/// signal on a disk queue — the tracing line is a best-effort
/// operator signal, not a load-bearing recovery target.
pub async fn route_shutdown_batch_ambiguous_to_dlq(
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: &OutputMetrics,
    output_name: &str,
    events: Vec<(Event, crate::queue::QueueAckHandle)>,
    flush_err: &anyhow::Error,
) {
    use std::sync::atomic::Ordering;

    if events.is_empty() {
        return;
    }
    let reason = format!(
        "shutdown flush failed after send-boundary (ambiguous wire state): {}",
        flush_err
    );
    if let Some(writer) = error_log {
        for (ev, ack) in events {
            let position = ack.position();
            let ctx = crate::pipeline::ErroredEventContext::Output {
                timestamp: chrono::Utc::now(),
                pipeline: String::new(),
                site: format!("{} shutdown", output_name),
                reason: reason.clone(),
                output_name: output_name.to_string(),
                event: crate::pipeline::OutputEvent::from_owned(&ev),
            };
            if let Err(write_err) = writer.write(&ctx).await {
                emit_dlq_tracing_fallback(
                    /* error_log_configured */ true,
                    error_log_fallback,
                    &ctx,
                    Some(position),
                    Some(&write_err),
                );
                metrics
                    .events_errored_unwritable
                    .fetch_add(1, Ordering::Relaxed);
            }
            // The DLQ record has been written (or the tracing
            // fallback fired per ladder) for operator visibility,
            // but the disposition is forced to `Dropped` regardless:
            // the wire state cannot be proved pre-boundary, so
            // `Recovered` would fabricate an at-least-once
            // guarantee the transport does not support.
            resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Dropped, metrics);
        }
    } else {
        for (ev, ack) in events {
            let position = ack.position();
            let ctx = crate::pipeline::ErroredEventContext::Output {
                timestamp: chrono::Utc::now(),
                pipeline: String::new(),
                site: format!("{} shutdown", output_name),
                reason: reason.clone(),
                output_name: output_name.to_string(),
                event: crate::pipeline::OutputEvent::from_owned(&ev),
            };
            emit_dlq_tracing_fallback(
                /* error_log_configured */ false,
                error_log_fallback,
                &ctx,
                Some(position),
                None,
            );
            resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Dropped, metrics);
        }
    }
}

/// Emit the DLQ tracing fallback line per operator ladder.
///
/// Central emission for all four DLQ paths (steady-state per-event,
/// batched shutdown-drain, ambiguous shutdown-drain, and the
/// pipeline-side runtime error path in `runtime.rs`). Only the
/// tracing line is written here — every caller keeps ownership of
/// its own ack disposition so this helper can never accidentally
/// change queue cursor semantics.
///
/// # Ladder
///
/// | State                                    | Line body                                                                 |
/// |------------------------------------------|---------------------------------------------------------------------------|
/// | `error_log` unset                        | payload-free summary; `fallback` value ignored                            |
/// | `error_log` set, fallback `Off`          | payload-free summary; write-fail context noted                            |
/// | `error_log` set, fallback `Meta`         | structured metadata (`kind`, `fallback`, `reason`, `timestamp`, `size`, `position`); no payload bytes |
/// | `error_log` set, fallback `Full`         | `event_record = <full JSONL>`; opt-in to payload exposure                 |
///
/// # Row-A ordering guard
///
/// The unset check is *before* the fallback match by design: an
/// operator who omits `error_log` has already declared "no durable
/// recovery needed", and honouring a stray `error_log_fallback
/// "full"` on that operator's config would contradict the declaration
/// (a `--check` warning surfaces the inert combination separately).
///
/// # Structured fields
///
/// - `kind`: `"output"` or `"process"` — matches the DLQ record shape.
/// - `name`: the output name (Output flavor) or pipeline name (Process flavor).
/// - `site`: failure site (`<name>`, `<name> shutdown`, `(pipeline body)`, …).
/// - `reason`: the failure reason captured on `ctx`.
/// - `fallback`: the resolved ladder state, present only when
///   `error_log` was configured (row-A skips the fallback fields
///   because the value is being ignored anyway).
/// - `timestamp`: RFC3339 wall-clock from `ctx` (Meta only).
/// - `size`: bytes of the recoverable payload — egress for Output,
///   ingress for Process (Meta only).
/// - `position`: `AckPosition` debug form; queue kind + numeric
///   offset/seq only, no filesystem path (Meta only). Always
///   present on sink-side callers — `route_event_to_dlq` /
///   `finalize_shutdown_singleton_disposition{,_ambiguous}` /
///   `route_shutdown_batch_to_dlq` / `route_shutdown_batch_ambiguous_to_dlq`
///   all require a live `AckPosition` at the type level and pass
///   `Some(position)` here. `(none)` only appears on the pipeline
///   side (`runtime::write_errored_to_dlq`) where the event
///   never entered an output queue and no `AckPosition` exists.
///
/// # Excluded fields
///
/// The `Meta` shape deliberately never carries: `event_record`,
/// rendered body / egress bytes, ingress bytes, HTTP headers, or
/// any operator/customer-populated labels. If a future extension
/// wants any of those, gate them behind a `Full` opt-in — this
/// keeps the confidentiality boundary that separates `Meta` from
/// `Full` bright-line.
pub(crate) fn emit_dlq_tracing_fallback(
    error_log_configured: bool,
    fallback: crate::error_log::ErrorLogFallback,
    ctx: &crate::pipeline::ErroredEventContext,
    position: Option<crate::queue::AckPosition>,
    write_err: Option<&anyhow::Error>,
) {
    use crate::error_log::ErrorLogFallback;

    let (kind, name) = match ctx {
        crate::pipeline::ErroredEventContext::Output { output_name, .. } => {
            ("output", output_name.as_str())
        }
        crate::pipeline::ErroredEventContext::Process { pipeline, .. } => {
            ("process", pipeline.as_str())
        }
    };
    let site = ctx.site();
    let reason = ctx.reason();

    // Row-A guard: unset error_log always emits payload-free line
    // regardless of the fallback value.
    if !error_log_configured {
        tracing::error!(
            kind = kind,
            name = name,
            site = site,
            reason = reason,
            "{} '{}' (site '{}'): DLQ record not written (no error_log configured); \
             payload omitted from tracing fallback to preserve confidentiality. \
             Configure `control {{ error_log \"...\" }}` and set \
             `control {{ error_log_fallback \"meta\" | \"full\" }}` to opt into a \
             tracing-side recovery trail.",
            kind,
            name,
            site,
        );
        return;
    }

    let write_err_str = write_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());

    match fallback {
        ErrorLogFallback::Off => {
            tracing::error!(
                kind = kind,
                name = name,
                site = site,
                fallback = "off",
                reason = reason,
                "{} '{}' (site '{}'): error_log write failed: {} — payload omitted \
                 from tracing fallback (error_log_fallback = off; set \"meta\" or \
                 \"full\" to expose)",
                kind,
                name,
                site,
                write_err_str,
            );
        }
        ErrorLogFallback::Meta => {
            let position_str = position
                .map(|p| format!("{:?}", p))
                .unwrap_or_else(|| "(none)".to_string());
            tracing::error!(
                kind = kind,
                name = name,
                site = site,
                fallback = "meta",
                reason = reason,
                timestamp = %ctx.timestamp().to_rfc3339(),
                size = ctx.payload_size_hint(),
                position = position_str,
                "{} '{}' (site '{}'): error_log write failed: {} — metadata emitted, \
                 payload bytes omitted (error_log_fallback = meta)",
                kind,
                name,
                site,
                write_err_str,
            );
        }
        ErrorLogFallback::Full => {
            tracing::error!(
                kind = kind,
                name = name,
                site = site,
                fallback = "full",
                event_record = %ctx.to_jsonl(),
                reason = reason,
                "{} '{}' (site '{}'): error_log write failed: {} — routing as Dropped; \
                 the final disposition (queue-cursor hold for replay vs. terminal \
                 loss) depends on the queue backend and is applied on the ack side. \
                 event_record below carries the full JSONL (error_log_fallback = \
                 full — payload may reach journald / log aggregation)",
                kind,
                name,
                site,
                write_err_str,
            );
        }
    }
}

/// Resolve `ack` per the queue backend and the DLQ route outcome,
/// and count the terminal failure in `events_failed`. This helper
/// owns the failure count for every DLQ-adjacent path so the metric
/// is bumped exactly once per event, regardless of which queue backend
/// resolved it. Callers must NOT bump `events_failed` themselves next
/// to this call.
///
/// - `Recovered` (any queue): bump `events_failed`, `resolve_recovered`.
/// - `Dropped` + memory queue: bump `events_failed`, `resolve_recovered`
///   — memory queues cannot replay on restart, so `resolve_dropped`
///   would only wedge the pipeline without a recovery path; the
///   event is actually lost. `events_errored_unwritable` (bumped
///   inside `route_event_to_dlq` before this call) is the
///   operator alarm signal for that loss, not a durable trace of
///   the event itself.
/// - `Dropped` + disk queue: `resolve_dropped` — the disk cursor holds
///   and the event replays on next start (the disk-queue fail-stop
///   wedge kicks in on the consumer side). `events_failed` is bumped
///   on the ack side by `handle_ack_disposition(Dropped)` when the
///   consumer receives the disposition, so this helper deliberately
///   does NOT bump here. Bumping here as well would double-count
///   every disk-backed DLQ-write failure.
pub fn resolve_ack_from_dlq_outcome(
    ack: crate::queue::QueueAckHandle,
    outcome: DlqRouteOutcome,
    metrics: &OutputMetrics,
) {
    use crate::queue::AckPosition;
    use std::sync::atomic::Ordering;
    match (outcome, ack.position()) {
        (DlqRouteOutcome::Recovered, _) | (DlqRouteOutcome::Dropped, AckPosition::Memory) => {
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
            ack.resolve_recovered();
        }
        (DlqRouteOutcome::Dropped, AckPosition::Disk { .. }) => {
            ack.resolve_dropped();
        }
    }
}

/// Outcome of a single per-event DLQ route.
///
/// - `Recovered` — either the record was written to the operator-
///   configured DLQ file, or `error_log` was unset and the operator
///   has declared no durable recovery is required. In the first
///   shape the DLQ file is a load-bearing recovery target; in the
///   second the `tracing::error!` line runs per the operator's
///   `error_log_fallback` ladder (payload-free by default, `Meta`
///   / `Full` on explicit opt-in) as a best-effort operator signal.
///   `resolve_recovered()` is honest in both shapes because the
///   operator either has a written DLQ record or has explicitly
///   opted out of one.
/// - `Dropped` — `error_log` was configured but the write failed
///   (disk full, permission drop, corrupted DLQ, etc.). Nothing
///   durable was written; on a disk queue the caller should
///   `resolve_dropped()` so the queue cursor holds and the event
///   replays on next start (disk-queue fail-stop wedge). On a memory
///   queue the caller still `resolve_recovered()` because there
///   is no replay path either way and `events_errored_unwritable`
///   has already been bumped as the operator signal.
#[must_use]
pub enum DlqRouteOutcome {
    Recovered,
    Dropped,
}

/// Per-event DLQ writer shared by every output's `consume` body. Writes
/// one `ErroredEventContext` record carrying the original event and a
/// human-readable reason; on `error_log` configured + write failure
/// bumps `events_errored_unwritable` and returns `Dropped` so the
/// caller can choose between `resolve_recovered` (memory queue) and
/// `resolve_dropped` (disk queue wedge). Does NOT touch the ack
/// handle directly.
///
/// `position` is the queue-side `AckPosition` snapshot taken from the
/// caller's `QueueAckHandle` *before* the ack is resolved. It is
/// required — not `Option` — because this helper is the sink-side
/// entry point and every sink call site holds an in-scope
/// `QueueAckHandle` at the moment of dispatch (the immediate next
/// statement resolves it via `resolve_ack_from_dlq_outcome`). The
/// pipeline-side runtime path (`runtime::write_errored_to_dlq`)
/// legitimately has no such handle and does not route through this
/// helper — it delegates to `emit_dlq_tracing_fallback` directly
/// with `position = None`. Requiring the argument here at the type
/// level locks the sink-side plumbing so a future refactor cannot
/// silently drop the `Meta`-arm `position` field back to `(none)`.
pub async fn route_event_to_dlq(
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: &OutputMetrics,
    output_name: &str,
    event: &Event,
    position: crate::queue::AckPosition,
    reason: &str,
) -> DlqRouteOutcome {
    use std::sync::atomic::Ordering;
    if let Some(writer) = error_log {
        let ctx = crate::pipeline::ErroredEventContext::Output {
            timestamp: chrono::Utc::now(),
            pipeline: String::new(),
            site: output_name.to_string(),
            reason: reason.to_string(),
            output_name: output_name.to_string(),
            event: crate::pipeline::OutputEvent::from_owned(event),
        };
        match writer.write(&ctx).await {
            Ok(()) => DlqRouteOutcome::Recovered,
            Err(write_err) => {
                // DLQ file itself failed. Bump the operator-facing
                // counter that pipeline-side `write_errored_to_dlq`
                // (in runtime.rs) also uses so both sink-side and
                // pipeline-side DLQ-write failures show up under
                // the same metric. `Dropped` signals the caller
                // to hold the cursor on a disk queue rather than
                // advance past an event with no durable trail.
                //
                // Tracing fallback is dispatched through the
                // ladder helper: whether any payload / metadata /
                // JSONL appears on the tracing side is the
                // operator's `control { error_log_fallback "..." }`
                // choice, not this call site's. `Off` (default)
                // keeps the line payload-free; `Meta` adds
                // structured metadata; `Full` emits `event_record`
                // as before.
                emit_dlq_tracing_fallback(
                    /* error_log_configured */ true,
                    error_log_fallback,
                    &ctx,
                    Some(position),
                    Some(&write_err),
                );
                metrics
                    .events_errored_unwritable
                    .fetch_add(1, Ordering::Relaxed);
                DlqRouteOutcome::Dropped
            }
        }
    } else {
        // No DLQ configured — the operator has declared no
        // durable recovery is required. The ladder helper emits a
        // payload-free summary line regardless of the fallback
        // value (row-A guard); the disposition still folds to
        // `Recovered` on any queue because there is no path
        // forward for this event and holding the cursor without
        // a recovery target would only wedge the pipeline.
        let ctx = crate::pipeline::ErroredEventContext::Output {
            timestamp: chrono::Utc::now(),
            pipeline: String::new(),
            site: output_name.to_string(),
            reason: reason.to_string(),
            output_name: output_name.to_string(),
            event: crate::pipeline::OutputEvent::from_owned(event),
        };
        emit_dlq_tracing_fallback(
            /* error_log_configured */ false,
            error_log_fallback,
            &ctx,
            Some(position),
            None,
        );
        DlqRouteOutcome::Recovered
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
mod pre_send_or_shutdown_tests {
    use super::pre_send_or_shutdown;
    use std::time::Duration;

    /// Happy path: the pre-send future completes first, the shutdown
    /// receiver is untouched, and the outcome is returned intact.
    #[tokio::test]
    async fn pre_send_completes_before_shutdown_returns_the_outcome() {
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let outcome = pre_send_or_shutdown(&mut rx, async { 42u32 }).await;
        assert_eq!(outcome, Some(42));
    }

    /// Shutdown fires while the pre-send is still pending: the
    /// helper returns `None` and the pre-send future is dropped.
    /// Pin this on wall time so a regression that dropped the
    /// shutdown arm would hang and time out here — the assert bound
    /// (200 ms against a 5 s sleep) is far under any plausible
    /// scheduler jitter.
    #[tokio::test]
    async fn shutdown_before_pre_send_completes_returns_none() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let helper = async move {
            pre_send_or_shutdown(&mut rx, tokio::time::sleep(Duration::from_secs(5)))
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
            pre_send_or_shutdown(&mut rx, tokio::time::sleep(Duration::from_secs(5))).await;
        assert!(outcome.is_none());
    }
}

#[cfg(test)]
mod resolve_ack_from_dlq_outcome_tests {
    use super::{DlqRouteOutcome, resolve_ack_from_dlq_outcome};
    use crate::metrics::OutputMetrics;
    use crate::queue::{AckDisposition, AckPosition, QueueAckHandle};
    use std::sync::atomic::Ordering;

    /// Recovered outcome always maps to `resolve_recovered` on
    /// both memory and disk queues — the DLQ record is durable,
    /// so cursor advancement is honest. The helper bumps
    /// `events_failed` for the recoverable failure so callers do
    /// not need to (and must not).
    #[tokio::test]
    async fn recovered_outcome_resolves_recovered_on_memory() {
        let (ack, mut rx) = QueueAckHandle::for_test();
        let metrics = OutputMetrics::default();
        resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Recovered, &metrics);
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Recovered))
        );
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn recovered_outcome_resolves_recovered_on_disk() {
        let position = AckPosition::Disk {
            seq: 4,
            offset: 128,
        };
        let (ack, mut rx) = QueueAckHandle::for_test_with_position(position);
        let metrics = OutputMetrics::default();
        resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Recovered, &metrics);
        assert_eq!(rx.recv().await, Some((position, AckDisposition::Recovered)));
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 1);
    }

    /// Dropped outcome on a memory queue still resolves as
    /// `Recovered` — memory queues cannot replay, so wedging
    /// would only cause loss without a recovery path.
    /// `events_errored_unwritable` (bumped inside
    /// `route_event_to_dlq`) is the operator's alarm signal for
    /// the loss, not a durable trace of the event, and the helper
    /// bumps `events_failed` here so the memory-queue terminal-
    /// failure count matches the disk-queue count (which is bumped
    /// on the ack side by `handle_ack_disposition`).
    #[tokio::test]
    async fn dropped_outcome_resolves_recovered_on_memory() {
        let (ack, mut rx) = QueueAckHandle::for_test();
        let metrics = OutputMetrics::default();
        resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Dropped, &metrics);
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Recovered))
        );
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 1);
    }

    /// Dropped outcome on a disk queue resolves as `Dropped` — the
    /// disk queue's consumer wedges on the Dropped disposition (the
    /// disk-queue fail-stop wedge) and holds the cursor at this
    /// position for replay on next start. The helper deliberately
    /// does NOT bump `events_failed` on this arm because the ack
    /// side (`handle_ack_disposition(Dropped)`) already bumps it
    /// when the consumer receives the disposition — bumping here
    /// would double-count.
    #[tokio::test]
    async fn dropped_outcome_resolves_dropped_on_disk() {
        let position = AckPosition::Disk {
            seq: 7,
            offset: 4242,
        };
        let (ack, mut rx) = QueueAckHandle::for_test_with_position(position);
        let metrics = OutputMetrics::default();
        resolve_ack_from_dlq_outcome(ack, DlqRouteOutcome::Dropped, &metrics);
        assert_eq!(rx.recv().await, Some((position, AckDisposition::Dropped)));
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod output_dlq_tracing_fallback_ladder_tests {
    //! Structural pins for the DLQ tracing fallback ladder.
    //!
    //! The ladder policy — enforced by `emit_dlq_tracing_fallback`
    //! and delegated to from every DLQ emission site — is:
    //!
    //! - `error_log` unset: payload-free line, fallback value
    //!   ignored (row-A ordering guard).
    //! - `error_log` set, fallback `Off` (default): payload-free
    //!   line, no `event_record`, no meta fields.
    //! - `error_log` set, fallback `Meta`: structured metadata
    //!   (`kind`, `fallback = "meta"`, `size`, `timestamp`,
    //!   `position`), no payload bytes.
    //! - `error_log` set, fallback `Full`: `event_record =
    //!   %ctx.to_jsonl()` — the pre-ladder shape, kept behind an
    //!   explicit opt-in.
    //!
    //! Testing via a tracing subscriber layer would give runtime
    //! observation of every combination but requires infra the
    //! workspace does not yet share. Instead, pin the shape at
    //! source level: the helper is the single site the ladder
    //! lives in, so grep pins there catch drift at every emission
    //! site simultaneously.

    fn helper_body() -> &'static str {
        let src = include_str!("mod.rs");
        let fn_start = src
            .find("pub(crate) fn emit_dlq_tracing_fallback(")
            .expect("emit_dlq_tracing_fallback must exist");
        // Bound the body at the function's own closing brace, not
        // at the next top-level `pub fn` — the doc comment of the
        // following function is above its `pub fn` line and would
        // otherwise be pulled into the extracted body, muddying
        // grep pins with unrelated `resolve_*` references from
        // that neighbour's docs.
        let fn_end_rel = src[fn_start..]
            .find("\n}\n")
            .expect("emit_dlq_tracing_fallback must have a closing brace at column 0");
        let end = fn_start + fn_end_rel + "\n}\n".len();
        let bytes = &src.as_bytes()[fn_start..end];
        std::str::from_utf8(bytes).expect("mod.rs must be utf-8")
    }

    /// `Off` arm (default): no `event_record`, no payload-carrying
    /// tracing fields. This is the confidentiality baseline — an
    /// operator who did not opt in must never see event bytes on
    /// the tracing side.
    #[test]
    fn ladder_off_arm_has_no_event_record() {
        let body = helper_body();
        let off_marker = body
            .find("ErrorLogFallback::Off =>")
            .expect("Off arm must exist");
        let next_arm = body[off_marker..]
            .find("ErrorLogFallback::Meta =>")
            .expect("Meta arm must follow Off");
        let off_block = &body[off_marker..off_marker + next_arm];
        assert!(
            !off_block.contains("event_record"),
            "Off arm must not emit event_record — payload leak on a \
             fallback the operator explicitly disabled"
        );
        assert!(
            off_block.contains("fallback = \"off\""),
            "Off arm must tag the tracing line with `fallback = \"off\"` \
             so operators can filter"
        );
    }

    /// `Meta` arm: structured metadata only. The confidentiality
    /// boundary between `Meta` and `Full` is the whole point of the
    /// ladder — a stray `event_record` here would silently upgrade
    /// every operator on `Meta` to `Full` semantics.
    #[test]
    fn ladder_meta_arm_has_structured_fields_but_no_event_record() {
        let body = helper_body();
        let meta_marker = body
            .find("ErrorLogFallback::Meta =>")
            .expect("Meta arm must exist");
        let next_arm = body[meta_marker..]
            .find("ErrorLogFallback::Full =>")
            .expect("Full arm must follow Meta");
        let meta_block = &body[meta_marker..meta_marker + next_arm];
        assert!(
            !meta_block.contains("event_record"),
            "Meta arm must not emit event_record — that upgrade belongs \
             to the Full opt-in"
        );
        assert!(
            meta_block.contains("fallback = \"meta\""),
            "Meta arm must tag `fallback = \"meta\"` for filter parity"
        );
        for field in ["size", "timestamp", "position"] {
            assert!(
                meta_block.contains(field),
                "Meta arm must emit `{field}` structured field so \
                 operators can correlate the record with metrics / \
                 replay tooling"
            );
        }
    }

    /// Sink-side helpers require a live `AckPosition` at the type
    /// level. Every route the ladder's `Meta` arm reaches from a
    /// sink caller therefore has a real queue position to emit,
    /// not `(none)`. This pin ensures a future refactor cannot
    /// silently regress `route_event_to_dlq` /
    /// `finalize_shutdown_singleton_disposition{,_ambiguous}` to
    /// `Option<AckPosition>` (or drop the parameter) without
    /// updating every call site — which is exactly what let the
    /// previous sink-side plumbing drop `position` back to `(none)` on the
    /// steady-state retry-exhausted path (the most common DLQ
    /// route) despite the ladder docs promising it.
    ///
    /// Detection is source-level: grep the four public helper
    /// signatures for `position: crate::queue::AckPosition,`
    /// (required, not `Option`). If a future variant needs to
    /// stay position-agnostic, add a separate helper — do not
    /// weaken these four.
    #[test]
    fn sink_side_dlq_helpers_require_ack_position() {
        let src = include_str!("mod.rs");

        // `route_event_to_dlq` is the shared shape every sink's
        // `consume` call reaches; the type-level requirement is
        // the load-bearing part of the fix, so its signature must
        // continue to require `AckPosition` directly (not `Option`).
        let start = src
            .find("pub async fn route_event_to_dlq(")
            .expect("route_event_to_dlq must exist");
        let sig_end = src[start..]
            .find(") ->")
            .expect("route_event_to_dlq must have a return arrow");
        let sig = &src[start..start + sig_end];
        assert!(
            sig.contains("position: crate::queue::AckPosition,"),
            "route_event_to_dlq must require an `AckPosition` (not \
             `Option<...>`) so sink-side callers cannot silently drop \
             the ladder's `Meta`-arm `position` field back to `(none)` \
             — see the doc comment on `route_event_to_dlq`",
        );
        assert!(
            !sig.contains("position: Option<crate::queue::AckPosition>"),
            "route_event_to_dlq takes `AckPosition` directly; wrapping \
             it in `Option` here defeats the type-level guarantee",
        );

        // `finalize_shutdown_singleton_disposition{,_ambiguous}` take
        // the whole `QueueAckHandle` (they own the resolve) and extract
        // position internally via `ack.position()` before handing it
        // to `route_event_to_dlq`. Pin the extraction so a future edit
        // cannot skip it and re-introduce the `(none)` regression.
        for finalize_helper in [
            "pub async fn finalize_shutdown_singleton_disposition(",
            "pub async fn finalize_shutdown_singleton_disposition_ambiguous(",
        ] {
            let start = src
                .find(finalize_helper)
                .unwrap_or_else(|| panic!("{} must exist", finalize_helper));
            // Find the closing brace at column 0 for the fn body.
            let body_end = src[start..]
                .find("\n}\n")
                .unwrap_or_else(|| panic!("{} must have a closing brace", finalize_helper));
            let body = &src[start..start + body_end];
            assert!(
                body.contains("let position = ack.position();"),
                "{finalize_helper} must extract `let position = ack.position();` \
                 before delegating to `route_event_to_dlq` so the `Meta`-arm \
                 `position` field carries the real queue position rather than \
                 `(none)`",
            );
        }
    }

    /// `Full` arm: kept for operators who explicitly opted into
    /// payload exposure on the tracing side. This preserves the
    /// pre-ladder `event_record = %ctx.to_jsonl()` shape so a
    /// `journalctl | jq` extraction path documented against
    /// earlier releases still works when opted in.
    #[test]
    fn ladder_full_arm_emits_event_record_jsonl() {
        let body = helper_body();
        let full_marker = body
            .find("ErrorLogFallback::Full =>")
            .expect("Full arm must exist");
        let block_end = body[full_marker..]
            .find("        }\n    }")
            .expect("Full arm must have a closing brace pair");
        let full_block = &body[full_marker..full_marker + block_end];
        assert!(
            full_block.contains("event_record = %ctx.to_jsonl()"),
            "Full arm must emit `event_record = %ctx.to_jsonl()` — this \
             is the payload-exposure opt-in the operator selected"
        );
        assert!(
            full_block.contains("fallback = \"full\""),
            "Full arm must tag `fallback = \"full\"` for filter parity"
        );
    }

    /// Row-A ordering guard: the unset-error_log branch checks
    /// `error_log_configured` BEFORE reading the fallback value,
    /// and its own tracing line has no `event_record` regardless
    /// of the fallback the operator inadvertently set. Without
    /// this ordering, an operator who set
    /// `error_log_fallback "full"` without setting `error_log`
    /// would leak payloads on every DLQ path — contradicting
    /// their own "no durable recovery needed" declaration.
    #[test]
    fn ladder_no_error_log_arm_ignores_fallback_and_has_no_event_record() {
        let body = helper_body();
        let guard_marker = body
            .find("if !error_log_configured {")
            .expect("row-A guard must exist");
        let after_guard = body[guard_marker..]
            .find("return;")
            .expect("row-A guard must return before fallback match");
        let guard_block = &body[guard_marker..guard_marker + after_guard];
        assert!(
            !guard_block.contains("event_record"),
            "row-A (unset error_log) branch must not emit event_record"
        );
        assert!(
            !guard_block.contains("ErrorLogFallback::"),
            "row-A branch must not switch on the fallback value — that \
             would let a stray `error_log_fallback \"full\"` upgrade the \
             confidentiality-declined path"
        );
        // Also verify the guard appears before the `match fallback`
        // block, not after — order matters for the invariant.
        let match_marker = body
            .find("match fallback {")
            .expect("fallback match must exist");
        assert!(
            guard_marker < match_marker,
            "row-A guard must run BEFORE the fallback match, otherwise \
             the ordering invariant collapses"
        );
    }

    /// Disposition ownership: the ladder must change the tracing
    /// emission only, never the ack disposition. `route_*` helpers
    /// still own the `resolve_ack_from_dlq_outcome` call for every
    /// path; the ladder helper itself has no `resolve_*` inside it.
    #[test]
    fn ladder_helper_does_not_touch_ack_disposition() {
        let body = helper_body();
        assert!(
            !body.contains("resolve_ack_from_dlq_outcome"),
            "emit_dlq_tracing_fallback must not resolve ack disposition — \
             that ownership stays with the route_* callers so the ladder \
             cannot silently change disk-queue wedge / memory-queue fold \
             semantics"
        );
        assert!(
            !body.contains("resolve_delivered")
                && !body.contains("resolve_recovered")
                && !body.contains("resolve_dropped"),
            "emit_dlq_tracing_fallback must not touch any ack resolve method"
        );
    }
}

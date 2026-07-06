//! Daemon runtime: wires inputs, pipelines, output queues, and outputs
//! into a running system.
//!
//! Runtime does NOT count metrics — each component counts its own.
//! Runtime only collects metrics handles into MetricsRegistry for stats.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};

use crate::control::ControlServer;
use crate::dsl::ast::*;
use crate::dsl::props;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::{MetricsRegistry, PipelineMetrics};
use crate::modules::{self, HasMetrics, ModuleRegistry};
use crate::pipeline::CompiledConfig;
use crate::queue::{self, QueueConfig, QueueSender};
use crate::tap::TapRegistry;

pub struct Runtime {
    shutdown_tx: watch::Sender<bool>,
    handles: Vec<tokio::task::JoinHandle<()>>,
    config_file: PathBuf,
    compiled_config: CompiledConfig,
}

impl Runtime {
    pub async fn start(config: CompiledConfig, config_file: PathBuf) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        let mut registry = ModuleRegistry::new();
        modules::register_builtins(&mut registry);
        // Future: dynamic plugin loading from /etc/limpid/plugins/

        init_geoip(&config);
        let table_store = init_tables(&config)?;

        let mut func_registry = FunctionRegistry::new();
        crate::functions::register_builtins(&mut func_registry, table_store);
        crate::functions::register_user_functions(&mut func_registry, &config);
        let func_registry = Arc::new(func_registry);

        config.validate()?;
        let registry = Arc::new(registry);

        let mut metrics_registry = MetricsRegistry::new();
        let tap = TapRegistry::new();

        // Optional dead-letter queue for events that fail in `process`
        // or that an output drops after exhausting retries
        // (retry-exhausted recovery). `control { error_log "..." }`
        // opts in to file-based recovery; when unset, the runtime
        // falls back to a structured `tracing::warn!` / `error!` line
        // (pipeline path emits the full JSONL on the tracing line; the
        // output path emits the failure summary without the full
        // payload). The path is validated at startup (parent dir
        // reachable) so operator typos surface before the first
        // failure event.
        //
        // Built *before* outputs are constructed so each batched output
        // (`http`, `otlp_http`, `otlp_grpc`) receives the handle via
        // its constructor — no post-construction setter, no interior
        // mutability. Non-batched outputs ignore the parameter; the
        // queue consumer hands `error_log` to `run_queue_consumer`,
        // which routes the retry-exhausted payload to the DLQ once
        // each handle resolves.
        let error_log_path = config
            .global_blocks
            .get("control")
            .and_then(|p| props::get_string(p, "error_log"));
        let error_log = match error_log_path {
            Some(p) => {
                let writer = crate::error_log::ErrorLogWriter::new(PathBuf::from(p));
                writer.validate_at_startup().await?;
                Some(Arc::new(writer))
            }
            None => None,
        };

        // Single bundle threaded into every Input/Output factory. Future
        // build-time dependencies (transport-key registry, metrics hooks)
        // land as new fields on this struct rather than as new parameters.
        let build_ctx = crate::modules::BuildContext {
            funcs: Arc::clone(&func_registry),
            error_log: error_log.as_ref().map(Arc::clone),
            shutdown_signal: shutdown_rx.clone(),
        };

        // --- 1. Create outputs (each output owns its own OutputMetrics) ---
        let mut output_senders: HashMap<String, QueueSender> = HashMap::new();
        let mut output_receivers = Vec::new();

        for (name, output_def) in &config.outputs {
            let queue_config =
                QueueConfig::from_output_properties(name, output_def.properties.user_properties())?;
            // Retry config is parsed by each output's `from_properties`
            // (outputs own retry + DLQ). The runtime no longer needs a
            // copy here.
            let (mut sender, receiver) = queue::create_queue(name.clone(), queue_config)?;

            // `output_def.properties` is a `ModuleProperties`: it carries the
            // resolved `type` already, so `create_output` doesn't take a
            // separate type_name argument (and can't be passed one — the
            // strip is the whole point). `BuildContext` carries `funcs` and
            // the optional `error_log` so outputs can stash them at
            // construction time.
            let created = match registry.create_output(name, &output_def.properties, &build_ctx) {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "failed to create output '{}': {} — aborting startup",
                        name, e
                    );
                    for h in &handles {
                        h.abort();
                    }
                    for h in handles {
                        let _ = h.await;
                    }
                    return Err(e);
                }
            };

            // Attach metrics so QueueSender::send counts events_received.
            sender.attach_metrics(Arc::clone(&created.metrics));
            output_senders.insert(name.clone(), sender);

            // Collect metrics handle (output owns the data, we just hold a reference)
            let output_metrics = Arc::clone(&created.metrics);
            metrics_registry.register_output(name, created.metrics);
            tap.register(&format!("output {}", name)).await;

            output_receivers.push((name.clone(), receiver, created.output, output_metrics));
        }

        // Start queue consumers (no metrics counting here — output does it)
        for (_name, receiver, writer, output_metrics) in output_receivers {
            let shutdown = shutdown_rx.clone();
            let tap_clone = tap.clone();
            let error_log_for_consumer = error_log.as_ref().map(Arc::clone);
            handles.push(tokio::spawn(async move {
                queue::run_queue_consumer(
                    receiver,
                    writer,
                    Some(tap_clone),
                    output_metrics,
                    error_log_for_consumer,
                    shutdown,
                )
                .await;
            }));
        }

        let output_senders = Arc::new(output_senders);

        // --- 2. Group pipelines by input ---
        //
        // A pipeline with `input a, b;` (fan-in) is registered under every listed
        // input. Events from each input are still fed into the pipeline's
        // per-input worker dispatcher; since a single `PipelineWorker` instance
        // is shared across inputs (wrapped in Arc at spawn time), its metrics
        // aggregate across inputs without per-input attribution — by design.
        let mut input_pipelines: HashMap<String, Vec<Arc<PipelineWorker>>> = HashMap::new();

        for pipeline_def in config.pipelines.values() {
            let worker = Arc::new(PipelineWorker::new(pipeline_def.clone()));
            metrics_registry.register_pipeline(&pipeline_def.name, worker.metrics());
            let input_names = get_pipeline_inputs(pipeline_def);
            for input_name in input_names {
                input_pipelines
                    .entry(input_name.clone())
                    .or_default()
                    .push(Arc::clone(&worker));
            }
        }

        // --- 2b. Register tap points for inputs and processes ---
        for input_name in input_pipelines.keys() {
            tap.register(&format!("input {}", input_name)).await;
        }
        for proc_name in config.processes.keys() {
            tap.register(&format!("process {}", proc_name)).await;
        }

        // --- 3. Start inputs (each input owns its own InputMetrics) ---
        let compiled_config = config.clone();
        let config = Arc::new(config);

        let mut input_senders: HashMap<
            String,
            (mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>),
        > = HashMap::new();

        for (input_name, pipelines) in input_pipelines {
            let input_def = config
                .inputs
                .get(&input_name)
                .ok_or_else(|| anyhow::anyhow!("input '{}' not found", input_name))?;

            let queue_size =
                props::get_positive_int(input_def.properties.user_properties(), "queue_size")?
                    .unwrap_or(4096) as usize;
            let (event_tx, event_rx) = mpsc::channel::<Event>(queue_size);

            // Pipeline workers subscribed to this input. A pipeline with fan-in
            // (`input a, b;`) appears in the worker list of both inputs — its
            // merge semantics is implicit: two dispatcher tasks feeding the
            // same `PipelineWorker`, serialized through its own `run_pipeline`
            // call per event. No ordering guarantee between inputs.
            let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(pipelines);
            let ctx = PipelineContext {
                output_senders: Arc::clone(&output_senders),
                config: Arc::clone(&config),
                funcs: Arc::clone(&func_registry),
                tap: tap.clone(),
                error_log: error_log.as_ref().map(Arc::clone),
            };
            let iname = input_name.clone();
            let shutdown_for_worker = shutdown_rx.clone();
            let sender_for_inject = event_tx.clone();
            handles.push(tokio::spawn(async move {
                run_pipeline_workers(event_rx, &workers, &ctx, &iname, shutdown_for_worker).await;
            }));

            // Input — registry builds, spawns, and returns metrics handle.
            // `input_def.properties` carries the resolved `type`; no separate
            // type_name argument needed (see ModuleProperties rationale).
            let created = match registry.create_input(
                &input_name,
                &input_def.properties,
                &build_ctx,
                event_tx,
                shutdown_rx.clone(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "failed to start input '{}': {} — aborting startup",
                        input_name, e
                    );
                    for h in &handles {
                        h.abort();
                    }
                    for h in handles {
                        let _ = h.await;
                    }
                    return Err(e);
                }
            };
            input_senders.insert(
                input_name.clone(),
                (sender_for_inject, Arc::clone(&created.metrics)),
            );
            metrics_registry.register_input(&input_name, created.metrics);
            handles.push(created.handle);
        }

        // --- 4. Start control socket (after all metrics are registered) ---
        let metrics_registry = Arc::new(metrics_registry);
        let control_path = config
            .global_blocks
            .get("control")
            .and_then(|p| props::get_string(p, "socket"));
        // Validate the control socket's parent BEFORE the control
        // task is spawned. `ControlServer::run` returns `()` and is
        // fire-and-forget from the runtime's perspective, so a
        // fail-closed check inside the task would just make the
        // control socket die silently while the daemon runs on.
        // Bailing here stops the whole startup — the same shape as
        // `ErrorLogWriter::validate_at_startup`.
        crate::control::validate_control_socket_parent(control_path.as_deref())?;
        let started_at = std::time::Instant::now();
        let control = ControlServer::new(
            control_path,
            tap.clone(),
            Arc::clone(&metrics_registry),
            Arc::clone(&config),
            input_senders,
            Arc::clone(&output_senders),
            started_at,
        );
        let s = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            control.run(s).await;
        }));

        info!("limpid daemon started");
        Ok(Self {
            shutdown_tx,
            handles,
            config_file,
            compiled_config,
        })
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn compiled_config(&self) -> CompiledConfig {
        self.compiled_config.clone()
    }

    pub async fn shutdown(self) {
        use std::time::Duration;
        use tokio::time::timeout;

        const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

        info!(
            "initiating graceful shutdown (timeout: {}s)",
            SHUTDOWN_TIMEOUT.as_secs()
        );
        let _ = self.shutdown_tx.send(true);

        // Collect abort handles before moving JoinHandles into join_all
        let abort_handles: Vec<_> = self.handles.iter().map(|h| h.abort_handle()).collect();

        match timeout(SHUTDOWN_TIMEOUT, Self::join_all(self.handles)).await {
            Ok(()) => {
                info!("shutdown complete");
            }
            Err(_) => {
                error!(
                    "shutdown timed out after {}s — aborting remaining tasks",
                    SHUTDOWN_TIMEOUT.as_secs()
                );
                for ah in &abort_handles {
                    ah.abort();
                }
            }
        }
    }

    async fn join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
        for handle in handles {
            if let Err(e) = handle.await
                && e.is_panic()
            {
                error!("task panicked during shutdown: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Global subsystem initialization
// ---------------------------------------------------------------------------

fn init_geoip(config: &CompiledConfig) {
    let db_path = config
        .global_blocks
        .get("geoip")
        .and_then(|p| props::get_string(p, "database"))
        .map(PathBuf::from);
    crate::functions::geoip::init(db_path.as_ref());
}

pub(crate) fn init_tables(config: &CompiledConfig) -> Result<crate::functions::table::TableStore> {
    use crate::dsl::ast::Property;
    use crate::functions::table::{TableConfig, TableStore};
    use std::time::Duration;

    let mut configs = Vec::new();

    if let Some(props) = config.global_blocks.get("table") {
        for prop in props {
            if let Property::Block {
                key: table_name,
                properties: inner_props,
                ..
            } = prop
            {
                let load_path = props::get_string(inner_props, "load").map(PathBuf::from);
                let max = props::get_positive_int(inner_props, "max")?.map(|n| n as usize);
                let ttl = props::get_positive_int(inner_props, "ttl")?.map(Duration::from_secs);

                configs.push(TableConfig {
                    name: table_name.clone(),
                    max,
                    default_ttl: ttl,
                    load_path,
                });
            }
        }
    }

    TableStore::from_configs(configs)
}

// ---------------------------------------------------------------------------
// Pipeline context — shared references for pipeline execution
// ---------------------------------------------------------------------------

struct PipelineContext {
    output_senders: Arc<HashMap<String, QueueSender>>,
    config: Arc<CompiledConfig>,
    funcs: Arc<FunctionRegistry>,
    tap: TapRegistry,
    /// Dead-letter queue writer used for: (1) `process` runtime
    /// errors, (2) output retry-exhausted payloads, (3)
    /// batched-output shutdown-flush leftovers, (4) runtime-side
    /// enqueue failures (queue closed, disk write error, unknown
    /// output). `None` when `control { error_log }` is unset — the
    /// fallback shape is now uniform across sites: both the
    /// process-side / enqueue-failure paths (`write_errored_to_dlq`
    /// in this module) and the sink-side retry-exhaustion /
    /// shutdown-drain paths (`route_event_to_dlq` and
    /// `route_shutdown_batch_to_dlq` in
    /// `crates/limpid/src/modules/mod.rs`) emit a
    /// `tracing::error!` line with the **full failure JSONL** in
    /// the `event_record` structured field. The payload is
    /// recoverable from journald in either case, and both converge
    /// on the same file once `error_log` is configured.
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
}

// ---------------------------------------------------------------------------
// Pipeline worker — owns its own metrics via HasMetrics
// ---------------------------------------------------------------------------

struct PipelineWorker {
    def: PipelineDef,
    metrics: Arc<PipelineMetrics>,
}

impl PipelineWorker {
    fn new(def: PipelineDef) -> Self {
        Self {
            def,
            metrics: Arc::new(PipelineMetrics::default()),
        }
    }
}

impl HasMetrics for PipelineWorker {
    type Stats = PipelineMetrics;
    fn metrics(&self) -> Arc<PipelineMetrics> {
        Arc::clone(&self.metrics)
    }
}

async fn run_pipeline_workers(
    mut event_rx: mpsc::Receiver<Event>,
    workers: &[Arc<PipelineWorker>],
    ctx: &PipelineContext,
    input_name: &str,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    info!(
        "pipeline worker for input '{}' started ({} pipeline(s))",
        input_name,
        workers.len()
    );

    let input_tap_key = format!("input {}", input_name);

    // Per-worker arena. Bump-allocator state is owned by this task and
    // recycled via `Bump::reset()` after each event, so the underlying
    // chunk-group is malloc'd once at task startup and stays around as
    // long as one event's working set fits in the initial chunk.
    // 1 MiB comfortably fits the heaviest realistic OCSF compose (the
    // D pipeline's measured workspace tree under load is well under
    // 256 KiB). Without enough headroom, bumpalo grows the chunk
    // chain mid-event and `reset` then frees the excess — defeating
    // the whole purpose of pooling.
    let mut bump = bumpalo::Bump::with_capacity(1024 * 1024);

    loop {
        let event = tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Close the receiver first, then drain with
                    // `recv().await` until `None`. The previous
                    // `try_recv()` snapshot loop raced with input
                    // tasks that had reserved an mpsc permit but not
                    // yet written the value — the loop could exit
                    // observing an empty channel while a permit-holder
                    // was mid-write, silently dropping that event.
                    // `close()` + `recv-until-None` is the tokio-
                    // documented contract for reading every value that
                    // was already sent or is being sent by an
                    // outstanding permit-holder before terminating.
                    // Any input-side `send` that races the close and
                    // has not yet reserved a permit wakes with
                    // `SendError`, surfaced by the input tasks
                    // themselves (see `run_input`).
                    event_rx.close();
                    while let Some(event) = event_rx.recv().await {
                        process_event(&event, workers, ctx, &input_tap_key, &mut bump).await;
                        bump.reset();
                    }
                    break;
                }
                continue;
            }

            event = event_rx.recv() => {
                match event {
                    Some(e) => e,
                    None => break,
                }
            }
        };
        process_event(&event, workers, ctx, &input_tap_key, &mut bump).await;
        bump.reset();
    }

    info!("pipeline worker for input '{}' stopped", input_name);
}

async fn run_pipeline_with_outputs(
    pipeline: &PipelineDef,
    event: &Event,
    ctx: &PipelineContext,
    bump: &mut bumpalo::Bump,
) -> Result<crate::pipeline::PipelineRunResult> {
    // No `--test-pipeline` trace collector on the daemon hot path —
    // passing `None` skips every trace push (and the `format!` /
    // `to_string` work behind it) in `run_pipeline`, since nothing
    // here reads `PipelineRunResult::trace`.
    let mut result = crate::pipeline::run_pipeline(
        pipeline,
        event,
        &ctx.config,
        &ctx.funcs,
        Some(&ctx.tap),
        None,
        bump,
    )?;

    // Drain the per-event outputs vec into the queues. After this
    // change every output statement enqueues a plain `OwnedEvent`
    // regardless of the queue kind; render happens consumer-side
    // inside each sink's `Output::consume`.
    //
    // `result.had_outputs` was set inside `run_pipeline` *before* the
    // vec was moved out here, so the downstream
    // `events_finished` / `events_discarded` decision still observes
    // the original semantics (Finished AND emitted ≥1 output → finished).
    let outputs = std::mem::take(&mut result.outputs);
    // Each failed enqueue carries the per-output `OwnedEvent`
    // snapshot so the DLQ record reflects the value the pipeline
    // produced for that sink. The input event the function received
    // is not equivalent: the pipeline may have produced a different
    // egress for the output, and `inject output` replay must
    // preserve the bytes the sink would have shipped.
    let mut failed_outputs: Vec<(String, crate::event::OwnedEvent)> = Vec::new();
    for (output_name, event) in outputs {
        if let Some(sender) = ctx.output_senders.get(&output_name) {
            // Clone before send: the sender consumes `event`. `OwnedEvent`
            // clone is cheap (Bytes are refcounted; workspace cost
            // scales with the per-event populated keys).
            let snapshot = event.clone();
            if let Err(e) = sender.send(event).await {
                // QueueSender::send already bumped per-output
                // `events_failed` on the Err branch — that gives the
                // operator per-output visibility. Collect the names
                // here so the pipeline-level disposition (below)
                // routes the event through the DLQ instead of
                // counting it as `events_finished`.
                error!(
                    "pipeline '{}': enqueue to output '{}' failed: {}",
                    pipeline.name, output_name, e
                );
                failed_outputs.push((output_name, snapshot));
            }
        } else {
            // Unknown output name slipped past startup validation —
            // bug, but recoverable: treat as an enqueue failure so
            // the event still hits the DLQ.
            error!(
                "pipeline '{}': output '{}' not found",
                pipeline.name, output_name
            );
            failed_outputs.push((output_name, event));
        }
    }

    // Any enqueue failure overrides the termination: the pipeline
    // body finished, but the event never reached (some of) the
    // configured downstream queues. Routing through the existing
    // `Errored` termination path gives the operator the same
    // recovery affordances as a `process` error — DLQ entry plus an
    // `events_errored` increment — instead of a silent
    // `events_finished` count on an event that was effectively lost.
    //
    // The DLQ records are emitted **per failed output** so each one
    // can be replayed independently through
    // `limpidctl inject output <name>` — joining them into one
    // multi-output record would force the operator to re-run every
    // sibling sink for one enqueue failure.
    if !failed_outputs.is_empty() {
        let reason = "output enqueue failed (queue closed, disk write \
             error, or unknown output)"
            .to_string();
        result.termination = crate::pipeline::PipelineTermination::Errored;
        for (output_name, snapshot) in failed_outputs {
            result
                .errored
                .push(crate::pipeline::ErroredEventContext::Output {
                    timestamp: chrono::Utc::now(),
                    pipeline: pipeline.name.clone(),
                    site: format!("{} enqueue", output_name),
                    reason: reason.clone(),
                    output_name: output_name.clone(),
                    event: crate::pipeline::OutputEvent::from_owned(&snapshot),
                });
        }
    }

    Ok(result)
}

async fn process_event(
    event: &Event,
    workers: &[Arc<PipelineWorker>],
    ctx: &PipelineContext,
    input_tap_key: &str,
    bump: &mut bumpalo::Bump,
) {
    ctx.tap.emit(input_tap_key, event).await;
    for (i, worker) in workers.iter().enumerate() {
        worker
            .metrics
            .events_received
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Pass the event by reference. `run_pipeline` views it into
        // the arena (read-only on the input owned form) and any DLQ
        // path constructs a fresh `OwnedEvent` via `to_owned()` from
        // the arena view — so multiple fan-out workers can share the
        // same input without cloning the workspace HashMap.
        //
        // Reset only between fan-out workers (skip before the first):
        // the caller's outer reset already cleared the bump for this
        // event; the redundant inner reset on single-worker fan-out
        // would just thrash the chunk-chain pointer for nothing.
        if i > 0 {
            bump.reset();
        }
        match run_pipeline_with_outputs(&worker.def, event, ctx, bump).await {
            Ok(result) => {
                use crate::pipeline::PipelineTermination;
                match result.termination {
                    PipelineTermination::Dropped => {
                        worker
                            .metrics
                            .events_dropped
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    PipelineTermination::Errored => {
                        worker
                            .metrics
                            .events_errored
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Drain every accumulated DLQ record. For a
                        // pipeline-side failure this is exactly one
                        // record; for a runtime-side per-failed-output
                        // enqueue failure it is one record per output.
                        // The metric increments once per pipeline run
                        // (= one logical event lost), independent of
                        // the record count, matching prior semantics.
                        if result.errored.is_empty() {
                            error!(
                                "pipeline '{}': Errored termination without error context — bug",
                                worker.def.name
                            );
                        } else {
                            for err_ctx in &result.errored {
                                write_errored_to_dlq(
                                    err_ctx,
                                    &worker.metrics,
                                    ctx.error_log.as_ref(),
                                )
                                .await;
                            }
                        }
                    }
                    PipelineTermination::Finished => {
                        if !result.had_outputs {
                            worker
                                .metrics
                                .events_discarded
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            worker
                                .metrics
                                .events_finished
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
            Err(e) => {
                // Pipeline body raised a runtime error that wasn't
                // caught by `process` (= came out of expression
                // evaluation in `error <expr>`, switch discriminant/
                // pattern, or `if` condition).
                // Pre-fix this branch only logged and the event
                // disappeared without an `events_errored` increment
                // or a DLQ entry — operators had no replay path,
                // contradicting the documented runtime-error
                // contract. Route through the same DLQ path as
                // `PipelineTermination::Errored` so the metric +
                // file record stay consistent across both shapes
                // of runtime error.
                worker
                    .metrics
                    .events_errored
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let owned = event.to_owned();
                let err_ctx = crate::pipeline::ErroredEventContext::Process {
                    timestamp: chrono::Utc::now(),
                    pipeline: worker.def.name.clone(),
                    site: "(pipeline body)".to_string(),
                    reason: e.to_string(),
                    event: crate::pipeline::ProcessEvent::from_owned(&owned),
                };
                write_errored_to_dlq(&err_ctx, &worker.metrics, ctx.error_log.as_ref()).await;
            }
        }
    }
}

/// Persist an errored event to the dead-letter queue, or — if no
/// `error_log` is configured — emit a structured tracing line so the
/// record never disappears silently.
///
/// Shared by both runtime-error shapes the orchestrator surfaces:
/// `PipelineTermination::Errored` (a `process` raised an error mid-
/// pipeline) and an `Err` return from `run_pipeline` itself
/// (expression evaluation under `error`/`if`/`switch` or a process
/// body expression raised an error). Keeping the two on the same routing helper
/// guarantees the operator-visible behaviour stays in lockstep:
/// same JSONL on disk, same `events_errored` counter, same
/// `events_errored_unwritable` semantics when the DLQ write itself
/// fails.
async fn write_errored_to_dlq(
    err_ctx: &crate::pipeline::ErroredEventContext,
    worker_metrics: &PipelineMetrics,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
) {
    match error_log {
        Some(writer) => {
            if let Err(e) = writer.write(err_ctx).await {
                worker_metrics
                    .events_errored_unwritable
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                error!(
                    event_record = %err_ctx.to_jsonl(),
                    "error_log: write failed: {} — record below for manual recovery",
                    e
                );
            }
        }
        None => {
            // No DLQ configured — surface the record as a structured
            // tracing line so the failure data is never silently
            // lost. Operators can grep / `journalctl | jq` it.
            error!(
                event_record = %err_ctx.to_jsonl(),
                "pipeline '{}': site '{}' errored; configure `control {{ error_log \"...\" }}` for file-based DLQ",
                err_ctx.pipeline(),
                err_ctx.site()
            );
        }
    }
}

/// Return the list of input names a pipeline subscribes to (fan-in).
///
/// Empty if no `input` statement is present. A pipeline declared with
/// `input a, b;` returns `["a", "b"]`; the legacy single-input form
/// `input a;` returns `["a"]`.
fn get_pipeline_inputs(pipeline: &PipelineDef) -> &[String] {
    for stmt in &pipeline.body {
        if let PipelineStatement::Input(names) = stmt {
            return names;
        }
    }
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn pipeline_def(src: &str) -> PipelineDef {
        let cfg = parse_config(src).unwrap();
        for def in cfg.definitions {
            if let Definition::Pipeline(p) = def {
                return p;
            }
        }
        panic!("no pipeline in src");
    }

    #[test]
    fn get_pipeline_inputs_single() {
        let def = pipeline_def("def pipeline p { input a; drop }");
        assert_eq!(get_pipeline_inputs(&def), &["a".to_string()]);
    }

    #[test]
    fn get_pipeline_inputs_fan_in() {
        let def = pipeline_def("def pipeline p { input a, b, c; drop }");
        assert_eq!(
            get_pipeline_inputs(&def),
            &["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn get_pipeline_inputs_missing_input_stmt() {
        // Not valid per compiled-config validation, but the helper alone should
        // still return an empty slice rather than panic.
        let def = pipeline_def("def pipeline p { drop }");
        assert!(get_pipeline_inputs(&def).is_empty());
    }

    /// End-to-end-ish fan-in runtime test: two independent mpsc channels
    /// (simulating two input sources) both push events into a dispatcher
    /// that shares a single `PipelineWorker`. Events from both sides land
    /// on the same pipeline — we verify via the worker's own metrics.
    #[tokio::test]
    async fn fan_in_merges_two_inputs_into_single_worker() {
        // Minimal pipeline with a single `drop` step; the body doesn't matter
        // for this test — we only care that events flow through the worker.
        let def = pipeline_def("def pipeline p { input a, b; drop }");
        let worker = Arc::new(PipelineWorker::new(def));
        let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(vec![Arc::clone(&worker)]);

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx_a, rx_a) = mpsc::channel::<Event>(16);
        let (tx_b, rx_b) = mpsc::channel::<Event>(16);

        let tap = TapRegistry::new();
        tap.register("input a").await;
        tap.register("input b").await;

        // A throwaway compiled config is required by PipelineContext; an empty
        // one suffices because the pipeline body is `drop` (no output lookup,
        // no process lookup).
        let cfg = CompiledConfig::from_config(parse_config("").unwrap()).unwrap();
        let ctx_a = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            config: Arc::new(cfg.clone()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: tap.clone(),
            error_log: None,
        };
        let ctx_b = PipelineContext {
            output_senders: Arc::clone(&ctx_a.output_senders),
            config: Arc::clone(&ctx_a.config),
            funcs: Arc::clone(&ctx_a.funcs),
            tap: tap.clone(),
            error_log: None,
        };

        let workers_a = Arc::clone(&workers);
        let workers_b = Arc::clone(&workers);
        let sd_a = shutdown_rx.clone();
        let sd_b = shutdown_rx.clone();
        let h_a = tokio::spawn(async move {
            run_pipeline_workers(rx_a, &workers_a, &ctx_a, "a", sd_a).await;
        });
        let h_b = tokio::spawn(async move {
            run_pipeline_workers(rx_b, &workers_b, &ctx_b, "b", sd_b).await;
        });

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        for _ in 0..3 {
            tx_a.send(Event::new(Bytes::from_static(b"from_a"), addr))
                .await
                .unwrap();
        }
        for _ in 0..5 {
            tx_b.send(Event::new(Bytes::from_static(b"from_b"), addr))
                .await
                .unwrap();
        }
        drop(tx_a);
        drop(tx_b);

        // Wait for both dispatchers to drain (they exit when their senders drop).
        tokio::time::timeout(Duration::from_secs(2), async {
            let _ = h_a.await;
            let _ = h_b.await;
        })
        .await
        .expect("dispatchers should drain promptly");

        // All 8 events should have been attributed to the shared worker.
        assert_eq!(worker.metrics.events_received.load(Ordering::Relaxed), 8);
        assert_eq!(worker.metrics.events_dropped.load(Ordering::Relaxed), 8);
    }

    fn make_err_ctx(reason: &str) -> crate::pipeline::ErroredEventContext {
        let addr = std::net::SocketAddr::from_str("127.0.0.1:0").unwrap();
        let ev = Event::new(Bytes::from_static(b"test-event"), addr);
        crate::pipeline::ErroredEventContext::Process {
            timestamp: chrono::Utc::now(),
            pipeline: "test_pipeline".to_string(),
            site: "(test process)".to_string(),
            reason: reason.to_string(),
            event: crate::pipeline::ProcessEvent::from_owned(&ev),
        }
    }

    #[tokio::test]
    async fn write_errored_to_dlq_writes_to_configured_error_log() {
        // The shared DLQ-routing helper feeds the writer when one is
        // configured. Pin that the failure JSONL actually lands in
        // the file — this is the recovery path operators rely on.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("dlq.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(log_path.clone()));
        let metrics = PipelineMetrics::default();
        let err_ctx = make_err_ctx("simulated runtime error");

        write_errored_to_dlq(&err_ctx, &metrics, Some(&writer)).await;

        // Errored counter is bumped at the caller (worker.metrics);
        // the helper itself only writes. Verify the JSONL is on disk.
        // `ErrorLogWriter::write` now awaits `shutdown()` on the file
        // handle before returning, so the record is visible by the
        // time the helper's future resolves — but keep the async
        // reader for symmetry with the runtime path.
        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            contents.contains("simulated runtime error"),
            "DLQ file must contain the reason; got: {contents}"
        );
        assert!(
            contents.contains("test_pipeline"),
            "DLQ file must name the pipeline; got: {contents}"
        );
        assert_eq!(
            metrics.events_errored_unwritable.load(Ordering::Relaxed),
            0,
            "unwritable counter must not bump on a successful write"
        );
    }

    #[tokio::test]
    async fn write_errored_to_dlq_without_writer_does_not_panic() {
        // Baseline: when `error_log` isn't configured, the helper
        // emits a structured tracing line instead. The structured
        // line is observed via `tracing` subscribers in operator
        // setups; the test here pins the no-panic contract so the
        // logged-only branch can't regress to an unwrap somewhere.
        let metrics = PipelineMetrics::default();
        let err_ctx = make_err_ctx("no DLQ configured");

        write_errored_to_dlq(&err_ctx, &metrics, None).await;

        // Sanity: no metric is touched on this branch (the caller
        // already bumped events_errored before calling us).
        assert_eq!(metrics.events_errored_unwritable.load(Ordering::Relaxed), 0,);
    }

    #[tokio::test]
    async fn output_enqueue_failure_splits_one_dlq_record_per_failed_output() {
        // When a pipeline lists multiple `output` targets and none of
        // them resolve at runtime (= unknown output names slipped past
        // startup validation, or queues were torn down), the enqueue
        // path must produce ONE DLQ record per failed output rather
        // than a single joined record. That lets the operator replay
        // each output independently via
        // `limpidctl inject output <name>` without re-running sibling
        // sinks that were already fine.
        use crate::pipeline::ErroredEventContext;
        let def = pipeline_def("def pipeline p { input i; output sink_a; output sink_b; finish }");

        let cfg = CompiledConfig::from_config(parse_config("").unwrap()).unwrap();
        let ctx = PipelineContext {
            // Empty output_senders → every `output` statement falls
            // into the "unknown output" arm and is reported as a
            // failed enqueue. This is exactly the codepath the runtime
            // is meant to split per-output.
            output_senders: Arc::new(HashMap::new()),
            config: Arc::new(cfg),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
        };

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        let event = Event::new(Bytes::from_static(b"payload"), addr);
        let mut bump = bumpalo::Bump::new();
        let result = run_pipeline_with_outputs(&def, &event, &ctx, &mut bump)
            .await
            .expect("run_pipeline_with_outputs should not propagate");

        assert_eq!(
            result.termination,
            crate::pipeline::PipelineTermination::Errored
        );
        assert_eq!(
            result.errored.len(),
            2,
            "two failed outputs must produce two DLQ records"
        );
        let mut names: Vec<String> = result
            .errored
            .iter()
            .map(|ctx| match ctx {
                ErroredEventContext::Output {
                    output_name, site, ..
                } => {
                    assert!(site.ends_with(" enqueue"), "unexpected site: {site}");
                    assert_eq!(*site, format!("{} enqueue", output_name));
                    output_name.clone()
                }
                other => panic!("expected Output variant, got {:?}", other),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["sink_a".to_string(), "sink_b".to_string()]);
    }

    /// Structural pin: the pipeline worker's shutdown arm closes
    /// `event_rx` and drains with `recv().await` until `None`, not
    /// `try_recv()` snapshot. The old snapshot loop had the same
    /// permit-holder race as the output queue drain: an input task
    /// that had reserved an mpsc permit but not yet written the
    /// value would complete after the worker exited, silently
    /// dropping the event. Mirror-tested at the tokio-mpsc level in
    /// `queue::tests::tokio_mpsc_close_then_permit_send_still_visible`.
    #[test]
    fn pipeline_worker_shutdown_arm_uses_close_recv_pattern() {
        let src = include_str!("runtime.rs");
        // Anchor on the specific inner select arm inside
        // `run_pipeline_workers`, not the outer `let event = tokio::select!`
        // that races receive vs shutdown.
        let marker = "// Close the receiver first, then drain with";
        let start = src
            .find(marker)
            .expect("pipeline worker shutdown drain marker must exist");
        let tail = &src[start..];
        let body_end = tail.find("break;").expect("shutdown arm must break out");
        let body = &tail[..body_end];

        assert!(
            body.contains("event_rx.close()"),
            "pipeline worker shutdown must close event_rx before draining",
        );
        assert!(
            body.contains("event_rx.recv().await"),
            "pipeline worker shutdown must drain with recv().await, not try_recv()",
        );
        assert!(
            !body.contains("event_rx.try_recv()"),
            "pipeline worker shutdown must not use try_recv() — permit-holder race",
        );
    }
}

//! Daemon runtime: wires inputs, pipelines, output queues, and outputs
//! into a running system.
//!
//! Runtime does NOT count metrics — each component counts its own.
//! Runtime distributes one shared metrics registry to every component.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::control::ControlServer;
use crate::dsl::ast::*;
use crate::dsl::props;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::{LtpMetrics, PipelineMetrics, Registry};
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

fn configured_ltp_peer_ids(config: &CompiledConfig) -> Result<BTreeSet<String>> {
    let mut peers = BTreeSet::new();
    for (kind, name, properties) in config
        .inputs
        .iter()
        .map(|(name, def)| ("input", name, &def.properties))
        .chain(
            config
                .outputs
                .iter()
                .map(|(name, def)| ("output", name, &def.properties)),
        )
        .filter(|(_, _, properties)| properties.type_name() == "ltp")
    {
        for peer in properties
            .user_properties()
            .iter()
            .filter_map(|property| match property {
                Property::Block {
                    key, properties, ..
                } if key == "peer" => Some(properties.as_slice()),
                _ => None,
            })
        {
            let node_id = props::get_string(peer, "node_id").ok_or_else(|| {
                anyhow::anyhow!("{kind} '{name}': peer node_id requires a string value")
            })?;
            peers.insert(node_id);
        }
    }
    Ok(peers)
}

impl Runtime {
    pub async fn start(config: CompiledConfig, config_file: PathBuf) -> Result<Self> {
        Self::start_with_registry(config, config_file, Arc::new(Registry::new())).await
    }

    pub(crate) async fn start_with_registry(
        config: CompiledConfig,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
    ) -> Result<Self> {
        Self::start_with_registry_and_node_id_resolver(
            config,
            config_file,
            metrics_registry,
            || Ok(gethostname::gethostname().to_string_lossy().into_owned()),
        )
        .await
    }

    pub(crate) async fn start_with_registry_and_node_id_resolver<F>(
        config: CompiledConfig,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
        resolve_hostname: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<String>,
    {
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
        let ltp_peer_ids = configured_ltp_peer_ids(&config)?;
        let ltp_metrics = if ltp_peer_ids.is_empty() {
            None
        } else {
            Some(LtpMetrics::register(&metrics_registry, &ltp_peer_ids)?)
        };
        let ltp_node_key = config
            .node_key
            .as_deref()
            .map(Path::new)
            .map(crate::ltp::load_node_key)
            .transpose()?
            .map(Arc::new);
        let node_id = match &config.node_id {
            Some(node_id) => node_id.clone(),
            None => resolve_hostname()?,
        };
        crate::metrics::register_build_info(
            &metrics_registry,
            env!("CARGO_PKG_VERSION"),
            &node_id,
        )?;
        let registry = Arc::new(registry);

        let tap = TapRegistry::new();

        // Optional dead-letter queue for events that fail in `process`
        // or that an output drops after exhausting retries
        // (retry-exhausted recovery). `control { error_log "..." }`
        // opts in to file-based recovery; when unset, every emission
        // site delegates to `emit_dlq_tracing_fallback`, which
        // enforces the operator's `error_log_fallback` ladder —
        // payload-free summary by default (`Off`), structured
        // metadata on `Meta`, or full JSONL via `event_record`
        // on `Full`. Pipeline-side and sink-side paths share the
        // same helper, so the ladder shape is identical across
        // both surfaces. The path is validated at startup (parent
        // dir reachable) so operator typos surface before the
        // first failure event.
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

        // Fallback policy for the tracing line when `error_log` write
        // fails or is unset. Parsed here so invalid values fail
        // daemon startup (matching `--check`'s config-time refusal).
        let error_log_fallback = match config
            .global_blocks
            .get("control")
            .and_then(|p| props::get_string(p, "error_log_fallback"))
        {
            Some(s) => crate::error_log::ErrorLogFallback::parse(&s)
                .map_err(|e| anyhow::anyhow!("{}", e))?,
            None => crate::error_log::ErrorLogFallback::default(),
        };
        if error_log.is_none()
            && error_log_fallback != crate::error_log::ErrorLogFallback::default()
        {
            warn!(
                "control.error_log_fallback = \"{}\" is set but control.error_log is unset — \
                 tracing fallback stays payload-free because no durable DLQ was requested; \
                 either set control.error_log to opt into the fallback, or remove \
                 control.error_log_fallback to silence this warning",
                error_log_fallback.as_str(),
            );
        }

        // Single bundle threaded into every Input/Output factory. Future
        // build-time dependencies (transport-key registry, metrics hooks)
        // land as new fields on this struct rather than as new parameters.
        let build_ctx = crate::modules::BuildContext {
            funcs: Arc::clone(&func_registry),
            metrics: Arc::clone(&metrics_registry),
            error_log: error_log.as_ref().map(Arc::clone),
            error_log_fallback,
            shutdown_signal: shutdown_rx.clone(),
            ltp_node_id: Some(Arc::<str>::from(node_id.clone())),
            ltp_node_key,
            ltp_metrics,
        };

        // --- 1. Create outputs (each output owns its own OutputMetrics) ---
        let mut output_senders: HashMap<String, QueueSender> = HashMap::new();
        let mut output_receivers = Vec::new();
        // Populated in the loop below alongside the queue creation so
        // the same `QueueConfig` decides which set of outputs need a
        // workspace-carrying snapshot at `output` statement time. See
        // `PipelineContext::disk_outputs` for the runtime contract.
        let mut disk_outputs: HashSet<String> = HashSet::new();

        for (name, output_def) in &config.outputs {
            let queue_config =
                QueueConfig::from_output_properties(name, output_def.properties.user_properties())?;
            if matches!(queue_config.queue_type, queue::QueueType::Disk { .. }) {
                disk_outputs.insert(name.clone());
            }
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

            let output_metrics = Arc::clone(&created.metrics);
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
        let disk_outputs = Arc::new(disk_outputs);

        // --- 2. Group pipelines by input ---
        //
        // A pipeline with `input a, b;` (fan-in) is registered under every listed
        // input. Events from each input are still fed into the pipeline's
        // per-input worker dispatcher; since a single `PipelineWorker` instance
        // is shared across inputs (wrapped in Arc at spawn time), its metrics
        // aggregate across inputs without per-input attribution — by design.
        let mut input_pipelines: HashMap<String, Vec<Arc<PipelineWorker>>> = HashMap::new();

        for pipeline_def in config.pipelines.values() {
            let worker = Arc::new(PipelineWorker::new_with_process_metrics(
                pipeline_def.clone(),
                &config.processes,
                &metrics_registry,
            )?);
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
                disk_outputs: Arc::clone(&disk_outputs),
                config: Arc::clone(&config),
                funcs: Arc::clone(&func_registry),
                tap: tap.clone(),
                error_log: error_log.as_ref().map(Arc::clone),
                error_log_fallback,
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
            handles.push(created.handle);
        }

        // --- 4. Start control socket (after all metrics are registered) ---
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
    /// Names of outputs whose queue backend is disk-based. Precomputed
    /// at startup from each output's parsed `QueueConfig` and handed
    /// to `run_pipeline` per event via
    /// [`crate::pipeline::OutputCapturePolicy::DiskOnly`], which uses
    /// it to decide per-output whether to keep the workspace on the
    /// snapshot pushed to the queue. Disk-backed queues need the
    /// workspace because the WAL persists the full `Event` JSON and
    /// replay rehydrates it; memory queues drop it because no
    /// downstream reader (sink, DLQ record, output tap) touches
    /// workspace on that path.
    disk_outputs: Arc<HashSet<String>>,
    config: Arc<CompiledConfig>,
    funcs: Arc<FunctionRegistry>,
    tap: TapRegistry,
    /// Dead-letter queue writer used for: (1) `process` runtime
    /// errors, (2) output retry-exhausted payloads, (3)
    /// batched-output shutdown-flush leftovers, (4) runtime-side
    /// enqueue failures (queue closed, disk write error, unknown
    /// output). `None` when `control { error_log }` is unset — on
    /// that path every emission site delegates to
    /// [`crate::modules::emit_dlq_tracing_fallback`], which enforces
    /// the operator's `error_log_fallback` ladder policy: payload-
    /// free summary by default (`Off`), structured metadata on
    /// opt-in (`Meta`), or the pre-ladder full-JSONL shape on
    /// explicit `Full` opt-in. See the ladder documentation in
    /// `docs/src/operations/error-log.md` for the confidentiality
    /// rationale.
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    /// Operator-selected confidentiality policy for the tracing-side
    /// fallback line. Threaded through `write_errored_to_dlq` so the
    /// pipeline-side runtime error surface obeys the same ladder as
    /// the sink-side DLQ paths.
    error_log_fallback: crate::error_log::ErrorLogFallback,
}

// ---------------------------------------------------------------------------
// Pipeline worker — owns its own metrics via HasMetrics
// ---------------------------------------------------------------------------

struct PipelineWorker {
    def: PipelineDef,
    metrics: Arc<PipelineMetrics>,
    process_metrics: Option<Arc<crate::pipeline::PipelineProcessMetrics>>,
}

impl PipelineWorker {
    #[cfg(test)]
    fn new(def: PipelineDef, registry: &Registry) -> Result<Self, crate::metrics::MetricsError> {
        let metrics = PipelineMetrics::register(registry, &def.name)?;
        Ok(Self {
            def,
            metrics,
            process_metrics: None,
        })
    }

    fn new_with_process_metrics(
        def: PipelineDef,
        processes: &HashMap<String, crate::dsl::ast::ProcessDef>,
        registry: &Registry,
    ) -> anyhow::Result<Self> {
        let metrics = PipelineMetrics::register(registry, &def.name)?;
        let process_metrics = Arc::new(crate::pipeline::PipelineProcessMetrics::register(
            &def, processes, registry,
        )?);
        Ok(Self {
            def,
            metrics,
            process_metrics: Some(process_metrics),
        })
    }

    #[cfg(test)]
    fn compile_process_metric_plan_for_testing(
        def: &PipelineDef,
        processes: &HashMap<String, crate::dsl::ast::ProcessDef>,
        registry: &Registry,
    ) -> Result<CompiledProcessMetricPlan, crate::metrics::MetricsError> {
        Ok(CompiledProcessMetricPlan {
            pipeline_metrics: PipelineMetrics::register(registry, &def.name)?,
            process_metrics: crate::pipeline::PipelineProcessMetrics::compile_raw(
                def, processes, registry,
            )?,
        })
    }

    #[cfg(test)]
    fn new_with_compiled_process_metrics_for_testing(
        def: PipelineDef,
        processes: &HashMap<String, crate::dsl::ast::ProcessDef>,
        plan: CompiledProcessMetricPlan,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            process_metrics: Some(Arc::new(plan.process_metrics.validate(&def, processes)?)),
            def,
            metrics: plan.pipeline_metrics,
        })
    }
}

#[cfg(test)]
struct CompiledProcessMetricPlan {
    pipeline_metrics: Arc<PipelineMetrics>,
    process_metrics: crate::pipeline::RawPipelineProcessMetrics,
}

#[cfg(test)]
impl CompiledProcessMetricPlan {
    fn process_metrics_mut_for_testing(
        &mut self,
    ) -> &mut crate::pipeline::RawPipelineProcessMetrics {
        &mut self.process_metrics
    }

    fn root_token_for_testing(&self, step: usize) -> Option<usize> {
        self.process_metrics.root_token_for_testing(step)
    }

    fn child_token_for_testing(&self, parent: usize, ordinal: usize) -> Option<usize> {
        self.process_metrics
            .child_token_for_testing(parent, ordinal)
    }

    fn metric_node_selection_trap_for_testing(&self) -> crate::pipeline::MetricNodeSelectionTrap {
        self.process_metrics
            .metric_node_selection_trap_for_testing()
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

#[cfg(test)]
async fn run_pipeline_with_outputs(
    pipeline: &PipelineDef,
    event: &Event,
    ctx: &PipelineContext,
    bump: &mut bumpalo::Bump,
) -> Result<crate::pipeline::PipelineRunResult> {
    run_pipeline_with_outputs_inner(pipeline, None, event, ctx, bump).await
}

async fn run_pipeline_with_outputs_inner(
    pipeline: &PipelineDef,
    process_metrics: Option<&crate::pipeline::PipelineProcessMetrics>,
    event: &Event,
    ctx: &PipelineContext,
    bump: &mut bumpalo::Bump,
) -> Result<crate::pipeline::PipelineRunResult> {
    // No `--test-pipeline` trace collector on the daemon hot path —
    // passing `None` skips every trace push (and the `format!` /
    // `to_string` work behind it) in `run_pipeline`, since nothing
    // here reads `PipelineRunResult::trace`.
    let mut result = match process_metrics {
        Some(process_metrics) => crate::pipeline::run_pipeline_with_process_metrics(
            pipeline,
            event,
            &ctx.config,
            &ctx.funcs,
            Some(&ctx.tap),
            None,
            crate::pipeline::OutputCapturePolicy::DiskOnly(&ctx.disk_outputs),
            bump,
            process_metrics,
        )?,
        None => crate::pipeline::run_pipeline(
            pipeline,
            event,
            &ctx.config,
            &ctx.funcs,
            Some(&ctx.tap),
            None,
            crate::pipeline::OutputCapturePolicy::DiskOnly(&ctx.disk_outputs),
            bump,
        )?,
    };

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
    // Each failed enqueue carries a per-output snapshot of just the
    // fields the DLQ record actually stores (`OutputEvent`:
    // `key`, `source`, `received_at`, `ingress`, `egress` — the same shape
    // `inject output` replay needs). Snapshotting `OutputEvent` here
    // rather than the whole `OwnedEvent` avoids the workspace
    // `HashMap<String, OwnedValue>` deep clone on every success:
    // pre-fix this ran unconditionally before `sender.send(event)`
    // because the send consumed `event`, and populated workspaces
    // dominated the runtime hot path (see the `634cbd0` perf
    // regression thread). The input event the function received is
    // not equivalent to a per-output snapshot: the pipeline may have
    // produced a different `egress` per output, and replay must
    // preserve the bytes the sink would have shipped — which is why
    // the snapshot is per-`(output_name, event)`, not one shared
    // capture at function entry.
    let mut failed_outputs: Vec<(String, crate::pipeline::OutputEvent)> = Vec::new();
    for (output_name, event) in outputs {
        if let Some(sender) = ctx.output_senders.get(&output_name) {
            // Snapshot the DLQ-relevant fields before the send
            // consumes `event`. Cheap: five Copy scalars + two
            // `Bytes` refcount bumps; workspace is not touched.
            let snapshot = crate::pipeline::OutputEvent::from_owned(&event);
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
            failed_outputs.push((
                output_name,
                crate::pipeline::OutputEvent::from_owned(&event),
            ));
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
                    event: snapshot,
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
        worker.metrics.events_received.inc();

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
        worker.metrics.inflight.inc();
        match run_pipeline_with_outputs_inner(
            &worker.def,
            worker.process_metrics.as_deref(),
            event,
            ctx,
            bump,
        )
        .await
        {
            Ok(result) => {
                use crate::pipeline::PipelineTermination;
                match result.termination {
                    PipelineTermination::Dropped => {
                        worker.metrics.events_dropped.inc();
                    }
                    PipelineTermination::Errored => {
                        worker.metrics.events_errored.inc();
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
                                    ctx.error_log_fallback,
                                )
                                .await;
                            }
                        }
                    }
                    PipelineTermination::Finished => {
                        if !result.had_outputs {
                            worker.metrics.events_discarded.inc();
                        } else {
                            worker.metrics.events_finished.inc();
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
                worker.metrics.events_errored.inc();
                let owned = event.to_owned();
                let err_ctx = crate::pipeline::ErroredEventContext::Process {
                    timestamp: chrono::Utc::now(),
                    pipeline: worker.def.name.clone(),
                    site: "(pipeline body)".to_string(),
                    reason: e.to_string(),
                    event: crate::pipeline::ProcessEvent::from_owned(&owned),
                };
                write_errored_to_dlq(
                    &err_ctx,
                    &worker.metrics,
                    ctx.error_log.as_ref(),
                    ctx.error_log_fallback,
                )
                .await;
            }
        }
        worker.metrics.inflight.dec();
    }
}

/// Persist an errored event to the dead-letter queue, or — if no
/// `error_log` is configured — delegate to the `error_log_fallback`
/// ladder helper so the failure surfaces as a payload-free operator
/// signal by default (or as `Meta` / `Full` on explicit opt-in). The
/// tracing line is best-effort, not a durable recovery trail; the
/// DLQ file remains the load-bearing recovery target.
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
    error_log_fallback: crate::error_log::ErrorLogFallback,
) {
    match error_log {
        Some(writer) => {
            if let Err(e) = writer.write(err_ctx).await {
                worker_metrics.events_errored_unwritable.inc();
                crate::modules::emit_dlq_tracing_fallback(
                    /* error_log_configured */ true,
                    error_log_fallback,
                    err_ctx,
                    None,
                    Some(&e),
                );
            }
        }
        None => {
            // No DLQ configured — payload-free tracing per ladder
            // row-A. Operator declared no durable recovery is
            // required; the summary line is all that surfaces.
            crate::modules::emit_dlq_tracing_fallback(
                /* error_log_configured */ false,
                error_log_fallback,
                err_ctx,
                None,
                None,
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
    use crate::metrics::{MetricsError, OutputMetrics, Registry};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn assert_output_duplicate(error: &MetricsError, label_value: &str, diagnostic: &str) {
        let (name, labelset) = match error {
            MetricsError::DuplicateSeries { name, labelset } => (name, labelset),
            other => panic!("expected DuplicateSeries, got {other:?}"),
        };
        assert!(
            [
                "limpid_output_events_received_total",
                "limpid_output_events_injected_total",
                "limpid_output_events_written_total",
                "limpid_output_events_failed_total",
                "limpid_output_retries_total",
                "limpid_output_events_wedged_total",
                "limpid_output_events_errored_unwritable_total",
            ]
            .contains(&name.as_str())
        );
        assert_eq!(labelset, &[("output".to_owned(), label_value.to_owned())]);
        assert!(diagnostic.contains(&format!("name={name:?}")));
        assert!(diagnostic.contains(&format!("labelset={labelset:?}")));
    }

    fn pipeline_def(src: &str) -> PipelineDef {
        let cfg = parse_config(src).unwrap();
        for def in cfg.definitions {
            if let Definition::Pipeline(p) = def {
                return p;
            }
        }
        panic!("no pipeline in src");
    }

    fn compiled_config(src: &str) -> CompiledConfig {
        CompiledConfig::from_config(parse_config(src).expect("parse config"))
            .expect("compile config")
    }

    #[cfg(unix)]
    fn write_ltp_test_identity(path: &Path) -> String {
        use base64::Engine as _;
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair as _};
        use std::os::unix::fs::PermissionsExt as _;

        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        std::fs::write(
            path,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        base64::engine::general_purpose::STANDARD.encode(spki)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_runtimes_deliver_one_event_over_mutual_rpk_ltp() {
        use std::os::unix::fs::PermissionsExt as _;
        use tokio::io::AsyncWriteExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let node_a_key = dir.path().join("node-a.pem");
        let node_b_key = dir.path().join("node-b.pem");
        let node_a_spki = write_ltp_test_identity(&node_a_key);
        let node_b_spki = write_ltp_test_identity(&node_b_key);
        let delivered_path = dir.path().join("delivered.log");
        let node_a_socket = dir.path().join("node-a.sock");
        let node_b_socket = dir.path().join("node-b.sock");

        let ltp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ltp_addr = ltp_listener.local_addr().unwrap();
        drop(ltp_listener);
        let node_b_config = compiled_config(&format!(
            r#"
node_id "node-b"
node_key {node_b_key:?}
control {{ socket {node_b_socket:?} }}
def input from_a {{
    type ltp
    bind "{ltp_addr}"
    peer {{ node_id "node-a" pubkey {node_a_spki:?} }}
}}
def output delivered {{ type file path {delivered_path:?} }}
def pipeline receive {{ input from_a; output delivered }}
"#
        ));
        let node_b = Runtime::start(node_b_config, dir.path().join("node-b.limpid"))
            .await
            .unwrap();

        let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();
        drop(tcp_listener);
        let node_a_config = compiled_config(&format!(
            r#"
node_id "node-a"
node_key {node_a_key:?}
control {{ socket {node_a_socket:?} }}
def input source {{ type syslog_tcp bind "{tcp_addr}" }}
def output to_b {{
    type ltp
    peer {{ node_id "node-b" pubkey {node_b_spki:?} endpoint "{ltp_addr}" }}
}}
def pipeline relay {{ input source; output to_b }}
"#
        ));
        let node_a = Runtime::start(node_a_config, dir.path().join("node-a.limpid"))
            .await
            .unwrap();

        let marker = b"<13>two-runtime-mutual-rpk";
        let mut frame = marker.to_vec();
        frame.push(b'\n');
        let mut sender = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tokio::net::TcpStream::connect(tcp_addr).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                }
            }
        })
        .await
        .expect("syslog TCP input did not become ready");
        sender.write_all(&frame).await.unwrap();
        sender.flush().await.unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(bytes) = tokio::fs::read(&delivered_path).await
                    && bytes == frame
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;

        node_a.shutdown().await;
        node_b.shutdown().await;
        delivered.expect("two-daemon LTP delivery timed out");
        assert_eq!(tokio::fs::read(&delivered_path).await.unwrap(), frame);
    }

    #[test]
    fn ltp_peer_metric_registration_uses_the_deduplicated_static_config_union() {
        use base64::Engine as _;

        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&[7; 32]);
        let pubkey = base64::engine::general_purpose::STANDARD.encode(spki);
        let config = compiled_config(&format!(
            "def input ltp_in {{ type ltp peer {{ node_id \"peer-input\" pubkey {pubkey:?} }} }}\n\
             def output ltp_out_a {{ type ltp peer {{ node_id \"peer-a\" pubkey {pubkey:?} endpoint \"127.0.0.1:7514\" }} }}\n\
             def output ltp_out_b {{ type ltp peer {{ node_id \"peer-b\" pubkey {pubkey:?} endpoint \"127.0.0.1:7515\" }} }}"
        ));

        assert_eq!(
            configured_ltp_peer_ids(&config).unwrap(),
            BTreeSet::from([
                "peer-a".to_owned(),
                "peer-b".to_owned(),
                "peer-input".to_owned(),
            ])
        );
    }

    fn metric_series(registry: &Registry, name: &str) -> Vec<serde_json::Value> {
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

    fn series_value(registry: &Registry, family: &str, labels: &[(&str, &str)]) -> u64 {
        let expected: serde_json::Map<String, serde_json::Value> = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::json!(value)))
            .collect();
        metric_series(registry, family)
            .into_iter()
            .find(|series| series["labels"].as_object() == Some(&expected))
            .unwrap_or_else(|| panic!("missing {family} series for {expected:?}"))["value"]
            .as_u64()
            .expect("counter value")
    }

    #[test]
    fn process_metrics_prepopulate_exact_static_dfs_topology() {
        let config = compiled_config(
            r#"
def process leaf { egress = ingress }
def process parent_one { process leaf }
def process parent_two { process leaf }
def process repeated { egress = ingress }
def process dispatch { process leaf; process leaf; drop }
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
    drop
}
"#,
        );
        let registry = Registry::new();
        let def = config.pipelines.get("topology").expect("pipeline").clone();
        let _worker = PipelineWorker::new_with_process_metrics(def, &config.processes, &registry)
            .expect("register pipeline and process metrics");

        let process_families = [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
            "limpid_process_events_errored_total",
        ];
        let expected = [
            ("1", "/parent_one", "parent_one"),
            ("2", "/parent_one/leaf", "leaf"),
            ("3", "/parent_two", "parent_two"),
            ("4", "/parent_two/leaf", "leaf"),
            ("5", "/repeated", "repeated"),
            ("6", "/repeated", "repeated"),
            ("7", "/dispatch", "dispatch"),
            ("8", "/dispatch/leaf", "leaf"),
            ("9", "/branch_then", "branch_then"),
            ("10", "/branch_else", "branch_else"),
            ("11", "/arm_first", "arm_first"),
            ("12", "/arm_default", "arm_default"),
            ("13", "/(inline)", "(inline)"),
        ];
        for family in process_families {
            let series = metric_series(&registry, family);
            assert_eq!(series.len(), expected.len(), "{family}");
            for (step, path, name) in expected {
                assert_eq!(
                    series_value(
                        &registry,
                        family,
                        &[
                            ("pipeline", "topology"),
                            ("step", step),
                            ("process_path", path),
                            ("process_name", name),
                        ],
                    ),
                    0,
                    "{family} must be prepopulated"
                );
            }
        }

        let dropped = metric_series(&registry, "limpid_events_dropped_total");
        assert_eq!(dropped.len(), expected.len() + 1);
        assert_eq!(
            series_value(
                &registry,
                "limpid_events_dropped_total",
                &[
                    ("pipeline", "topology"),
                    ("step", "0"),
                    ("process_path", "/"),
                    ("process_name", ""),
                ],
            ),
            0
        );
        for (step, path, name) in expected {
            assert_eq!(
                series_value(
                    &registry,
                    "limpid_events_dropped_total",
                    &[
                        ("pipeline", "topology"),
                        ("step", step),
                        ("process_path", path),
                        ("process_name", name),
                    ],
                ),
                0
            );
        }
    }

    #[test]
    fn process_metrics_reuse_same_callee_calls_within_one_parent() {
        let config = compiled_config(
            r#"
def process leaf { egress = ingress }
def process dispatch { process leaf; process leaf }
def pipeline bounded { process dispatch }
"#,
        );
        let registry = Registry::new();
        let def = config.pipelines.get("bounded").expect("pipeline").clone();
        let _worker = PipelineWorker::new_with_process_metrics(def, &config.processes, &registry)
            .expect("register bounded process topology");
        let series = metric_series(&registry, "limpid_process_events_in_total");
        let paths: Vec<&str> = series
            .iter()
            .map(|series| series["labels"]["process_path"].as_str().expect("path"))
            .collect();
        assert_eq!(series.len(), 2);
        assert_eq!(paths.iter().filter(|path| **path == "/dispatch").count(), 1);
        assert_eq!(
            paths
                .iter()
                .filter(|path| **path == "/dispatch/leaf")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn process_metric_call_sites_compile_to_opaque_tokens_consumed_by_execution() {
        let config = compiled_config(
            r#"
def process leaf { egress = ingress }
def process parent_one { process leaf }
def process parent_two { process leaf }
def process dispatch { process leaf; process leaf }
def pipeline p {
    process parent_one
    process parent_two
    process dispatch
}
"#,
        );
        let registry = Registry::new();
        let def = config.pipelines.get("p").expect("pipeline").clone();
        let plan = PipelineWorker::compile_process_metric_plan_for_testing(
            &def,
            &config.processes,
            &registry,
        )
        .expect("compile process metric plan");

        let parent_one = plan.root_token_for_testing(1).expect("parent_one token");
        let parent_two = plan.root_token_for_testing(3).expect("parent_two token");
        let dispatch = plan.root_token_for_testing(5).expect("dispatch token");
        let parent_one_leaf = plan
            .child_token_for_testing(parent_one, 0)
            .expect("parent_one leaf token");
        let parent_two_leaf = plan
            .child_token_for_testing(parent_two, 0)
            .expect("parent_two leaf token");
        assert_ne!(
            parent_one_leaf, parent_two_leaf,
            "different parents must have different pre-resolved child frames"
        );
        assert_eq!(
            plan.child_token_for_testing(dispatch, 0),
            plan.child_token_for_testing(dispatch, 1),
            "same-parent calls to the same callee must reuse one frame token"
        );
        let execution_config = compiled_config(
            r#"
def process leaf { egress = ingress }
def process dispatch { process leaf; process leaf }
def pipeline execute { process dispatch; finish }
"#,
        );
        let execution_registry = Registry::new();
        let execution_def = execution_config
            .pipelines
            .get("execute")
            .expect("execution pipeline")
            .clone();
        let execution_plan = PipelineWorker::compile_process_metric_plan_for_testing(
            &execution_def,
            &execution_config.processes,
            &execution_registry,
        )
        .expect("compile executable process metric plan");
        // This probe is owned by the metric-node selection seam used by execution,
        // rather than by the test facade. Arming it after compilation measures the
        // exact token-selection path without counting definition-registry lookups.
        let selection_trap = execution_plan.metric_node_selection_trap_for_testing();
        let worker = Arc::new(
            PipelineWorker::new_with_compiled_process_metrics_for_testing(
                execution_def,
                &execution_config.processes,
                execution_plan,
            )
            .expect("execution must consume the compiled token plan"),
        );
        selection_trap.arm_for_testing();
        let ctx = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(execution_config),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        process_event(
            &fixture_event(),
            &[worker],
            &ctx,
            "input compiled-plan",
            &mut bumpalo::Bump::new(),
        )
        .await;
        assert_eq!(
            selection_trap.total_token_selections_for_testing(),
            3,
            "dispatch once plus leaf twice must perform three token selections"
        );
        assert_eq!(
            selection_trap.invalid_token_selections_for_testing(),
            0,
            "compiled token selection must not access an invalid node"
        );
        assert_process_vector(
            &execution_registry,
            &[
                ("pipeline", "execute"),
                ("step", "1"),
                ("process_path", "/dispatch"),
                ("process_name", "dispatch"),
            ],
            [1, 1, 0, 0],
        );
        assert_process_vector(
            &execution_registry,
            &[
                ("pipeline", "execute"),
                ("step", "2"),
                ("process_path", "/dispatch/leaf"),
                ("process_name", "leaf"),
            ],
            [2, 2, 0, 0],
        );
        assert_process_conservation(&execution_registry);
    }

    #[test]
    fn process_metric_selection_probe_separates_total_and_invalid_tokens() {
        let config = compiled_config(
            "def process pass { egress = ingress } def pipeline p { process pass }",
        );
        let registry = Registry::new();
        let def = config.pipelines.get("p").expect("pipeline").clone();
        let plan = PipelineWorker::compile_process_metric_plan_for_testing(
            &def,
            &config.processes,
            &registry,
        )
        .expect("compile process metric plan");
        let probe = plan.metric_node_selection_trap_for_testing();
        let worker = PipelineWorker::new_with_compiled_process_metrics_for_testing(
            def,
            &config.processes,
            plan,
        )
        .expect("construct worker");
        probe.arm_for_testing();

        assert!(
            !worker
                .process_metrics
                .as_ref()
                .expect("process metrics")
                .select_node_for_testing(usize::MAX),
            "out-of-range token must not resolve"
        );
        assert_eq!(probe.total_token_selections_for_testing(), 1);
        assert_eq!(probe.invalid_token_selections_for_testing(), 1);
    }

    async fn run_compiled_process_metric_fixture(
        source: &str,
        pipeline: &str,
        mutate: impl FnOnce(&mut crate::pipeline::RawPipelineProcessMetrics),
    ) -> (Registry, crate::pipeline::MetricNodeSelectionTrap) {
        let config = compiled_config(source);
        let registry = Registry::new();
        let def = config
            .pipelines
            .get(pipeline)
            .unwrap_or_else(|| panic!("missing pipeline {pipeline}"))
            .clone();
        let mut plan = PipelineWorker::compile_process_metric_plan_for_testing(
            &def,
            &config.processes,
            &registry,
        )
        .expect("compile process metric plan");
        mutate(plan.process_metrics_mut_for_testing());
        let selection_trap = plan.metric_node_selection_trap_for_testing();
        let worker = Arc::new(
            PipelineWorker::new_with_compiled_process_metrics_for_testing(
                def,
                &config.processes,
                plan,
            )
            .expect("construct worker from compiled metric plan"),
        );
        selection_trap.arm_for_testing();
        let ctx = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(config),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        process_event(
            &fixture_event(),
            &[worker],
            &ctx,
            "input compiled-plan",
            &mut bumpalo::Bump::new(),
        )
        .await;
        (registry, selection_trap)
    }

    fn assert_compiled_plan_rejected_before_execution(
        source: &str,
        pipeline: &str,
        mutate: impl FnOnce(&mut crate::pipeline::RawPipelineProcessMetrics),
    ) {
        let config = compiled_config(source);
        let registry = Registry::new();
        let def = config
            .pipelines
            .get(pipeline)
            .unwrap_or_else(|| panic!("missing pipeline {pipeline}"))
            .clone();
        let mut plan = PipelineWorker::compile_process_metric_plan_for_testing(
            &def,
            &config.processes,
            &registry,
        )
        .expect("compile raw process metric plan");
        mutate(plan.process_metrics_mut_for_testing());
        let selection_probe = plan.metric_node_selection_trap_for_testing();
        selection_probe.arm_for_testing();
        assert!(
            PipelineWorker::new_with_compiled_process_metrics_for_testing(
                def,
                &config.processes,
                plan,
            )
            .is_err(),
            "invalid raw plan must be rejected during worker construction"
        );
        assert_eq!(selection_probe.total_token_selections_for_testing(), 0);
        assert_eq!(selection_probe.invalid_token_selections_for_testing(), 0);
        assert_eq!(
            series_value(
                &registry,
                "limpid_pipeline_events_errored_total",
                &[("pipeline", pipeline)],
            ),
            0,
            "startup rejection must happen before an event is executed"
        );
        assert_eq!(
            series_value(
                &registry,
                "limpid_events_dropped_total",
                &[
                    ("pipeline", pipeline),
                    ("step", "0"),
                    ("process_path", "/"),
                    ("process_name", ""),
                ],
            ),
            0,
            "the process body's drop side effect must not run during validation"
        );
        for series in metric_series(&registry, "limpid_process_events_in_total") {
            assert_eq!(series["value"], 0, "no process frame may start at startup");
        }
        assert_process_conservation(&registry);
    }

    #[test]
    fn compiled_metric_plan_mismatches_fail_closed_during_worker_construction() {
        const NAMED: &str = r#"
def process leaf { drop }
def process root { process leaf }
def pipeline p { process root; finish }
"#;
        assert_compiled_plan_rejected_before_execution(NAMED, "p", |metrics| {
            metrics.remove_root_plan_for_testing()
        });
        assert_compiled_plan_rejected_before_execution(NAMED, "p", |metrics| {
            metrics.replace_first_root_plan_with_none_for_testing();
        });
        assert_compiled_plan_rejected_before_execution(NAMED, "p", |metrics| {
            metrics.invalidate_first_root_token_for_testing();
        });
        assert_compiled_plan_rejected_before_execution(NAMED, "p", |metrics| {
            metrics.replace_first_process_body_plan_with_none_for_testing();
        });
        assert_compiled_plan_rejected_before_execution(NAMED, "p", |metrics| {
            metrics.invalidate_first_nested_token_for_testing();
        });
        assert_compiled_plan_rejected_before_execution(
            "def pipeline p { process { drop }; finish }",
            "p",
            |metrics| metrics.invalidate_first_inline_token_for_testing(),
        );

        assert_compiled_plan_rejected_before_execution(
            "def process a { drop } def process b { drop } def pipeline p { process a | b }",
            "p",
            |metrics| metrics.swap_first_two_root_tokens_for_testing(),
        );
        assert_compiled_plan_rejected_before_execution(
            r#"
def process leaf { drop }
def process parent_one { process leaf }
def process parent_two { process leaf }
def pipeline p { process parent_one; process parent_two }
"#,
            "p",
            |metrics| metrics.swap_first_two_nested_tokens_for_testing(),
        );
        assert_compiled_plan_rejected_before_execution(
            "def process named { drop } def pipeline p { process named | { drop } }",
            "p",
            |metrics| metrics.swap_first_two_root_tokens_for_testing(),
        );
    }

    async fn assert_compiled_branch_selection(
        source: &str,
        pipeline: &str,
        expected: &[(&str, &str, &str, [u64; 4])],
    ) {
        let (registry, trap) = run_compiled_process_metric_fixture(source, pipeline, |_| {}).await;
        let expected_selections = expected
            .iter()
            .map(|(_, _, _, vector)| vector[0] as usize)
            .sum::<usize>();
        assert_eq!(
            trap.total_token_selections_for_testing(),
            expected_selections,
            "each invoked process frame must consume one compiled token"
        );
        assert_eq!(
            trap.invalid_token_selections_for_testing(),
            0,
            "compiled branch selection must not access an invalid node"
        );
        for (step, path, name, vector) in expected {
            assert_process_vector(
                &registry,
                &[
                    ("pipeline", pipeline),
                    ("step", step),
                    ("process_path", path),
                    ("process_name", name),
                ],
                *vector,
            );
        }
        assert_process_conservation(&registry);
    }

    #[tokio::test]
    async fn compiled_process_branches_use_selected_ordinals_for_nonfirst_and_fallback_bodies() {
        const SOURCE: &str = r#"
def process first { egress = ingress }
def process selected { egress = ingress }
def process fallback { egress = ingress }
def process nonfirst {
    if false { process first } else if true { process selected } else { process fallback }
    switch "second" {
        "first" { process first }
        "second" { process selected }
        default { process fallback }
    }
}
def process defaults {
    if false { process first } else if false { process selected } else { process fallback }
    switch "missing" {
        "first" { process first }
        "second" { process selected }
        default { process fallback }
    }
}
def pipeline p_nonfirst { process nonfirst; finish }
def pipeline p_fallback { process defaults; finish }
"#;
        assert_compiled_branch_selection(
            SOURCE,
            "p_nonfirst",
            &[
                ("1", "/nonfirst", "nonfirst", [1, 1, 0, 0]),
                ("2", "/nonfirst/first", "first", [0, 0, 0, 0]),
                ("3", "/nonfirst/selected", "selected", [2, 2, 0, 0]),
                ("4", "/nonfirst/fallback", "fallback", [0, 0, 0, 0]),
            ],
        )
        .await;
        assert_compiled_branch_selection(
            SOURCE,
            "p_fallback",
            &[
                ("1", "/defaults", "defaults", [1, 1, 0, 0]),
                ("2", "/defaults/first", "first", [0, 0, 0, 0]),
                ("3", "/defaults/selected", "selected", [0, 0, 0, 0]),
                ("4", "/defaults/fallback", "fallback", [2, 2, 0, 0]),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn compiled_pipeline_branches_use_selected_ordinals_for_nonfirst_and_fallback_bodies() {
        const SOURCE: &str = r#"
def process leaf { egress = ingress }
def process parent { process leaf }
def pipeline p_nonfirst {
    if false { process parent } else if true { process parent } else { process parent }
    switch "second" {
        "first" { process parent }
        "second" { process parent }
        default { process parent }
    }
    finish
}
def pipeline p_fallback {
    if false { process parent } else if false { process parent } else { process parent }
    switch "missing" {
        "first" { process parent }
        "second" { process parent }
        default { process parent }
    }
    finish
}
"#;
        assert_compiled_branch_selection(
            SOURCE,
            "p_nonfirst",
            &[
                ("3", "/parent", "parent", [1, 1, 0, 0]),
                ("4", "/parent/leaf", "leaf", [1, 1, 0, 0]),
                ("9", "/parent", "parent", [1, 1, 0, 0]),
                ("10", "/parent/leaf", "leaf", [1, 1, 0, 0]),
            ],
        )
        .await;
        assert_compiled_branch_selection(
            SOURCE,
            "p_fallback",
            &[
                ("5", "/parent", "parent", [1, 1, 0, 0]),
                ("6", "/parent/leaf", "leaf", [1, 1, 0, 0]),
                ("11", "/parent", "parent", [1, 1, 0, 0]),
                ("12", "/parent/leaf", "leaf", [1, 1, 0, 0]),
            ],
        )
        .await;
    }

    #[test]
    fn process_metric_families_have_exact_counter_metadata_and_label_dimensions() {
        let config = compiled_config(
            "def process pass { egress = ingress } def pipeline p { process pass }",
        );
        let registry = Registry::new();
        let def = config.pipelines.get("p").expect("pipeline").clone();
        let _worker = PipelineWorker::new_with_process_metrics(def, &config.processes, &registry)
            .expect("register metrics");
        let snapshot = serde_json::to_value(registry.snapshot()).expect("snapshot");
        for name in [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
            "limpid_events_dropped_total",
            "limpid_process_events_errored_total",
        ] {
            let family = snapshot["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|family| family["name"] == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(family["type"], "counter");
            assert!(family["help"].as_str().is_some_and(|help| !help.is_empty()));
            let labels = family["series"][0]["labels"].as_object().unwrap();
            assert_eq!(
                labels.keys().map(String::as_str).collect::<Vec<_>>(),
                ["pipeline", "process_name", "process_path", "step"]
            );
        }
    }

    async fn run_process_metric_fixture(src: &str, pipeline: &str, mut event: Event) -> Registry {
        let config = compiled_config(src);
        let registry = Registry::new();
        let def = config
            .pipelines
            .get(pipeline)
            .unwrap_or_else(|| panic!("missing pipeline {pipeline}"))
            .clone();
        let worker = Arc::new(
            PipelineWorker::new_with_process_metrics(def, &config.processes, &registry)
                .expect("register process metrics"),
        );
        let ctx = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(config),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        if event.egress.is_empty() {
            event.egress = event.ingress.clone();
        }
        process_event(
            &event,
            &[worker],
            &ctx,
            "input fixture",
            &mut bumpalo::Bump::new(),
        )
        .await;
        registry
    }

    fn fixture_event() -> Event {
        Event::new(
            Bytes::from_static(b"payload"),
            SocketAddr::from_str("127.0.0.1:0").unwrap(),
        )
    }

    fn assert_process_conservation(registry: &Registry) {
        let family_names = [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
            "limpid_events_dropped_total",
            "limpid_process_events_errored_total",
        ];
        let label_sets: Vec<std::collections::BTreeSet<Vec<(String, String)>>> = family_names
            .iter()
            .map(|family| {
                metric_series(registry, family)
                    .into_iter()
                    .filter(|series| series["labels"]["process_path"] != "/")
                    .map(|series| {
                        series["labels"]
                            .as_object()
                            .expect("labels")
                            .iter()
                            .map(|(key, value)| {
                                (key.clone(), value.as_str().expect("label value").to_owned())
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        for labels in &label_sets[1..] {
            assert_eq!(
                labels, &label_sets[0],
                "all process terminal families must have the exact input label-set"
            );
        }

        for input in metric_series(registry, "limpid_process_events_in_total") {
            let labels = input["labels"].as_object().expect("labels");
            let owned_labels: Vec<(String, String)> = labels
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().expect("label value").to_owned()))
                .collect();
            let labels: Vec<(&str, &str)> = owned_labels
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let input = input["value"].as_u64().expect("input value");
            let terminal: u64 = ["out", "dropped", "errored"]
                .into_iter()
                .map(|suffix| {
                    let family = if suffix == "dropped" {
                        "limpid_events_dropped_total".to_owned()
                    } else {
                        format!("limpid_process_events_{suffix}_total")
                    };
                    series_value(registry, &family, &labels)
                })
                .sum();
            assert_eq!(input, terminal, "non-conserving process series {labels:?}");
        }
    }

    fn assert_process_vector(registry: &Registry, labels: &[(&str, &str)], expected: [u64; 4]) {
        let actual = ["in", "out", "dropped", "errored"].map(|suffix| {
            let family = if suffix == "dropped" {
                "limpid_events_dropped_total".to_owned()
            } else {
                format!("limpid_process_events_{suffix}_total")
            };
            series_value(registry, &family, labels)
        });
        assert_eq!(actual, expected, "wrong process vector for {labels:?}");
    }

    #[tokio::test]
    async fn process_invocations_conserve_continue_drop_and_caught_error_frames() {
        let continued = run_process_metric_fixture(
            r#"
def process leaf { egress = ingress }
def process dispatch { process leaf }
def pipeline p { process dispatch; finish }
"#,
            "p",
            fixture_event(),
        )
        .await;
        for (step, path) in [("1", "/dispatch"), ("2", "/dispatch/leaf")] {
            let labels = [
                ("pipeline", "p"),
                ("step", step),
                ("process_path", path),
                ("process_name", path.rsplit('/').next().unwrap()),
            ];
            assert_process_vector(&continued, &labels, [1, 1, 0, 0]);
        }
        assert_process_conservation(&continued);

        let dropped = run_process_metric_fixture(
            r#"
def process leaf { drop }
def process dispatch { process leaf }
def pipeline p { process dispatch; finish }
"#,
            "p",
            fixture_event(),
        )
        .await;
        for (step, path, name) in [
            ("1", "/dispatch", "dispatch"),
            ("2", "/dispatch/leaf", "leaf"),
        ] {
            let labels = [
                ("pipeline", "p"),
                ("step", step),
                ("process_path", path),
                ("process_name", name),
            ];
            assert_process_vector(&dropped, &labels, [1, 0, 1, 0]);
        }
        assert_eq!(
            series_value(
                &dropped,
                "limpid_events_dropped_total",
                &[
                    ("pipeline", "p"),
                    ("step", "0"),
                    ("process_path", "/"),
                    ("process_name", ""),
                ],
            ),
            1
        );
        assert_process_conservation(&dropped);

        let caught = run_process_metric_fixture(
            r#"
def process fail { error "expected" }
def process catcher { try { process fail } catch { egress = ingress } }
def pipeline p { process catcher; finish }
"#,
            "p",
            fixture_event(),
        )
        .await;
        assert_process_vector(
            &caught,
            &[
                ("pipeline", "p"),
                ("step", "1"),
                ("process_path", "/catcher"),
                ("process_name", "catcher"),
            ],
            [1, 1, 0, 0],
        );
        assert_process_vector(
            &caught,
            &[
                ("pipeline", "p"),
                ("step", "2"),
                ("process_path", "/catcher/fail"),
                ("process_name", "fail"),
            ],
            [1, 0, 0, 1],
        );
        assert_eq!(
            series_value(
                &caught,
                "limpid_pipeline_events_errored_total",
                &[("pipeline", "p")],
            ),
            0,
            "a caught process error must not become a pipeline error"
        );
        assert_process_conservation(&caught);

        let uncaught = run_process_metric_fixture(
            r#"
def process fail { error "expected" }
def process outer { process fail }
def pipeline p { process outer; finish }
"#,
            "p",
            fixture_event(),
        )
        .await;
        for (step, path, name) in [("1", "/outer", "outer"), ("2", "/outer/fail", "fail")] {
            let labels = [
                ("pipeline", "p"),
                ("step", step),
                ("process_path", path),
                ("process_name", name),
            ];
            assert_process_vector(&uncaught, &labels, [1, 0, 0, 1]);
        }
        assert_eq!(
            series_value(
                &uncaught,
                "limpid_pipeline_events_errored_total",
                &[("pipeline", "p")],
            ),
            1,
            "one errored pipeline run is independent of its two errored frames"
        );
        assert_process_conservation(&uncaught);
    }

    #[tokio::test]
    async fn dropped_events_share_one_rooted_pipeline_and_process_hierarchy() {
        for body in ["drop", "process { drop }", "process named"] {
            let source =
                format!("def process named {{ drop }} def pipeline p {{ {body}; finish }}");
            let registry = run_process_metric_fixture(&source, "p", fixture_event()).await;
            let dropped = metric_series(&registry, "limpid_events_dropped_total");
            assert_eq!(
                series_value(
                    &registry,
                    "limpid_events_dropped_total",
                    &[
                        ("pipeline", "p"),
                        ("step", "0"),
                        ("process_path", "/"),
                        ("process_name", ""),
                    ],
                ),
                1,
                "each dropped event must increment the hierarchy root once: {body}"
            );
            assert_eq!(dropped.len(), if body == "drop" { 1 } else { 2 }, "{body}");
            if body != "drop" {
                assert_process_conservation(&registry);
            }
        }

        let nested = run_process_metric_fixture(
            r#"
def process leaf { drop }
def process outer { process leaf }
def pipeline nested { process outer; finish }
"#,
            "nested",
            fixture_event(),
        )
        .await;
        let dropped = metric_series(&nested, "limpid_events_dropped_total");
        assert_eq!(dropped.len(), 3);
        assert_eq!(
            series_value(
                &nested,
                "limpid_events_dropped_total",
                &[
                    ("pipeline", "nested"),
                    ("step", "0"),
                    ("process_path", "/"),
                    ("process_name", ""),
                ],
            ),
            1
        );
        for (step, path, name) in [("1", "/outer", "outer"), ("2", "/outer/leaf", "leaf")] {
            assert_process_vector(
                &nested,
                &[
                    ("pipeline", "nested"),
                    ("step", step),
                    ("process_path", path),
                    ("process_name", name),
                ],
                [1, 0, 1, 0],
            );
        }
        assert_process_conservation(&nested);
    }

    #[test]
    fn process_metric_compilation_rejects_recursion_if_analysis_is_bypassed() {
        let config = compiled_config(
            r#"
def process a { process b }
def process b { process a }
def pipeline p { process a }
"#,
        );
        let registry = Registry::new();
        let def = config.pipelines.get("p").expect("pipeline");
        let error = match PipelineWorker::compile_process_metric_plan_for_testing(
            def,
            &config.processes,
            &registry,
        ) {
            Ok(_) => panic!("metric compilation must reject a recursive process graph"),
            Err(error) => error,
        };
        match error {
            crate::metrics::MetricsError::ProcessCallCycle { path } => {
                assert_eq!(path, ["a", "b", "a"]);
            }
            other => panic!("unexpected metric compilation error: {other}"),
        };
    }

    #[tokio::test]
    async fn concurrent_tasks_share_prepopulated_process_handles_without_cardinality_growth() {
        let config = compiled_config(
            "def process pass { egress = ingress } def pipeline p { process pass; finish }",
        );
        let registry = Arc::new(Registry::new());
        let def = config.pipelines.get("p").expect("pipeline").clone();
        let worker = Arc::new(
            PipelineWorker::new_with_process_metrics(def, &config.processes, &registry)
                .expect("register metrics"),
        );
        let ctx = Arc::new(PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(config),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        });
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let worker = Arc::clone(&worker);
            let ctx = Arc::clone(&ctx);
            tasks.push(tokio::spawn(async move {
                process_event(
                    &fixture_event(),
                    &[worker],
                    &ctx,
                    "input concurrent",
                    &mut bumpalo::Bump::new(),
                )
                .await;
            }));
        }
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("invocation must finish")
                .expect("invocation task must not panic");
        }
        let labels = [
            ("pipeline", "p"),
            ("step", "1"),
            ("process_path", "/pass"),
            ("process_name", "pass"),
        ];
        assert_process_vector(&registry, &labels, [16, 16, 0, 0]);
        assert_eq!(
            metric_series(&registry, "limpid_process_events_in_total").len(),
            1,
            "shared pre-resolved handles must not grow metric cardinality"
        );
        assert_process_conservation(&registry);
    }

    #[tokio::test]
    async fn startup_preserves_metric_registration_errors_from_real_factories() {
        let config = CompiledConfig::from_config(
            parse_config("def output conflicting { type stdout }").expect("parse"),
        )
        .expect("compile");
        let registry = Arc::new(Registry::new());
        OutputMetrics::register(&registry, "conflicting")
            .expect("preseeded output metrics must register");

        let error = match Runtime::start_with_registry(
            config,
            PathBuf::from("metrics-conflict-test.limpid"),
            registry,
        )
        .await
        {
            Ok(_) => panic!("daemon startup unexpectedly swallowed the registration conflict"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        let metrics_error = error
            .chain()
            .find_map(|source| source.downcast_ref::<MetricsError>())
            .unwrap_or_else(|| {
                panic!(
                    "MetricsError must remain downcastable in the startup error chain: {error:#}"
                )
            });
        assert_output_duplicate(metrics_error, "conflicting", &diagnostic);
    }

    #[tokio::test]
    async fn public_start_delegates_to_the_registry_wired_startup_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let source = format!("control {{ socket {:?} }}", socket.display().to_string());
        let config =
            CompiledConfig::from_config(parse_config(&source).expect("parse")).expect("compile");

        let runtime = Runtime::start(config, PathBuf::from("public-start-test.limpid"))
            .await
            .expect("public start must use the working registry-wired startup path");
        runtime.shutdown().await;
    }

    async fn assert_startup_build_info(
        configured_node_id: Option<&str>,
        expected_node_id: &str,
        expected_resolver_calls: usize,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let node_id = configured_node_id
            .map(|node_id| format!("node_id \"{node_id}\"\n"))
            .unwrap_or_default();
        let source = format!(
            "{node_id}control {{ socket {:?} }}",
            socket.display().to_string()
        );
        let config = compiled_config(&source);
        let registry = Arc::new(Registry::new());
        let resolver_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&resolver_calls);

        let runtime = Runtime::start_with_registry_and_node_id_resolver(
            config,
            PathBuf::from("build-info-startup-test.limpid"),
            Arc::clone(&registry),
            move || {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(format!("resolved-host-{call}"))
            },
        )
        .await
        .expect("runtime must start");

        let labels = [
            ("node_id", expected_node_id),
            ("version", env!("CARGO_PKG_VERSION")),
        ];
        assert_eq!(series_value(&registry, "limpid_build_info", &labels), 1);
        assert_eq!(metric_series(&registry, "limpid_build_info").len(), 1);
        assert_eq!(
            resolver_calls.load(Ordering::Relaxed),
            expected_resolver_calls,
            "startup must resolve hostname exactly when node_id is omitted"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn startup_with_explicit_node_id_skips_hostname_and_registers_that_value() {
        assert_startup_build_info(Some("configured-node"), "configured-node", 0).await;
    }

    #[tokio::test]
    async fn startup_without_node_id_resolves_hostname_once_and_registers_that_value() {
        assert_startup_build_info(None, "resolved-host-1", 1).await;
    }

    #[tokio::test]
    async fn startup_preflights_a_declared_node_key_and_ignores_an_omitted_one() {
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let key = dir.path().join("node-key.pem");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate key");
        std::fs::write(
            &key,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .expect("write key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .expect("secure key mode");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let peer_pubkey = base64::engine::general_purpose::STANDARD.encode(spki);

        let source = format!(
            "node_id \"node-a\"\nnode_key {:?}\ncontrol {{ socket {:?} }}\n\
             def output ltp_out {{ type ltp peer {{ node_id \"peer-a\" pubkey {:?} endpoint \"127.0.0.1:1\" }} }}",
            key.display().to_string(),
            socket.display().to_string(),
            peer_pubkey,
        );
        let runtime = Runtime::start(
            compiled_config(&source),
            PathBuf::from("node-key-startup-test.limpid"),
        )
        .await
        .expect("declared valid key must pass startup preflight");
        runtime.shutdown().await;

        let omitted_socket = dir.path().join("omitted-control.sock");
        let omitted = format!(
            "node_id \"node-a\"\ncontrol {{ socket {:?} }}",
            omitted_socket.display().to_string()
        );
        let runtime = Runtime::start(
            compiled_config(&omitted),
            PathBuf::from("missing-path-is-not-consulted.limpid"),
        )
        .await
        .expect("omitted node_key must not trigger filesystem preflight");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn startup_fails_before_tasks_when_a_declared_node_key_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let missing = dir.path().join("missing-node-key.pem");
        let source = format!(
            "node_id \"node-a\"\nnode_key {:?}\ncontrol {{ socket {:?} }}",
            missing.display().to_string(),
            socket.display().to_string()
        );

        let error = match Runtime::start(
            compiled_config(&source),
            PathBuf::from("node-key-failure-test.limpid"),
        )
        .await
        {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("declared missing node_key must fail startup")
            }
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("secure open failed"));
        assert!(
            !socket.exists(),
            "control task must not start before key preflight"
        );
    }

    #[tokio::test]
    async fn startup_propagates_duplicate_build_info_from_the_actual_registry() {
        let config = compiled_config("node_id \"configured-node\"");
        let registry = Arc::new(Registry::new());
        crate::metrics::register_build_info(
            &registry,
            env!("CARGO_PKG_VERSION"),
            "configured-node",
        )
        .expect("preseed build info");
        let resolver_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&resolver_calls);

        let error = match Runtime::start_with_registry_and_node_id_resolver(
            config,
            PathBuf::from("duplicate-build-info-startup-test.limpid"),
            Arc::clone(&registry),
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok("unexpected-hostname".to_owned())
            },
        )
        .await
        {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("duplicate build-info registration must fail startup");
            }
            Err(error) => error,
        };

        let metrics_error = error
            .downcast_ref::<MetricsError>()
            .expect("startup error must retain MetricsError");
        let expected_labelset = vec![
            ("node_id".to_owned(), "configured-node".to_owned()),
            ("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        ];
        match metrics_error {
            MetricsError::DuplicateSeries { name, labelset } => {
                assert_eq!(name, "limpid_build_info");
                assert_eq!(labelset, &expected_labelset);
            }
            other => panic!("expected DuplicateSeries, got {other:?}"),
        }
        let diagnostic = error.to_string();
        assert!(
            diagnostic.contains(&format!("name={:?}", "limpid_build_info")),
            "diagnostic must identify the metric family: {diagnostic}"
        );
        assert!(
            diagnostic.contains(&format!("labelset={expected_labelset:?}")),
            "diagnostic must include the complete labelset: {diagnostic}"
        );
        assert_eq!(resolver_calls.load(Ordering::Relaxed), 0);
        assert_eq!(metric_series(&registry, "limpid_build_info").len(), 1);
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
        let metrics_registry = Registry::new();
        let worker = Arc::new(
            PipelineWorker::new(def, &metrics_registry).expect("pipeline metrics must register"),
        );
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
        let disk_outputs = Arc::new(HashSet::new());
        let ctx_a = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::clone(&disk_outputs),
            config: Arc::new(cfg.clone()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: tap.clone(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        let ctx_b = PipelineContext {
            output_senders: Arc::clone(&ctx_a.output_senders),
            disk_outputs: Arc::clone(&disk_outputs),
            config: Arc::clone(&ctx_a.config),
            funcs: Arc::clone(&ctx_a.funcs),
            tap: tap.clone(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
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
        assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn pipeline_inflight_counts_concurrent_runs_and_returns_to_zero() {
        let def = pipeline_def("def pipeline p { input a, b; output sink; finish }");
        let metrics_registry = Registry::new();
        let worker = Arc::new(
            PipelineWorker::new(def, &metrics_registry).expect("pipeline metrics must register"),
        );
        let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(vec![Arc::clone(&worker)]);

        let (queue_sender, mut queue_receiver) = crate::queue::create_queue(
            "sink".to_owned(),
            crate::queue::QueueConfig {
                queue_type: crate::queue::QueueType::Memory,
                capacity: 1,
            },
        )
        .expect("memory queue");
        queue_sender
            .send(crate::event::QueuedEvent::new(
                Event::new(
                    Bytes::from_static(b"filler"),
                    "127.0.0.1:0".parse().unwrap(),
                ),
                crate::time::UnixNanos::now(),
            ))
            .await
            .expect("prefill queue");

        let cfg = CompiledConfig::from_config(parse_config("").unwrap()).unwrap();
        let tap = TapRegistry::new();
        tap.register("input a").await;
        tap.register("input b").await;
        let ctx = Arc::new(PipelineContext {
            output_senders: Arc::new(HashMap::from([("sink".to_owned(), queue_sender)])),
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(cfg),
            funcs: Arc::new(FunctionRegistry::new()),
            tap,
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        });
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx_a, rx_a) = mpsc::channel(1);
        let (tx_b, rx_b) = mpsc::channel(1);
        let h_a = {
            let workers = Arc::clone(&workers);
            let ctx = Arc::clone(&ctx);
            let shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                run_pipeline_workers(rx_a, &workers, &ctx, "a", shutdown).await;
            })
        };
        let h_b = {
            let workers = Arc::clone(&workers);
            let ctx = Arc::clone(&ctx);
            tokio::spawn(async move {
                run_pipeline_workers(rx_b, &workers, &ctx, "b", shutdown_rx).await;
            })
        };

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        tx_a.send(Event::new(Bytes::from_static(b"a"), addr))
            .await
            .unwrap();
        tx_b.send(Event::new(Bytes::from_static(b"b"), addr))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while worker.metrics.inflight.load(Ordering::Relaxed) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both blocked pipeline runs must become observable");

        for _ in 0..3 {
            queue_receiver.recv().await.expect("queued event");
        }
        drop(tx_a);
        drop(tx_b);
        tokio::time::timeout(Duration::from_secs(2), async {
            h_a.await.unwrap();
            h_b.await.unwrap();
        })
        .await
        .expect("pipeline workers must drain");
        assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
        assert_eq!(worker.metrics.events_finished.load(Ordering::Relaxed), 2);
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
        let metrics = PipelineMetrics::for_testing();
        let err_ctx = make_err_ctx("simulated runtime error");

        write_errored_to_dlq(
            &err_ctx,
            &metrics,
            Some(&writer),
            crate::error_log::ErrorLogFallback::default(),
        )
        .await;

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
        let metrics = PipelineMetrics::for_testing();
        let err_ctx = make_err_ctx("no DLQ configured");

        write_errored_to_dlq(
            &err_ctx,
            &metrics,
            None,
            crate::error_log::ErrorLogFallback::default(),
        )
        .await;

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
            disk_outputs: Arc::new(HashSet::new()),
            config: Arc::new(cfg),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
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

    #[tokio::test]
    async fn pipeline_inflight_covers_errored_termination_and_direct_error_dlq_work() {
        for (body, reason) in [
            ("error \"terminal failure\"", "terminal failure"),
            (
                "error missing_runtime_function()",
                "missing_runtime_function",
            ),
        ] {
            let def = pipeline_def(&format!("def pipeline p {{ input i; {body} }}"));
            let registry = Registry::new();
            let worker = Arc::new(
                PipelineWorker::new(def, &registry).expect("pipeline metrics must register"),
            );
            let dir = tempfile::tempdir().unwrap();
            let log_path = dir.path().join("pipeline-errors.jsonl");
            let error_log = Arc::new(crate::error_log::ErrorLogWriter::new(log_path.clone()));
            let guard = error_log.hold_write_lock_for_testing().await;
            let cfg = CompiledConfig::from_config(parse_config("").unwrap()).unwrap();
            let ctx = PipelineContext {
                output_senders: Arc::new(HashMap::new()),
                disk_outputs: Arc::new(HashSet::new()),
                config: Arc::new(cfg),
                funcs: Arc::new(FunctionRegistry::new()),
                tap: TapRegistry::new(),
                error_log: Some(Arc::clone(&error_log)),
                error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            };
            let event = Event::new(
                Bytes::from_static(b"payload"),
                SocketAddr::from_str("127.0.0.1:0").unwrap(),
            );
            let task_worker = Arc::clone(&worker);
            let task = tokio::spawn(async move {
                let mut bump = bumpalo::Bump::new();
                process_event(&event, &[task_worker], &ctx, "input i", &mut bump).await;
            });

            tokio::time::timeout(Duration::from_secs(2), async {
                while worker.metrics.events_errored.load(Ordering::Relaxed) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("runtime error must reach terminal bookkeeping");
            assert!(!task.is_finished(), "DLQ write must still be held");
            assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 1);

            drop(guard);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("DLQ completion must release the pipeline")
                .expect("pipeline task must not panic");
            assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
            let record = tokio::fs::read_to_string(&log_path).await.unwrap();
            assert!(record.contains(reason), "unexpected DLQ record: {record}");
        }
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

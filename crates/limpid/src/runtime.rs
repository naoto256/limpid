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
use tracing::{info, error};

use crate::control::ControlServer;
use crate::dsl::ast::*;
use crate::dsl::props;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::{MetricsRegistry, PipelineMetrics};
use crate::modules::{self, HasMetrics, ModuleRegistry};
use crate::pipeline::CompiledConfig;
use crate::queue::{self, QueueConfig, QueueSender, RetryConfig};
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
        let func_registry = Arc::new(func_registry);

        config.validate(&registry)?;
        let registry = Arc::new(registry);

        let mut metrics_registry = MetricsRegistry::new();
        let tap = TapRegistry::new();

        // --- 1. Create outputs (each output owns its own OutputMetrics) ---
        let mut output_senders: HashMap<String, QueueSender> = HashMap::new();
        let mut output_receivers = Vec::new();

        for (name, output_def) in &config.outputs {
            let queue_config = QueueConfig::from_output_properties(name, &output_def.properties)?;
            let retry_config = RetryConfig::from_output_properties(&output_def.properties)?;
            let (mut sender, receiver) = queue::create_queue(name.clone(), queue_config)?;

            let output_type = props::get_ident(&output_def.properties, "type")
                .ok_or_else(|| anyhow::anyhow!("output '{}' has no type", name))?;
            let created = match registry.create_output(&output_type, name, &output_def.properties) {
                Ok(c) => c,
                Err(e) => {
                    error!("failed to create output '{}': {} — aborting startup", name, e);
                    for h in &handles { h.abort(); }
                    for h in handles { let _ = h.await; }
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

            output_receivers.push((receiver, created.writer, retry_config, output_metrics));
        }

        // Start queue consumers (no metrics counting here — output does it)
        for (receiver, writer, retry_config, output_metrics) in output_receivers {
            let secondary_sender = retry_config
                .secondary
                .as_ref()
                .and_then(|s| output_senders.get(s).cloned());
            let shutdown = shutdown_rx.clone();
            let tap_clone = tap.clone();
            handles.push(tokio::spawn(async move {
                queue::run_queue_consumer(receiver, writer, retry_config, secondary_sender, Some(tap_clone), output_metrics, shutdown)
                    .await;
            }));
        }

        let output_senders = Arc::new(output_senders);

        // --- 2. Group pipelines by input ---
        let mut input_pipelines: HashMap<String, Vec<PipelineWorker>> = HashMap::new();

        for pipeline_def in config.pipelines.values() {
            let worker = PipelineWorker::new(pipeline_def.clone());
            metrics_registry.register_pipeline(&pipeline_def.name, worker.metrics());
            if let Some(input_name) = get_pipeline_input(pipeline_def) {
                input_pipelines
                    .entry(input_name.clone())
                    .or_default()
                    .push(worker);
            }
        }

        // --- 2b. Register tap points for inputs and processes ---
        for input_name in input_pipelines.keys() {
            tap.register(&format!("input {}", input_name)).await;
        }
        for proc_name in config.processes.keys() {
            tap.register(&format!("process {}", proc_name)).await;
        }
        for name in registry.process_names() {
            tap.register(&format!("process {}", name)).await;
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

            let input_type = props::get_ident(&input_def.properties, "type")
                .ok_or_else(|| anyhow::anyhow!("input '{}' has no type", input_name))?;

            let queue_size =
                props::get_positive_int(&input_def.properties, "queue_size")?
                    .unwrap_or(4096) as usize;
            let (event_tx, event_rx) = mpsc::channel::<Event>(queue_size);

            // Pipeline workers
            let workers = Arc::new(pipelines);
            let ctx = PipelineContext {
                output_senders: Arc::clone(&output_senders),
                config: Arc::clone(&config),
                builtins: Arc::clone(&registry),
                funcs: Arc::clone(&func_registry),
                tap: tap.clone(),
            };
            let iname = input_name.clone();
            let shutdown_for_worker = shutdown_rx.clone();
            let sender_for_inject = event_tx.clone();
            handles.push(tokio::spawn(async move {
                run_pipeline_workers(event_rx, &workers, &ctx, &iname, shutdown_for_worker).await;
            }));

            // Input — registry builds, spawns, and returns metrics handle
            let created = match registry.create_input(&input_type, &input_name, &input_def.properties, event_tx, shutdown_rx.clone()) {
                Ok(c) => c,
                Err(e) => {
                    error!("failed to start input '{}': {} — aborting startup", input_name, e);
                    for h in &handles { h.abort(); }
                    for h in handles { let _ = h.await; }
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

        info!("initiating graceful shutdown (timeout: {}s)", SHUTDOWN_TIMEOUT.as_secs());
        let _ = self.shutdown_tx.send(true);

        // Collect abort handles before moving JoinHandles into join_all
        let abort_handles: Vec<_> = self.handles.iter().map(|h| h.abort_handle()).collect();

        match timeout(SHUTDOWN_TIMEOUT, Self::join_all(self.handles)).await {
            Ok(()) => {
                info!("shutdown complete");
            }
            Err(_) => {
                error!("shutdown timed out after {}s — aborting remaining tasks", SHUTDOWN_TIMEOUT.as_secs());
                for ah in &abort_handles {
                    ah.abort();
                }
            }
        }
    }

    async fn join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
        for handle in handles {
            if let Err(e) = handle.await
                && e.is_panic() {
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
            if let Property::Block(table_name, inner_props) = prop {
                let load_path = props::get_string(inner_props, "load").map(PathBuf::from);
                let max = props::get_positive_int(inner_props, "max")?.map(|n| n as usize);
                let ttl = props::get_positive_int(inner_props, "ttl")?
                    .map(Duration::from_secs);

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
    builtins: Arc<ModuleRegistry>,
    funcs: Arc<FunctionRegistry>,
    tap: TapRegistry,
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
    workers: &[PipelineWorker],
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

    loop {
        let event = tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Drain remaining events from channel before stopping
                    while let Ok(event) = event_rx.try_recv() {
                        process_event(&event, workers, ctx, &input_tap_key).await;
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
        process_event(&event, workers, ctx, &input_tap_key).await;
    }

    info!("pipeline worker for input '{}' stopped", input_name);
}

async fn run_pipeline_with_outputs(
    pipeline: &PipelineDef,
    event: Event,
    ctx: &PipelineContext,
) -> Result<crate::pipeline::PipelineRunResult> {
    let result = crate::pipeline::run_pipeline(pipeline, event, &ctx.config, &ctx.builtins, &ctx.funcs, Some(&ctx.tap))?;

    for (output_name, output_event) in &result.outputs {
        if let Some(sender) = ctx.output_senders.get(output_name) {
            sender.send(output_event.clone()).await;
        } else {
            error!(
                "pipeline '{}': output '{}' not found",
                pipeline.name, output_name
            );
        }
    }

    Ok(result)
}

async fn process_event(
    event: &Event,
    workers: &[PipelineWorker],
    ctx: &PipelineContext,
    input_tap_key: &str,
) {
    ctx.tap.emit(input_tap_key, event).await;
    for worker in workers {
        worker.metrics.events_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let event_copy = event.clone();
        match run_pipeline_with_outputs(&worker.def, event_copy, ctx).await {
            Ok(result) => {
                use crate::pipeline::PipelineTermination;
                match result.termination {
                    PipelineTermination::Dropped => {
                        worker.metrics.events_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    PipelineTermination::Finished => {
                        if result.outputs.is_empty() {
                            worker.metrics.events_discarded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            worker.metrics.events_finished.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
            Err(e) => {
                error!("pipeline '{}': {}", worker.def.name, e);
            }
        }
    }
}

fn get_pipeline_input(pipeline: &PipelineDef) -> Option<&String> {
    for stmt in &pipeline.body {
        if let PipelineStatement::Input(name) = stmt {
            return Some(name);
        }
    }
    None
}

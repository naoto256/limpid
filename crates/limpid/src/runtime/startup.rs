use super::*;

async fn post_output_preactivation_failure(_sender: &mut QueueSender) -> Result<()> {
    #[cfg(test)]
    if let Some(failure) = POST_OUTPUT_PREACTIVATION_FAILURE
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten()
    {
        if failure.enqueue_probe {
            _sender
                .send(crate::event::QueuedEvent::new(
                    Event::new(
                        bytes::Bytes::from_static(b"startup-rollback-probe"),
                        "127.0.0.1:1".parse().expect("static probe address"),
                    ),
                    crate::time::UnixNanos::now(),
                ))
                .await
                .context("failed to enqueue startup rollback probe")?;
        }
        failure.reached.notify_one();
        match failure.mode {
            PostOutputFailureMode::Error => {
                anyhow::bail!("test failpoint: post-output preactivation failure");
            }
            PostOutputFailureMode::Park => std::future::pending::<()>().await,
        }
    }
    Ok(())
}

fn late_post_listener_failure() -> Result<()> {
    #[cfg(test)]
    if LATE_POST_LISTENER_FAILURE
        .try_with(|armed| armed.replace(false))
        .unwrap_or(false)
    {
        anyhow::bail!("test failpoint: late post-listener startup failure");
    }
    Ok(())
}

pub(super) fn configured_ltp_peer_ids(
    blueprint: &crate::pipeline::RuntimeBlueprint,
) -> Result<BTreeSet<String>> {
    let mut peers = BTreeSet::new();
    for (kind, name, properties) in blueprint
        .inputs()
        .iter()
        .map(|(name, def)| ("input", name, &def.properties))
        .chain(
            blueprint
                .outputs()
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
        config.validate()?;
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config)?;
        Self::start_blueprint(blueprint, config_file).await
    }

    pub(crate) async fn start_blueprint(
        blueprint: Arc<crate::pipeline::RuntimeBlueprint>,
        config_file: PathBuf,
    ) -> Result<Self> {
        Self::start_blueprint_with_registry(blueprint, config_file, Arc::new(Registry::new())).await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_registry(
        config: CompiledConfig,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
    ) -> Result<Self> {
        config.validate()?;
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config)?;
        Self::start_blueprint_with_registry(blueprint, config_file, metrics_registry).await
    }

    async fn start_blueprint_with_registry(
        blueprint: Arc<crate::pipeline::RuntimeBlueprint>,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
    ) -> Result<Self> {
        Self::start_blueprint_with_registry_and_node_id_resolver(
            blueprint,
            config_file,
            metrics_registry,
            || Ok(gethostname::gethostname().to_string_lossy().into_owned()),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_registry_and_node_id_resolver<F>(
        config: CompiledConfig,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
        resolve_hostname: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<String>,
    {
        config.validate()?;
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config)?;
        Self::start_blueprint_with_registry_and_node_id_resolver(
            blueprint,
            config_file,
            metrics_registry,
            resolve_hostname,
        )
        .await
    }

    async fn start_blueprint_with_registry_and_node_id_resolver<F>(
        blueprint: Arc<crate::pipeline::RuntimeBlueprint>,
        config_file: PathBuf,
        metrics_registry: Arc<Registry>,
        resolve_hostname: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<String>,
    {
        // Bind every descriptor before any table, socket, module, or task is
        // acquired. A failed bind therefore leaves external resources at 0.
        let bound_blueprint = Arc::new(blueprint.bind(&metrics_registry)?);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut registry = ModuleRegistry::new();
        modules::register_builtins(&mut registry);
        // Future: dynamic plugin loading from /etc/limpid/plugins/

        init_geoip_from_globals(blueprint.global_blocks());
        let table_store = init_tables_from_globals(blueprint.global_blocks())?;

        let mut func_registry = FunctionRegistry::new();
        crate::functions::register_builtins(&mut func_registry, table_store);
        crate::functions::register_user_function_defs(
            &mut func_registry,
            blueprint.functions().values(),
        );
        let func_registry = Arc::new(func_registry);

        let ltp_peer_ids = configured_ltp_peer_ids(&blueprint)?;
        let ltp_metrics = if ltp_peer_ids.is_empty() {
            None
        } else {
            Some(LtpMetrics::register(&metrics_registry, &ltp_peer_ids)?)
        };
        let ltp_node_key = blueprint
            .node_key()
            .map(Path::new)
            .map(crate::ltp::load_node_key)
            .transpose()?
            .map(Arc::new);
        let node_id = match blueprint.node_id() {
            Some(node_id) => node_id.to_string(),
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
        let error_log_path = blueprint
            .global_blocks()
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
        let error_log_fallback = match blueprint
            .global_blocks()
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

        // Complete every deterministic plan named by the startup contract
        // before an output factory can acquire a resource.
        let control_path = blueprint
            .global_blocks()
            .get("control")
            .and_then(|p| props::get_string(p, "socket"));
        crate::control::validate_control_socket_parent(control_path.as_deref())?;
        let stdout_regular_file = if blueprint
            .outputs()
            .values()
            .any(|output| output.properties.type_name() == "stdout")
        {
            crate::modules::output::stdout::stdout_is_regular_file()
                .context("failed to classify stdout backend")?
        } else {
            false
        };

        // Group sealed pipeline identities by input. The worker never owns an
        // AST PipelineDef/ProcessDef clone.
        let mut input_pipelines: HashMap<String, Vec<Arc<PipelineWorker>>> = HashMap::new();
        for (pipeline_id, pipeline) in blueprint.pipelines() {
            let worker = Arc::new(PipelineWorker::from_bound(
                pipeline_id,
                &blueprint,
                &metrics_registry,
            )?);
            // Routing deliberately uses only the first top-level `input`
            // statement. Control-list flow remains a recursive union for
            // compatibility; changing that historical split is a separate WI.
            for input_name in routing_inputs(pipeline) {
                input_pipelines
                    .entry(input_name.clone())
                    .or_default()
                    .push(Arc::clone(&worker));
            }
        }

        let mut input_queue_sizes = HashMap::new();
        let mut prepared_ltp_inputs = HashMap::new();
        for input_name in input_pipelines.keys() {
            let input_def = blueprint
                .inputs()
                .get(input_name)
                .ok_or_else(|| anyhow::anyhow!("input '{}' not found", input_name))?;
            let queue_size =
                props::get_positive_int(input_def.properties.user_properties(), "queue_size")?
                    .unwrap_or(4096) as usize;
            input_queue_sizes.insert(input_name.clone(), queue_size);
            if input_def.properties.type_name() == "ltp" {
                let input = modules::input::ltp::LtpInput::build(
                    input_name,
                    &input_def.properties,
                    &build_ctx,
                )
                .with_context(|| format!("failed to create input '{input_name}'"))?;
                prepared_ltp_inputs.insert(input_name.clone(), input);
            }
        }

        // Tap registration does not create runtime actors or output resources.
        for input_name in input_pipelines.keys() {
            tap.register(&format!("input {}", input_name)).await;
        }
        register_process_taps(&tap, &blueprint).await;

        // From this point on, every acquired output resource is transactionally
        // owned. Each real queue consumer is spawned and registered immediately
        // after its output factory succeeds, before the next await or fallible
        // edge.
        let mut startup_guard = StartupGuard::new(shutdown_tx);

        // --- 1. Create outputs (each output owns its own OutputMetrics) ---
        let mut output_senders: HashMap<String, QueueSender> = HashMap::new();
        // Populated in the loop below alongside the queue creation so
        // the same `QueueConfig` decides which set of outputs need a
        // workspace-carrying snapshot at `output` statement time. See
        // `PipelineContext::disk_outputs` for the runtime contract.
        let mut disk_outputs: HashSet<String> = HashSet::new();

        for (name, output_def) in blueprint.outputs() {
            let queue_config = match QueueConfig::from_output_properties(
                name,
                output_def.properties.user_properties(),
            ) {
                Ok(config) => config,
                Err(error) => return Err(startup_guard.rollback(error).await),
            };
            let output_task_kind = output_task_kind(
                output_def.properties.type_name(),
                &queue_config.queue_type,
                error_log.is_some(),
                stdout_regular_file,
            );
            if matches!(queue_config.queue_type, queue::QueueType::Disk { .. }) {
                disk_outputs.insert(name.clone());
            }
            // Retry config is parsed by each output's `from_properties`
            // (outputs own retry + DLQ). The runtime no longer needs a
            // copy here.
            let (mut sender, receiver) = match queue::create_queue(name.clone(), queue_config) {
                Ok(queue) => queue,
                Err(error) => return Err(startup_guard.rollback(error).await),
            };

            // `output_def.properties` is a `ModuleProperties`: it carries the
            // resolved `type` already, so `create_output` doesn't take a
            // separate type_name argument (and can't be passed one — the
            // strip is the whole point). `BuildContext` carries `funcs` and
            // the optional `error_log` so outputs can stash them at
            // construction time.
            let created = match registry
                .create_output(name, &output_def.properties, &build_ctx)
                .with_context(|| format!("failed to create output '{name}'"))
            {
                Ok(created) => created,
                Err(error) => return Err(startup_guard.rollback(error).await),
            };

            // Attach metrics so QueueSender::send counts events_received.
            sender.attach_metrics(Arc::clone(&created.metrics));
            let output_metrics = Arc::clone(&created.metrics);
            let shutdown = shutdown_rx.clone();
            let tap_clone = tap.clone();
            let error_log_for_consumer = error_log.as_ref().map(Arc::clone);
            #[cfg(test)]
            let completion_observer = STARTUP_TASK_COMPLETIONS.try_with(Arc::clone).ok();
            startup_guard.track(
                output_task_kind,
                tokio::spawn(async move {
                    queue::run_queue_consumer(
                        receiver,
                        created.output,
                        Some(tap_clone),
                        output_metrics,
                        error_log_for_consumer,
                        shutdown,
                    )
                    .await;
                    #[cfg(test)]
                    if let Some(observer) = completion_observer {
                        observer
                            .output_queues
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }),
            );
            if let Err(error) = post_output_preactivation_failure(&mut sender).await {
                return Err(startup_guard.rollback(error).await);
            }
            output_senders.insert(name.clone(), sender);
            tap.register(&format!("output {}", name)).await;
        }

        let output_senders = Arc::new(output_senders);
        let has_disk_output = !disk_outputs.is_empty();
        let disk_outputs = Arc::new(disk_outputs);

        // --- 3. Start inputs (each input owns its own InputMetrics) ---

        let mut input_senders: HashMap<
            String,
            (mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>),
        > = HashMap::new();
        let mut ltp_inputs = Vec::new();

        for (input_name, pipelines) in input_pipelines {
            let input_def = blueprint
                .inputs()
                .get(&input_name)
                .expect("input preflight");
            let queue_size = input_queue_sizes
                .remove(&input_name)
                .expect("input queue-size preflight");
            let (event_tx, event_rx) = mpsc::channel::<Event>(queue_size);
            let input_queue_timer =
                match crate::metrics::InputQueueTimer::register(&metrics_registry, &input_name) {
                    Ok(timer) => timer,
                    Err(error) => return Err(startup_guard.rollback(error.into()).await),
                };

            // Pipeline workers subscribed to this input. A pipeline with fan-in
            // (`input a, b;`) appears in the worker list of both inputs — its
            // merge semantics is implicit: two dispatcher tasks feeding the
            // same `PipelineWorker`, serialized through its own `run_pipeline`
            // call per event. No ordering guarantee between inputs.
            let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(pipelines);
            let ctx = PipelineContext {
                output_senders: Arc::clone(&output_senders),
                disk_outputs: Arc::clone(&disk_outputs),
                bound_blueprint: Arc::clone(&bound_blueprint),
                funcs: Arc::clone(&func_registry),
                tap: tap.clone(),
                error_log: error_log.as_ref().map(Arc::clone),
                error_log_fallback,
            };
            let iname = input_name.clone();
            let shutdown_for_worker = shutdown_rx.clone();
            let sender_for_inject = event_tx.clone();
            #[cfg(test)]
            let completion_observer = STARTUP_TASK_COMPLETIONS.try_with(Arc::clone).ok();
            #[cfg(test)]
            let wal_barrier = queue::current_test_wal_barrier();
            let pipeline_task_kind = pipeline_task_kind(error_log.is_some(), has_disk_output);
            startup_guard.track(
                pipeline_task_kind,
                tokio::spawn(async move {
                    #[cfg(test)]
                    if let Some(barrier) = wal_barrier {
                        queue::with_test_wal_barrier(
                            barrier,
                            run_pipeline_workers(
                                event_rx,
                                &workers,
                                &ctx,
                                &iname,
                                &input_queue_timer,
                                shutdown_for_worker,
                            ),
                        )
                        .await;
                    } else {
                        run_pipeline_workers(
                            event_rx,
                            &workers,
                            &ctx,
                            &iname,
                            &input_queue_timer,
                            shutdown_for_worker,
                        )
                        .await;
                    }
                    #[cfg(not(test))]
                    run_pipeline_workers(
                        event_rx,
                        &workers,
                        &ctx,
                        &iname,
                        &input_queue_timer,
                        shutdown_for_worker,
                    )
                    .await;
                    #[cfg(test)]
                    if let Some(observer) = completion_observer {
                        observer
                            .pipelines
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }),
            );

            if input_def.properties.type_name() == "ltp" {
                let input = prepared_ltp_inputs
                    .remove(&input_name)
                    .expect("LTP input preflight");
                input_senders.insert(input_name.clone(), (sender_for_inject, input.metrics()));
                ltp_inputs.push((input, event_tx));
                continue;
            }

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
                    let error = e.context(format!("failed to start input '{input_name}'"));
                    return Err(startup_guard.rollback(error).await);
                }
            };
            input_senders.insert(
                input_name.clone(),
                (sender_for_inject, Arc::clone(&created.metrics)),
            );
            let input_task_kind = input_task_kind(input_def.properties.type_name());
            startup_guard.track(input_task_kind, created.handle);
            match created.startup.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let error = error.context(format!("failed to start input '{input_name}'"));
                    return Err(startup_guard.rollback(error).await);
                }
                Err(_) => {
                    let error = anyhow::anyhow!(
                        "input '{input_name}' task exited before startup readiness"
                    );
                    return Err(startup_guard.rollback(error).await);
                }
            }
        }

        let ltp_handles =
            match modules::input::ltp::start_listener_groups(ltp_inputs, shutdown_rx.clone()).await
            {
                Ok(handles) => handles,
                Err(error) => {
                    return Err(startup_guard
                        .rollback(error.context("failed to start LTP listener groups"))
                        .await);
                }
            };
        startup_guard.extend(TaskKind::AbortSafe, ltp_handles);

        if let Err(error) = late_post_listener_failure() {
            return Err(startup_guard.rollback(error).await);
        }

        // --- 4. Start control socket (after all metrics are registered) ---
        let started_at = std::time::Instant::now();
        let control = ControlServer::new(
            control_path,
            tap.clone(),
            Arc::clone(&metrics_registry),
            Arc::clone(&blueprint),
            input_senders,
            Arc::clone(&output_senders),
            started_at,
        );
        let s = shutdown_rx.clone();
        let (control_startup_tx, control_startup_rx) = tokio::sync::oneshot::channel();
        startup_guard.track(
            TaskKind::AbortSafe,
            tokio::spawn(async move {
                control.run(s, Some(control_startup_tx)).await;
            }),
        );
        match control_startup_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(diagnostic)) => {
                return Err(startup_guard.rollback(anyhow::anyhow!(diagnostic)).await);
            }
            Err(_) => {
                return Err(startup_guard
                    .rollback(anyhow::anyhow!(
                        "control task exited before startup readiness"
                    ))
                    .await);
            }
        }

        let (shutdown_tx, handles) = startup_guard.commit();
        info!("limpid daemon started");
        Ok(Self {
            shutdown_tx,
            handles,
            config_file,
            blueprint,
            #[cfg(test)]
            test_identity: RuntimeTestIdentity {
                metrics_registry,
                funcs: func_registry,
                tap,
            },
        })
    }
}

async fn register_process_taps(tap: &TapRegistry, blueprint: &crate::pipeline::RuntimeBlueprint) {
    for (name, kind) in blueprint.process_body_inventory() {
        if kind == crate::pipeline::SiteKind::Named {
            tap.register(&format!("process {name}")).await;
        }
    }
}

fn routing_inputs(pipeline: &crate::pipeline::PipelineBlueprint) -> &[String] {
    &pipeline.subscription_inputs
}

// ---------------------------------------------------------------------------
// Global subsystem initialization
// ---------------------------------------------------------------------------

fn init_geoip_from_globals(global_blocks: &HashMap<String, Vec<Property>>) {
    let db_path = global_blocks
        .get("geoip")
        .and_then(|p| props::get_string(p, "database"))
        .map(PathBuf::from);
    crate::functions::geoip::init(db_path.as_ref());
}

pub(crate) fn init_tables(config: &CompiledConfig) -> Result<crate::functions::table::TableStore> {
    init_tables_from_globals(&config.global_blocks)
}

fn init_tables_from_globals(
    global_blocks: &HashMap<String, Vec<Property>>,
) -> Result<crate::functions::table::TableStore> {
    use crate::dsl::ast::Property;
    use crate::functions::table::{TableConfig, TableStore};
    use std::time::Duration;

    let mut configs = Vec::new();

    if let Some(props) = global_blocks.get("table") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;
    use base64::Engine as _;

    #[tokio::test]
    async fn process_tap_inventory_registers_named_definitions_but_no_inline_sites() {
        let config = CompiledConfig::from_config(
            parse_config(
                "def process named { egress = ingress } \
                 def process unused { egress = ingress } \
                 def pipeline p { process named; process { egress = ingress }; \
                   if true { process { egress = ingress }; finish } else { finish } }",
            )
            .unwrap(),
        )
        .unwrap();
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config).unwrap();
        let tap = TapRegistry::new();
        register_process_taps(&tap, &blueprint).await;

        let mut named = 0;
        let mut inline = 0;
        for (name, kind) in blueprint.process_body_inventory() {
            let subscription = tap.subscribe(&format!("process {name}")).await;
            match kind {
                crate::pipeline::SiteKind::Named => {
                    named += 1;
                    assert!(subscription.is_some(), "missing named tap for {}", name);
                }
                crate::pipeline::SiteKind::Inline => {
                    inline += 1;
                    assert!(
                        subscription.is_none(),
                        "inline body {name} produced a ghost tap"
                    );
                }
            }
        }
        assert_eq!(named, 2, "fixture must exercise every named body");
        assert_eq!(inline, 2, "fixture must exercise multiple inline bodies");
    }

    #[test]
    fn routing_helper_uses_only_the_first_top_level_input_statement() {
        let config = CompiledConfig::from_config(
            parse_config(
                "def pipeline p { input a; input b; if true { input c; finish } else { finish } } \
                 def pipeline fan_in { input a, b, c; finish }",
            )
            .unwrap(),
        )
        .unwrap();
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config).unwrap();
        assert_eq!(routing_inputs(blueprint.pipeline("p").unwrap()), ["a"]);
        assert_eq!(
            routing_inputs(blueprint.pipeline("fan_in").unwrap()),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn ltp_peer_registration_uses_deduplicated_input_output_union() {
        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&[7; 32]);
        let pubkey = base64::engine::general_purpose::STANDARD.encode(spki);
        let config = CompiledConfig::from_config(
            parse_config(&format!(
                "def input ltp_in {{ type ltp peer {{ node_id \"peer-input\" pubkey {pubkey:?} }} }}\n\
                 def output ltp_out_a {{ type ltp peer {{ node_id \"peer-a\" pubkey {pubkey:?} endpoint \"127.0.0.1:7514\" }} }}\n\
                 def output ltp_out_b {{ type ltp peer {{ node_id \"peer-b\" pubkey {pubkey:?} endpoint \"127.0.0.1:7515\" }} }}"
            ))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            configured_ltp_peer_ids(&crate::pipeline::compile_runtime_blueprint(&config).unwrap(),)
                .unwrap(),
            BTreeSet::from([
                "peer-a".to_owned(),
                "peer-b".to_owned(),
                "peer-input".to_owned(),
            ])
        );
    }
}

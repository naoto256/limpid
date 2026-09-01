use super::*;

pub(super) struct PipelineContext {
    pub(super) output_senders: Arc<HashMap<String, QueueSender>>,
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
    pub(super) disk_outputs: Arc<HashSet<String>>,
    pub(super) bound_blueprint: Arc<crate::pipeline::BoundRuntimeBlueprint>,
    pub(super) funcs: Arc<FunctionRegistry>,
    pub(super) tap: TapRegistry,
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
    pub(super) error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    /// Operator-selected confidentiality policy for the tracing-side
    /// fallback line. Threaded through `write_errored_to_dlq` so the
    /// pipeline-side runtime error surface obeys the same ladder as
    /// the sink-side DLQ paths.
    pub(super) error_log_fallback: crate::error_log::ErrorLogFallback,
}
// ---------------------------------------------------------------------------
// Pipeline worker — owns its own metrics via HasMetrics
// ---------------------------------------------------------------------------

pub(super) struct PipelineWorker {
    pub(super) pipeline_id: crate::pipeline::PipelineId,
    pub(super) metrics: Arc<PipelineMetrics>,
    #[cfg(test)]
    pub(super) serial_test_gate: Option<Arc<tokio::sync::Barrier>>,
}

impl PipelineWorker {
    pub(super) fn from_bound(
        pipeline_id: crate::pipeline::PipelineId,
        blueprint: &crate::pipeline::RuntimeBlueprint,
        registry: &Registry,
    ) -> anyhow::Result<Self> {
        let pipeline = blueprint
            .pipeline_by_id(pipeline_id)
            .ok_or_else(|| anyhow::anyhow!("pipeline id is not in the blueprint"))?;
        Ok(Self {
            pipeline_id,
            metrics: PipelineMetrics::register(registry, &pipeline.name)?,
            #[cfg(test)]
            serial_test_gate: None,
        })
    }
}

impl HasMetrics for PipelineWorker {
    type Stats = PipelineMetrics;
    fn metrics(&self) -> Arc<PipelineMetrics> {
        Arc::clone(&self.metrics)
    }
}

pub(super) async fn run_pipeline_workers(
    mut event_rx: mpsc::Receiver<Event>,
    workers: &[Arc<PipelineWorker>],
    ctx: &PipelineContext,
    input_name: &str,
    input_queue_timer: &crate::metrics::InputQueueTimer,
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

            terminal = shutdown_change_is_terminal(&mut shutdown) => {
                if terminal {
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
                        process_event(
                            &event,
                            workers,
                            ctx,
                            &input_tap_key,
                            input_queue_timer,
                            &mut bump,
                        )
                        .await;
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
        process_event(
            &event,
            workers,
            ctx,
            &input_tap_key,
            input_queue_timer,
            &mut bump,
        )
        .await;
        bump.reset();
    }

    info!("pipeline worker for input '{}' stopped", input_name);
}

pub(super) async fn run_pipeline_with_outputs_inner(
    pipeline: &crate::pipeline::PipelineBlueprint,
    event: &Event,
    ctx: &PipelineContext,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) -> Result<crate::pipeline::PipelineRunResult> {
    // No `--test-pipeline` trace collector on the daemon hot path —
    // passing `None` skips every trace push (and the `format!` /
    // `to_string` work behind it) in `run_pipeline`, since nothing
    // here reads `PipelineRunResult::trace`.
    let mut result = crate::pipeline::run_pipeline_blueprint_resolved_at(
        &ctx.bound_blueprint,
        pipeline,
        event,
        &ctx.funcs,
        Some(&ctx.tap),
        None,
        crate::pipeline::OutputCapturePolicy::DiskOnly(&ctx.disk_outputs),
        bump,
        dispatch_started_at,
    )?;
    enqueue_pipeline_outputs(&pipeline.name, &mut result, ctx).await;
    Ok(result)
}

async fn enqueue_pipeline_outputs(
    pipeline_name: &str,
    result: &mut crate::pipeline::PipelineRunResult,
    ctx: &PipelineContext,
) {
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
                    pipeline_name, output_name, e
                );
                failed_outputs.push((output_name, snapshot));
            }
        } else {
            // Unknown output name slipped past startup validation —
            // bug, but recoverable: treat as an enqueue failure so
            // the event still hits the DLQ.
            error!(
                "pipeline '{}': output '{}' not found",
                pipeline_name, output_name
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
                    pipeline: pipeline_name.to_string(),
                    site: format!("{} enqueue", output_name),
                    reason: reason.clone(),
                    output_name: output_name.clone(),
                    event: snapshot,
                });
        }
    }
}

pub(super) async fn process_event(
    event: &Event,
    workers: &[Arc<PipelineWorker>],
    ctx: &PipelineContext,
    input_tap_key: &str,
    input_queue_timer: &crate::metrics::InputQueueTimer,
    bump: &mut bumpalo::Bump,
) {
    let dispatch_started_at = crate::time::UnixNanos::now();
    process_event_at(
        event,
        workers,
        ctx,
        input_tap_key,
        input_queue_timer,
        bump,
        dispatch_started_at,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_event_at(
    event: &Event,
    workers: &[Arc<PipelineWorker>],
    ctx: &PipelineContext,
    input_tap_key: &str,
    input_queue_timer: &crate::metrics::InputQueueTimer,
    bump: &mut bumpalo::Bump,
    dispatch_started_at: crate::time::UnixNanos,
) {
    input_queue_timer.observe_between(
        crate::time::UnixNanos::from_datetime(event.received_at),
        dispatch_started_at,
    );
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
        #[cfg(test)]
        if let Some(gate) = &worker.serial_test_gate {
            gate.wait().await;
            gate.wait().await;
        }
        worker.metrics.inflight.inc();
        let Some(pipeline) = ctx
            .bound_blueprint
            .blueprint
            .pipeline_by_id(worker.pipeline_id)
        else {
            // A sealed immutable blueprint cannot lose a PipelineId that was
            // assigned to this worker. Keep accounting balanced and fail
            // closed if that internal invariant is ever violated.
            worker.metrics.events_errored.inc();
            error!("pipeline id is not in the runtime blueprint");
            worker.metrics.inflight.dec();
            continue;
        };
        let pipeline_name = pipeline.name.as_str();
        let run_result =
            run_pipeline_with_outputs_inner(pipeline, event, ctx, bump, dispatch_started_at).await;
        match run_result {
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
                                pipeline_name
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
                    pipeline: pipeline_name.to_string(),
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
pub(super) async fn write_errored_to_dlq(
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

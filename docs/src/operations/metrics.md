# Metrics

limpid tracks metrics at every level of the pipeline. Each component counts its own metrics — the runtime only collects and reports them.

## Pipeline metrics

| Metric | Meaning |
|--------|---------|
| `received` | Events entering the pipeline |
| `finished` | Events that reached at least one output |
| `dropped` | Events explicitly discarded by `drop` |
| `discarded` | Events that completed without reaching any output |
| `errored` | Events that failed at any pipeline-side producer site — process / pipeline-skeleton runtime errors and runtime-side output enqueue failures — routed to the [error log](./error-log.md) for inspection and replay |
| `errored_unwritable` | Subset of `errored` where the configured `error_log` write itself failed (disk full / permissions / rotation race) |

**`events_discarded`** is a signal of possible misconfiguration — the event went through the entire pipeline but was never sent anywhere.

**`events_errored`** is the pipeline-side rollup of every event that ended in a Process flavor DLQ record (process body raised, pipeline-skeleton eval failed, explicit `error <expr>`) plus runtime-side output enqueue failures. Sink-side terminal failures (retry exhausted, shutdown drain, batched render failure, OTLP partial-success rejects) are counted separately under the per-output `events_failed`. The original event is preserved in the [error log](./error-log.md) for replay — `jq -c 'select(.kind == "process") | .event' /var/log/limpid/errored.jsonl | limpidctl inject input <name> --json` for Process records, `jq -c 'select(.kind == "output") | .event' ... | limpidctl inject output <name> --json` for Output records (see [Error Log → Replay](./error-log.md#replay)).

**`events_errored_unwritable`** is alarm-level — non-zero means a DLQ write to the configured `error_log` file fell back to the `tracing::error!` channel (disk full, permissions, rotation race). The counter is emitted under two labels that share the metric name and both need to be watched:

- **Pipeline-side** (`limpid_pipeline_events_errored_unwritable_total{pipeline=...}`) — Process-flavor records and the output-enqueue subset of Output-flavor records, both routed through `runtime::write_errored_to_dlq`.
- **Output-side** (`limpid_output_events_errored_unwritable_total{output=...}`) — sink-side Output-flavor DLQ writes (retry exhaustion, shutdown drain, batched render failure, partial-success reject) routed through `modules::route_event_to_dlq`. On a disk queue a sink-side DLQ-write failure also triggers the fail-stop wedge (see `events_wedged` below and the [Outputs disposition contract](../outputs/README.md#disposition-contract)) so the cursor holds for replay on next daemon start.

Investigate the underlying cause before assuming replay coverage is complete; see [Error Log → When the DLQ write itself fails](./error-log.md#when-the-dlq-write-itself-fails).

## Input metrics

| Metric | Meaning |
|--------|---------|
| `received` | Events received from the source (network, socket, file, etc.) — **does not include injected events** |
| `invalid` | Events rejected (invalid PRI header, etc.) |
| `injected` | Events pushed into this input's channel via `limpidctl inject` |

The split between `received` and `injected` keeps "real" traffic distinguishable from synthetic/replay events.

## Output metrics

| Metric | Meaning |
|--------|---------|
| `received` | Total events that entered this output's queue (from pipelines + injects) |
| `injected` | Events pushed into this output's queue via `limpidctl inject` |
| `written` | Events successfully written to the destination |
| `failed` | Events whose final state on this output was a terminal failure. Includes retry-budget exhaustion, per-event render failures on batched outputs' `flush()`, shutdown-drain leftovers when the final flush fails, and — for the OTLP outputs (`otlp_grpc` / `otlp_http`) — the receiver's `partial_success.rejected_log_records`, which are events the server *accepted at the transport layer* but refused at the validation layer (dropped per the [`partial_success` policy](../otlp.md#56-retry-transport-level-only)). |
| `retries` | Total retry attempts across all events |
| `wedged` | Disk-queue fail-stop wedges observed by this output — alarm-level. Non-zero means the consumer stopped accepting new events on this output and will replay from the wedged cursor on next daemon start (see [Outputs disposition contract](../outputs/README.md#disposition-contract)). Only printed by `limpidctl stats` when non-zero. |
| `errored_unwritable` | Sink-side counterpart of the pipeline-side `events_errored_unwritable` — alarm-level. Non-zero means a `route_event_to_dlq` write to `error_log` failed for this output; investigate DLQ file health. Only printed by `limpidctl stats` when non-zero. |

`received - injected` = events delivered via pipelines. `received - written - failed` ≈ events pending in the queue (useful for disk queues).

`events_failed` is the per-output rollup of every terminal sink-side failure (retry exhausted, render failure, shutdown-drain leftover, OTLP `partial_success.rejected_log_records`). Of those, the ones **actually lost** — no durable record anywhere — are only the events on a disk queue whose DLQ file write itself failed and no other channel captured the payload; those show up under `events_wedged` and the disk cursor holds for replay on next start. Events where `control { error_log }` is set and the DLQ file write succeeds are persisted for replay as Output-flavor records (see [Error Log → Producer sites](./error-log.md#producer-sites)). Events where `error_log` is unset take the tracing fallback: `route_event_to_dlq` emits a `tracing::error!` line carrying the full JSONL in an `event_record` structured field, so `journalctl | jq` can extract and replay the record — the disposition still resolves as `Recovered` and the payload is not lost, but this is strictly worse than a dedicated DLQ file (log rotation, aggregation delays, tracing filters), which is why the `--check` recovery-readiness warning (see [Error Log → Recovery readiness check](./error-log.md#recovery-readiness-check---check)) flags the no-`error_log` case at configuration time. Evaluate `events_failed` in combination with the DLQ contents, `events_errored_unwritable`, and `events_wedged`.

## Viewing metrics

### Command line

```bash
# Human-readable table (pipelines first, then inputs and outputs)
sudo limpidctl stats

# JSON (for scripting)
sudo limpidctl stats --json
```

### HTTP (Prometheus)

Run `limpid-prometheus` as a separate process. It queries limpid's control socket and converts JSON stats to Prometheus text exposition format:

```bash
limpid-prometheus --bind 127.0.0.1:9100 --socket /var/run/limpid/control.sock
```

Then configure Prometheus to scrape `http://127.0.0.1:9100/metrics`.

Exposed metrics:

| Metric | Type | Labels |
|--------|------|--------|
| `limpid_pipeline_events_received_total` | counter | `pipeline` |
| `limpid_pipeline_events_finished_total` | counter | `pipeline` |
| `limpid_pipeline_events_dropped_total` | counter | `pipeline` |
| `limpid_pipeline_events_discarded_total` | counter | `pipeline` |
| `limpid_pipeline_events_errored_total` | counter | `pipeline` |
| `limpid_pipeline_events_errored_unwritable_total` | counter | `pipeline` |
| `limpid_input_events_received_total` | counter | `input` |
| `limpid_input_events_invalid_total` | counter | `input` |
| `limpid_input_events_injected_total` | counter | `input` |
| `limpid_output_events_received_total` | counter | `output` |
| `limpid_output_events_injected_total` | counter | `output` |
| `limpid_output_events_written_total` | counter | `output` |
| `limpid_output_events_failed_total` | counter | `output` |
| `limpid_output_retries_total` | counter | `output` |
| `limpid_output_events_wedged_total` | counter | `output` |
| `limpid_output_events_errored_unwritable_total` | counter | `output` |

limpid itself has no Prometheus dependency — the format conversion is entirely `limpid-prometheus`'s job.

## Understanding the numbers

A healthy pipeline looks like:

```
Pipelines:
  main             100 received        95 finished     5 dropped     0 discarded
```

Warning signs:

- **`discarded > 0`** — events are reaching the end of the pipeline without hitting any `output`. Check your routing logic.
- **`failed > 0`** — output writes are failing. Check connectivity to the destination.
- **`retries` growing** — transient failures are occurring. May indicate network instability or destination overload.
- **`received` growing but `finished + dropped` not** — pipeline is backed up (unlikely with async queues, but possible).
- **`output.received > output.written + output.failed`** — events are pending in the queue (expected for disk queues under backpressure).

# Metrics

limpid tracks metrics at every level of the pipeline. Each component counts its own metrics — the runtime only collects and reports them.

## Pipeline metrics

| Metric | Meaning |
|--------|---------|
| `received` | Events entering the pipeline |
| `finished` | Events that reached at least one output |
| `dropped` | Events explicitly discarded by `drop` |
| `discarded` | Events that completed without reaching any output |

**`events_discarded`** is a signal of possible misconfiguration — the event went through the entire pipeline but was never sent anywhere.

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
| `failed` | Events that failed after all retry attempts |
| `retries` | Total retry attempts across all events |

`received - injected` = events delivered via pipelines. `received - written - failed` ≈ events pending in the queue (useful for disk queues).

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
| `limpid_input_events_received_total` | counter | `input` |
| `limpid_input_events_invalid_total` | counter | `input` |
| `limpid_input_events_injected_total` | counter | `input` |
| `limpid_output_events_received_total` | counter | `output` |
| `limpid_output_events_injected_total` | counter | `output` |
| `limpid_output_events_written_total` | counter | `output` |
| `limpid_output_events_failed_total` | counter | `output` |
| `limpid_output_retries_total` | counter | `output` |

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

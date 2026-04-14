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
| `received` | Events received from the source |
| `invalid` | Events rejected (invalid PRI header, etc.) |

## Output metrics

| Metric | Meaning |
|--------|---------|
| `written` | Events successfully written to the destination |
| `failed` | Events that failed after all retry attempts |
| `retries` | Total retry attempts across all events |

## Viewing metrics

### Command line

```bash
# Human-readable table
sudo limpid-tap --stats

# JSON (for scripting)
sudo limpid-tap --stats --json
```

### HTTP (Prometheus)

Run `limpid-prometheus` as a separate process. It queries limpid's control socket and converts JSON stats to Prometheus text exposition format:

```bash
limpid-prometheus --bind 127.0.0.1:9100 --socket /var/run/limpid/control.sock
```

Then configure Prometheus to scrape `http://127.0.0.1:9100/metrics`.

Exposed metrics:

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `limpid_input_events_received_total` | counter | `input` | Input |
| `limpid_input_events_invalid_total` | counter | `input` | Input |
| `limpid_pipeline_events_received_total` | counter | `pipeline` | Pipeline |
| `limpid_pipeline_events_finished_total` | counter | `pipeline` | Pipeline |
| `limpid_pipeline_events_dropped_total` | counter | `pipeline` | Pipeline |
| `limpid_pipeline_events_discarded_total` | counter | `pipeline` | Pipeline |
| `limpid_output_events_written_total` | counter | `output` | Output |
| `limpid_output_events_failed_total` | counter | `output` | Output |
| `limpid_output_retries_total` | counter | `output` | Output |

limpid itself has no Prometheus dependency — the format conversion is entirely `limpid-prometheus`'s job.

## Understanding the numbers

A healthy pipeline looks like:

```
Pipeline: main     100 received    95 finished     5 dropped     0 discarded
```

Warning signs:

- **`discarded > 0`** — events are reaching the end of the pipeline without hitting any `output`. Check your routing logic.
- **`failed > 0`** — output writes are failing. Check connectivity to the destination.
- **`retries` growing** — transient failures are occurring. May indicate network instability or destination overload.
- **`received` growing but `finished + dropped` not** — pipeline is backed up (unlikely with async queues, but possible).

# Metrics

limpid records counters at the input, pipeline, and output boundaries. One
self-describing registry owns the metric families and their fully labelled
series. The daemon exposes a read-only snapshot through the existing `stats`
control command; `limpidctl` and `limpid-prometheus` are consumers of that
snapshot, not alternate metric stores.

The current runtime metrics are counters. Schema v1 also defines gauge and
histogram series so consumers can render any registered family without a list
of hard-coded names.

## Stats schema v1

`limpidctl stats --json` prints the complete control-socket response unchanged.
The top-level object has `schema: 1` and a `metrics` array. Every family carries
its name, type, non-empty help text, and complete series:

```json
{
  "schema": 1,
  "metrics": [
    {
      "name": "limpid_output_events_written_total",
      "type": "counter",
      "help": "Total events successfully written by the output.",
      "series": [
        {"labels": {"output": "sink"}, "value": 42}
      ]
    },
    {
      "name": "example_queue_depth",
      "type": "gauge",
      "help": "Current example queue depth.",
      "series": [
        {"labels": {"queue": "primary"}, "value": 3}
      ]
    },
    {
      "name": "example_latency_seconds",
      "type": "histogram",
      "help": "Example operation latency.",
      "series": [
        {
          "labels": {"route": "west"},
          "buckets": [[0.1, 7], [0.5, 11]],
          "sum": 2.75,
          "count": 12
        }
      ]
    }
  ]
}
```

Counter and gauge series contain `labels` and `value`. Histogram `buckets` are
finite upper-bound/count pairs. Counts are cumulative and inclusive (`<=`), so
bounds must be strictly increasing and counts must be nondecreasing. An empty
bucket list is valid. `count` includes every observation, including values above
the last finite bound; `sum` is the accumulated observation sum.

Schema v1 deliberately does not contain an explicit `+Inf` bucket. A
Prometheus consumer derives `le="+Inf"` from `count`. Bucket, sum, and count
atomics are loaded independently, so a concurrent snapshot may transiently show
the last finite bucket above `count`; consumers must not clamp or reject that
state. Array order is not a data contract.

Registration fixes labels on each handle. Counter increments, gauge sets, and
histogram observations therefore do not accept labels and do not perform a
label lookup, lock, or allocation on their update path. Invalid metric names,
missing help, duplicate label names, invalid histogram boundaries, duplicate
series, or conflicting family metadata fail registration and propagate through
daemon startup instead of being ignored.

## Current metric families

These are the current metric families registered by the daemon. Each series
has exactly the fixed label shown; the label value is the configured component
name, so label cardinality is bounded by the configured components.

### Pipelines

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_pipeline_events_received_total` | `pipeline` | Events entering the pipeline. |
| `limpid_pipeline_events_finished_total` | `pipeline` | Events that reached at least one output. |
| `limpid_pipeline_events_dropped_total` | `pipeline` | Events explicitly discarded by `drop`. |
| `limpid_pipeline_events_discarded_total` | `pipeline` | Events that completed without reaching any output. |
| `limpid_pipeline_events_errored_total` | `pipeline` | Events that failed at a pipeline-side producer site and were routed to the [error log](./error-log.md). |
| `limpid_pipeline_events_errored_unwritable_total` | `pipeline` | Pipeline-side error-log writes that failed. |
| `limpid_pipeline_inflight` | `pipeline` | Pipeline executions currently in progress, including terminal bookkeeping. |

`events_discarded` is a possible routing-misconfiguration signal: the event
completed the pipeline but was never sent anywhere.

`events_errored` is the pipeline-side rollup of Process-flavour DLQ records
(process body errors, pipeline-skeleton evaluation failures, and explicit
`error <expr>`) plus runtime-side output enqueue failures. Sink-side terminal
failures are counted under the corresponding output's `events_failed`. The
original event is preserved when the configured error-log write succeeds; see
[Error Log → Replay](./error-log.md#replay).

### Inputs

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_input_events_received_total` | `input` | Events received from the source; injected events are excluded. |
| `limpid_input_events_invalid_total` | `input` | Events rejected by the input parser or protocol boundary. |
| `limpid_input_events_injected_total` | `input` | Events pushed into the input through `limpidctl inject`. |
| `limpid_input_bytes_received_total` | `input` | Logical bytes received by the input adapter before validation. |

Keeping `received` and `injected` separate makes source traffic distinguishable
from synthetic and replay traffic.

### Outputs

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_output_events_received_total` | `output` | Events entering the output queue from pipelines and injection. |
| `limpid_output_events_injected_total` | `output` | Events injected directly into the output queue. |
| `limpid_output_events_written_total` | `output` | Events successfully written to the destination. |
| `limpid_output_events_failed_total` | `output` | Events that reached a terminal failure disposition for this output. |
| `limpid_output_retries_total` | `output` | Retry attempts across all events. |
| `limpid_output_events_wedged_total` | `output` | Disk-queue fail-stop wedges observed by the output. |
| `limpid_output_events_errored_unwritable_total` | `output` | Sink-side error-log writes that failed. |
| `limpid_output_bytes_written_total` | `output` | Logical bytes whose transfer to the destination was confirmed. |
| `limpid_output_queue_depth` | `output` | Current unread or unacknowledged output queue depth. |
| `limpid_output_in_retry` | `output` | Whether an output retry cycle is active (`0` or `1`). |

`events_failed` includes retry-budget exhaustion, per-event render failures in
batched output flushes, shutdown-drain leftovers after a final flush failure,
and OTLP `partial_success.rejected_log_records`. Evaluate it with the DLQ file,
`events_errored_unwritable`, and `events_wedged`:

- When the DLQ write succeeds, the Output-flavour record is recoverable.
- When a disk queue's DLQ write fails, the fail-stop wedge holds the cursor for
  replay after the operator restores DLQ health.
- When a memory queue's DLQ write fails, there is no durable cursor and the
  event is lost.

See the [output disposition contract](../outputs/README.md#disposition-contract)
and [Error Log → When the DLQ write itself fails](./error-log.md#when-the-dlq-write-itself-fails).

Useful relationships are approximate during concurrent updates:

- `output.received - output.injected` is traffic delivered by pipelines.
- `output.received - output.written - output.failed` approximates events still
  pending in the queue.

### Runtime gauges

`limpid_output_queue_depth{output}` reports the memory receiver's current
length for memory queues. For disk queues it reports the saturating difference
between the current write-segment sequence and acknowledged-segment sequence.
The disk value therefore tracks durable segment progress rather than an exact
event count. Both backends publish zero when their consumer terminates.

`limpid_output_in_retry{output}` changes to one after the first failed attempt
enters a retry cycle. It returns to zero when an attempt succeeds, the retry
budget is exhausted, or shutdown interrupts the cycle. An output that observes
shutdown before making an attempt remains zero.

`limpid_pipeline_inflight{pipeline}` increments immediately before pipeline
execution and decrements only after its terminal counters and any error-log
work have completed. It therefore includes executions waiting on terminal
bookkeeping, not only expression evaluation.

Gauge updates and snapshot reads are concurrent relaxed atomic operations. A
snapshot is a useful point-in-time observation of each series, but values from
different series are not loaded as one transaction and need not describe the
same instant.

### Byte-counter boundary

The byte counters measure logical buffers at adapter boundaries, not physical
wire traffic. They exclude transport overhead such as TCP/IP, TLS, HTTP/2, and
Kafka protocol framing. They include framing, line feeds, compression, or
serialization only when that is part of the exact buffer the adapter receives
or hands to its transport.

`limpid_input_bytes_received_total{input}` counts a logical input buffer before
validation, so invalid input is included and an empty buffer adds zero. A
partially read tail record is counted once when it becomes complete; rewinding
or rereading it does not count it again. For structured sources without an
equivalent raw adapter buffer, OTLP gRPC uses the decoded request's canonical
protobuf encoded length, and journal input uses the length of the generated
JSON buffer.

`limpid_output_bytes_written_total{output}` counts the complete prepared body
only when the adapter can confirm its transfer to the transport. This includes
an HTTP non-success response and an OTLP partial rejection because the complete
request body was transferred. A connection or send failure without transfer
confirmation adds zero. A retry counts a confirmed attempt once, including
during shutdown, and the byte count is never apportioned to the accepted subset
of a batch. File, standard output, Unix-socket, and syslog counters include any
line feed or header added to the handed-off buffer; syslog UDP uses the length
confirmed by `send`.

## Viewing metrics with limpidctl

```bash
# Operator-focused table: Pipelines, Inputs, then Outputs
sudo limpidctl stats

# Generic human view of every schema-v1 family and series
sudo limpidctl stats --details

# Complete raw control-socket response, byte-for-byte
sudo limpidctl stats --json
```

The default table preserves the established 16-counter operator view. Component
names are sorted and alarm fields such as `wedged` and `errored_unwritable` are
shown under their existing conditional rules. Unknown future families are
ignored in this mode. If a known family is missing, duplicated, has the wrong
type or value, or does not have exactly its fixed label, limpidctl prints the
raw response rather than inventing a zero or a partial table.

`--details` replaces the default table with a generic view. Families are sorted
by metric name, series by their canonical label key/value tuples, and labels by
key. Every family shows its name, type, help, complete labels, and values.
Histograms show the finite cumulative buckets stored in schema v1 plus `sum`
and `count`; they do not synthesize `+Inf` in this human view.

`--json` bypasses parsing and formatting. It cannot be combined with
`--details`. Invalid JSON, unsupported schemas, and malformed schema-v1 data
retain the existing successful raw-response fallback for the human modes.

The deterministic human ordering is for reproducibility and testing only. It
does not assign display or query semantics to a metric, series, or dashboard.

## Prometheus exposition

Run the sidecar against limpid's control socket:

```bash
limpid-prometheus --bind 127.0.0.1:9100 \
  --socket /var/run/limpid/control.sock
```

Prometheus can then scrape `http://127.0.0.1:9100/metrics`. The exporter reads a
complete schema-v1 snapshot and emits Prometheus text exposition format 0.0.4:

- Families are sorted by metric name; `# HELP` precedes `# TYPE`, followed by
  samples.
- Series are sorted by canonical source-label key/value tuples, and rendered
  labels are sorted by key.
- HELP backslashes and newlines, and label-value backslashes, double quotes,
  and newlines, are escaped for text format 0.0.4.
- Counters and gauges emit the schema `value` directly.
- A histogram named `n` emits finite cumulative `n_bucket` samples, an implicit
  `n_bucket{le="+Inf"}` whose value is `count`, then `n_sum` and `n_count`.

This ordering is a reproducible exposition surface only; it has no PromQL or
Grafana ordering meaning.

The translator validates the entire snapshot before exposing any samples. It
rejects malformed or unsupported families, duplicate family names, inconsistent
label-name sets within a family, duplicate source or mapped labelsets, invalid
histogram sequences, and collisions between a declared family name and another
histogram's derived `_bucket`, `_sum`, or `_count` name. A failure produces the
existing `# error: ...` response body instead of partial exposition.

### Prometheus names and histogram `le`

The schema registry's validation policy and Prometheus's unquoted text-format
identifier policy are separate boundaries. The sidecar requires exported
metric names to match `[A-Za-z_:][A-Za-z0-9_:]*`. Source label names must match
`[A-Za-z_][A-Za-z0-9_]*` and must not start with the Prometheus-reserved `__`
prefix. It rejects invalid names; it does not normalize or quote them. These
sidecar checks do not add label-name or reserved-name validation to the core
registry.

Prometheus histogram buckets need the special `le` label. When a histogram
source series already contains exact key `le`, the sidecar shifts the complete
source underscore chain injectively before rendering:

| Schema source key | Prometheus source-label key |
| --- | --- |
| `le` | `le_` |
| `le_` | `le__` |
| `le__` | `le___` |

The generated bucket boundary keeps exact key `le`. The same shifted source
labels are used on finite and `+Inf` bucket, sum, and count samples. The shift is
conditional: a histogram without source `le` keeps an existing `le_` unchanged,
and counter or gauge labels named `le` are never shifted.

limpid itself has no Prometheus dependency; validation, collision handling, and
text conversion above belong entirely to `limpid-prometheus`.

## Operational scrape checkpoint

Prometheus defaults to a one-minute scrape interval and a 10-second scrape
timeout. The release checkpoint therefore measures sequential end-to-end
scrapes for stable per-scrape observations; it is not a load, concurrency, or
scrapes-per-second benchmark.

The canonical harness runs the real sidecar through both production boundaries
(schema-v1 control socket and HTTP `/metrics`) at three deterministic payload
scales:

| Profile | Families | Series per family | Histogram finite buckets |
| --- | ---: | ---: | ---: |
| P0 | current 16 counters | 1 | none |
| P1 | 48 (16 each counter/gauge/histogram) | 8 | 8 |
| P2 | 96 (32 each counter/gauge/histogram) | 32 | 16 |

For each profile it parses text format 0.0.4 and requires complete ordered
semantic equivalence with the source snapshot: HELP, TYPE, sample namespaces,
labels, values, histogram buckets, sum, and count must all match. It records
response bytes, sample count, median/p95/max end-to-end latency, process CPU
time, and peak RSS when the platform can report it. It also reports the maximum
latency as a fraction of and margin to the 10-second timeout. Correctness is
mandatory and any scaling cliff is reviewed; there is no arbitrary throughput
or latency threshold. A separate one-request P0 smoke uses a real daemon control
socket as an integration check. Machine-specific results remain in the
canonical harness receipt and PR evidence, not in this product contract.

## Interpreting the counters

A healthy pipeline table might look like:

```text
Pipelines:
  main             100 received        95 finished     5 dropped     0 discarded
```

Investigate these signals:

- `discarded > 0`: an event completed without reaching an output; check routing.
- `failed > 0`: output events reached terminal failure; inspect destination and
  recovery disposition.
- `retries` growing: transient destination failures are occurring.
- A sustained gap between `received` and
  `finished + dropped + discarded + errored` may mean pipeline work is in flight
  or backed up.
- `output.received > output.written + output.failed`: events remain pending,
  commonly under disk-queue backpressure.
- `errored_unwritable > 0` or `wedged > 0`: treat as an alarm and restore DLQ or
  disk-queue health before assuming replay coverage is complete.

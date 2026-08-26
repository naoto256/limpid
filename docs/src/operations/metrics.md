# Metrics

limpid records counters at the input, pipeline, and output boundaries. One
self-describing registry owns the metric families and their fully labelled
series. The daemon exposes a read-only snapshot through the existing `stats`
control command; `limpidctl` and `limpid-prometheus` are consumers of that
snapshot, not alternate metric stores.

The current runtime metrics are counters and gauges. Schema v1 also defines
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

Consumers ignore unknown fields at every level of the schema-v1 envelope,
metric family, and value or histogram series so additive metadata remains
forward-compatible. The documented fields above remain required and retain
their exact types; ignoring an unknown field does not make a missing or
malformed required field valid.

The shared schema-v1 crate also defines the well-known dropped hierarchy,
process family and label names, and the `/`-separated process-path relationship
used by current consumers. These definitions do not add fields to the wire
envelope.

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

These are the current metric families registered by the daemon. Their labels
are fixed from the compiled configuration at registration time, so cardinality
is bounded by that configuration. Process-family cardinality follows distinct
compiled invocation paths rather than only process definition count.

### Build information

`limpid_build_info{version,node_id}` is a gauge with value `1`. `version` is
the running limpid package version. `node_id` is the explicit top-level
configuration value when set; otherwise it is the host name resolved once at
startup. The series is registered before the control server starts and remains
stable for that runtime. See [Node identity](../configuration.md#node-identity)
for the same-host multi-instance deployment caveat.

### Pipelines

The input-queue boundary is measured before pipeline execution:

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_input_queue_wait_seconds` | `input` | Local input arrival to pipeline dispatch start after the event is dequeued. |
| `limpid_input_queue_wait_negative_delta_total` | `input` | Input-queue wait durations clamped to zero after a wall-clock reversal. |

Every dequeued event records exactly one input-queue observation, including
events consumed while draining after shutdown begins. Its finite bucket bounds
are `0.0001`, `0.001`, `0.005`, `0.025`, `0.1`, `0.5`, `2.5`, and `10`
seconds. If the wall clock moves backward between local input arrival and
dispatch start, the duration is clamped to zero and
`limpid_input_queue_wait_negative_delta_total` increments.

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_pipeline_events_received_total` | `pipeline` | Events entering the pipeline. |
| `limpid_pipeline_events_finished_total` | `pipeline` | Events that reached at least one output. |
| `limpid_pipeline_events_discarded_total` | `pipeline` | Events that completed without reaching any output. |
| `limpid_pipeline_events_errored_total` | `pipeline` | Events that failed at a pipeline-side producer site and were routed to the [error log](./error-log.md). |
| `limpid_pipeline_events_errored_unwritable_total` | `pipeline` | Pipeline-side error-log writes that failed. |
| `limpid_pipeline_inflight` | `pipeline` | Pipeline executions currently in progress, including terminal bookkeeping. |
| `limpid_pipeline_processing_seconds` | `pipeline`, `output` | Pipeline dispatch start to the emission of that output statement's event snapshot. |
| `limpid_pipeline_processing_negative_delta_total` | `pipeline`, `output` | Processing durations clamped to zero after a wall-clock reversal. |

`events_discarded` is a possible routing-misconfiguration signal: the event
completed the pipeline but was never sent anywhere.

`events_errored` is the pipeline-side rollup of Process-flavour DLQ records
(process body errors, pipeline-skeleton evaluation failures, and explicit
`error <expr>`) plus runtime-side output enqueue failures. Sink-side terminal
failures are counted under the corresponding output's `events_failed`. The
original event is preserved when the configured error-log write succeeds; see
[Error Log → Replay](./error-log.md#replay).

`limpid_pipeline_processing_seconds` is registered once for every configured
`pipeline`/`output` pair. Each Output statement observes the interval from the
event's single dispatch-start timestamp to that statement's snapshot. The same
dispatch timestamp is shared by every configured pipeline handling that event,
so input taps and time spent in earlier serial fan-out pipelines belong to the
pipeline stage rather than creating a gap between stages. Repeated calls to the
same output share one series, while fan-out outputs observe independent
snapshots. Reaching an Output statement therefore adds one processing
observation, including each branch of a fan-out. Its finite bucket bounds are
`0.0001`, `0.001`, `0.005`, `0.025`,
`0.1`, `0.5`, `2.5`, and `10` seconds.
If the wall clock moves backward between dispatch start and output emission, the
duration is clamped to zero and
`limpid_pipeline_processing_negative_delta_total` increments.

### Process invocations

The three process-only counter families use the labels `pipeline`, `step`,
`process_path`, and `process_name`:

- `limpid_process_events_in_total` counts frame entry.
- `limpid_process_events_out_total` counts frames that return `Continue`.
- `limpid_process_events_errored_total` counts frames terminated by an error.

These are invocation counters, not event counters. `step` is the root process
site's one-based, pipeline-wide source-order position; nested calls share that
root step. `process_path` is a `/`-separated invocation hierarchy whose leaf is
`process_name`: a named root has a path such as `/dispatch`, a nested call
extends it to `/dispatch/leaf`, and an inline root uses `/(inline)`. Process
call graphs must be acyclic, so every invocation path is finite and known at
config-load time. Each distinct compiled invocation node owns three
process-only counter series; reusing a helper under different parents creates
distinct path series. Every configured series is prepopulated with zero.

Each frame records exactly one terminal result, so for an individual series
`in = out + dropped + errored`. A nested drop propagates through its active
caller frames. A nested error counts as errored for that frame even when a
caller's catch block recovers and the caller returns normally. Consequently,
summing process series double-counts nested invocations and is not an event
flow total.

### Dropped-event hierarchy

`limpid_events_dropped_total` is one counter family for both the pipeline
frame and its process frames. Every series has the labels `pipeline`, `step`,
`process_path`, and `process_name`. The pipeline frame is the root node and is
represented by `step="0"`, `process_path="/"`, and an empty `process_name`.
Its value is the number of events dropped from that pipeline. Process nodes
use their ordinary one-based root step and invocation path. Their values count
drops that propagated through that process frame.

A drop therefore increments one finite path from the node that executed
`drop` through every active caller to the pipeline root. Process call graphs
are acyclic, so this hierarchy is fully known at config-load time. The source
family exposes propagated totals at every node; its root is the authoritative
pipeline dropped-event total.

`limpid-prometheus` additionally synthesizes the counter
`limpid_events_dropped_own_total` at scrape time. It has the same
`pipeline`, `step`, `process_path`, and `process_name` labels as the source
dropped family, with help text `Total events dropped directly at this
processing node, excluding direct child drops.` For each source series it
reports `max(0, parent - sum(direct children))`. A root process is a direct
child of `/` when its `pipeline` matches; its one-based `step` identifies the
root call site and does not have to match the root's reserved step `0`. Below a
process node, a direct child's `process_path` is the parent path extended by
`/` and exactly one non-empty segment, and all labels other than
`process_path` and `process_name` must be equal. Missing intermediate paths are
not bridged. The clamp accommodates independently read counter snapshots and
arithmetic overflow without changing or rejecting the source family. If the
source dropped family is absent, the derived family is absent too.

The derived value at `/` counts drops executed directly in the pipeline body.
At a process node it counts drops executed directly in that process body.

Only dropped frames propagate through every active caller, making direct-child
subtraction meaningful. Continue and error outcomes do not have that
propagation invariant, so no corresponding `own` families are synthesized.
The derived family is a sidecar exposition view rather than another daemon
registry series; `limpid_events_dropped_total` remains authoritative.

### Inputs

| Metric | Label | Meaning |
| --- | --- | --- |
| `limpid_input_events_received_total` | `input` | Events received from the source; injected events are excluded. |
| `limpid_input_events_invalid_total` | `input` | Events rejected by the input parser or protocol boundary. |
| `limpid_input_events_injected_total` | `input` | Events pushed into the input through `limpidctl inject`. |
| `limpid_input_bytes_received_total` | `input` | Logical bytes received by the input adapter before validation. |

Keeping `received` and `injected` separate makes source traffic distinguishable
from synthetic and replay traffic.

### LTP

LTP series are registered at startup from the deduplicated union of configured
input and output peers. The `peer` label therefore comes from authenticated or
declared configuration, never from untrusted wire metadata, and every series
exists at zero before traffic arrives.

| Metric | Labels | Meaning |
| --- | --- | --- |
| `limpid_ltp_hop_latency_seconds` | `peer`, `segment` | Hop latency histogram. `segment` is `network` or `intra`. |
| `limpid_ltp_negative_delta_total` | `peer` | Negative cross-host latency deltas clamped to zero. |
| `limpid_ltp_loop_dropped_total` | `peer` | Events dropped because the incoming history contains this node or has reached `max_hops`. |
| `limpid_ltp_rejected_unknown_peer_total` | *(none)* | Connection attempts rejected for an undeclared public key or a hello node identity that does not match its authenticated key. |

`network` is observed by the receiving input after cycle and hop-limit checks:
the previous hop's nonzero departure to the local arrival. Empty history and an
unsealed departure of zero are skipped. A negative wall-clock delta is observed
as zero and increments `limpid_ltp_negative_delta_total` for the authenticated
upstream peer.

`intra` is observed once, only after an output's final event write and flush
succeeds: the event's persisted local arrival to that successful attempt's
departure, labeled with the declared destination peer. Failed attempts and
retries do not add observations. The histogram uses the fixed upper bounds
`0.0001`, `0.001`, `0.005`, `0.025`, `0.1`, `0.5`, `2.5`, and `10` seconds as
the schema-v1 registry's eight finite bounds. The Prometheus exporter derives
the `+Inf` bucket from `count`; `limpidctl stats --json` contains no explicit
`+Inf` bucket.

An undeclared SPKI is counted at the raw-public-key verifier, and a declared key
with the wrong hello identity is counted after the handshake. TLS timeouts,
malformed handshakes, cipher failures, X.509 certificates, and intermediate
certificates are not classified as unknown peers.

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
| `limpid_output_delivery_seconds` | `output` | Output emission or direct injection to confirmed delivery. |
| `limpid_output_delivery_negative_delta_total` | `output` | Delivery durations clamped to zero after a wall-clock reversal. |

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

`limpid_output_delivery_seconds` starts at the per-output statement snapshot,
or immediately before a direct output injection enters the queue. It includes
remaining pipeline work, enqueue blocking, memory or disk queue residence,
batching, retries, transport work, and replay after restart. An observation is
added only when the queue acknowledgement resolves as Delivered; Recovered,
Dropped, wedge, and enqueue-failure paths do not contribute. This is therefore
a delivered-event latency distribution, not a success-rate denominator. Its
finite bucket bounds are `0.001`, `0.005`, `0.025`, `0.1`, `0.5`, `2.5`, `10`,
`30`, `60`, `300`, `900`, and `3600` seconds.
If the wall clock moves backward between emission and delivery, including
across a daemon restart, the duration is clamped to zero and
`limpid_output_delivery_negative_delta_total` increments.

Each event resolved as Delivered adds one delivery observation. Batched
outputs share a single delivery-time sample for the accepted prefix, but every
Delivered event still increments the histogram count. Together with the
per-Output processing observation above, telemetry work scales proportionally
with fan-out.

> **Upgrade note:** The disk output-queue record format changed with these
> latency boundaries. Drain disk-backed output queues before stopping the old
> daemon and upgrading. Older queued records do not contain the required
> `emitted_ns` field; the new daemon rejects them and emits the existing
> corrupted-record warning rather than mixing incompatible latency samples.

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
# Operator-focused table: Pipelines, Inputs, Outputs, then Processes when present
sudo limpidctl stats

# Expanded human view with process identity and structured metric families
sudo limpidctl stats --details

# Complete legacy generic family-and-series text
sudo limpidctl stats --raw

# Complete schema-v1 control-socket response, byte-for-byte
sudo limpidctl stats --json
```

The default table preserves the established component-counter view and appends
a `Processes` invocation table only when all four process families pass their
strict validation. Process rows sort by pipeline, numeric step, path, and name.
Component names are sorted and alarm fields such as `wedged` and
`errored_unwritable` are shown under their existing conditional rules. Unknown
future families are ignored in this mode. If the response fails wire, schema,
or base canonical-family validation, the human modes print the raw response
rather than inventing a zero or a partial table.

`--details` expands the human view. When strict process validation succeeds, it
keeps the default component tables and process tree, adds each process's
numeric `step` and `process_path` beneath its row, and presents every metric
family in a structured `Metrics` section. Families are sorted by metric name,
series by their canonical label key/value tuples, and labels by key. Counters
and gauges show their labels and values; histograms show the finite cumulative
buckets stored in schema v1 plus `sum` and `count`. This view does not
synthesize `+Inf`. A DTO-valid process-only semantic defect or incomplete
process-family set instead preserves the base component summary, omits
`Processes`, and retains each process family in `Metrics` with
`series: unavailable as a process summary; use --raw`.

`--raw` prints the complete legacy generic family-and-series text. It retains
each family's name, type, help, complete labels, and value or histogram data,
including process labels that the human views organize into the process tree.

`--json` bypasses parsing and formatting. It cannot be combined with
`--details` or `--raw`; the three options are mutually exclusive. Wire,
schema, and base canonical-family validation failures retain the existing
successful raw-response fallback for the human modes.

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

### Scrape every limpid node

Bind each exporter to a management address reachable by Prometheus, and apply
the same network-access policy used for the node's other management services.
Do not expose the exporter to an untrusted network. A three-node forwarding
topology can use one job:

```yaml
scrape_configs:
  - job_name: limpid
    static_configs:
      - targets:
          - sender-a.example.com:9100
          - sender-b.example.com:9100
          - receiver.example.com:9100
```

Scraping only a receiver gives an incomplete view. In an LTP topology the
receiver observes the network hop, while each sender observes its local
intra-daemon hop and output-delivery latency. Pipeline processing latency is
also local to the daemon that runs the pipeline. The dashboard remains
portable across one or several jobs; its `job` and `instance` variables are
derived from `limpid_build_info` rather than fixed deployment names.

After Prometheus reloads the checked configuration, verify all expected nodes
and all three local latency boundaries:

```promql
count(up{job="limpid"} == 1)
count(limpid_build_info{job="limpid"})
sum by (instance) (limpid_input_queue_wait_seconds_count{job="limpid"})
sum by (instance) (limpid_pipeline_processing_seconds_count{job="limpid"})
sum by (instance) (limpid_output_delivery_seconds_count{job="limpid"})
sum by (instance, segment) (limpid_ltp_hop_latency_seconds_count{job="limpid"})
```

The expected node count is deployment-specific. A zero histogram count is a
valid pre-registered series before traffic; a missing node or missing family is
not equivalent to zero.

### Import the dashboard and alert rules

The `limpid-prometheus` package installs these operator assets:

- `/usr/share/limpid/grafana/limpid-dashboard.json`
- `/usr/share/limpid/grafana/limpid-alerts.yaml`

For an interactive Grafana import, open **Dashboards → New → Import**, upload
`limpid-dashboard.json`, and map its Prometheus input to the deployment's
Prometheus datasource. The stable dashboard UID is `limpid-health-flow`, so an
update replaces that dashboard instead of creating another copy.

For file provisioning, render `${DS_PROMETHEUS}` in the JSON to the provisioned
Prometheus datasource UID, remove the top-level `__inputs` import metadata, and
place the rendered JSON under a Grafana dashboard provider's configured path.
Validate the resulting dashboard has the intended datasource before restarting
or reloading Grafana.

Before deploying the alert rules, validate them with the same Prometheus
version that will load them:

```bash
promtool check rules limpid-alerts.yaml
```

Copy the checked file into the Prometheus server's configuration-managed rules
directory, add that path to `rule_files`, validate the complete Prometheus
configuration, and use the deployment's supported reload mechanism. The rules
cover output wedges, unwritable pipeline/output recovery records, and a
persistent non-zero output backlog. They intentionally do not define alert
routing or receivers.

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

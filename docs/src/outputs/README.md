# Outputs

Output modules write processed events to external destinations.

## Available types

| Type | Description |
|------|-------------|
| [`file`](./file.md) | Local file with dynamic path templates |
| [`http`](./http.md) | HTTP/HTTPS endpoint (Elasticsearch, Splunk HEC, etc.); per-peer TLS / mTLS, round-robin across peers |
| [`kafka`](./kafka.md) | Apache Kafka topic with optional TLS / mTLS / SASL (requires `--features kafka`) |
| [`syslog_tcp`](./syslog-tcp.md) | Syslog TCP with persistent per-peer connections; per-peer TLS / mTLS optional |
| [`syslog_udp`](./syslog-udp.md) | Syslog UDP datagrams |
| [`unix_socket`](./unix-socket.md) | Unix stream socket |
| [`stdout`](./stdout.md) | Standard output (debugging) |
| [`otlp_http`](./otlp_http.md) | OTLP/HTTP logs sender (`http_protobuf` / `http_json`), per-peer TLS / mTLS |
| [`otlp_grpc`](./otlp_grpc.md) | OTLP/gRPC logs sender, per-peer TLS / mTLS |
| [`ltp`](../ltp.md) | LTP node-to-node transport (mutual TLS 1.3 with raw public keys); see [LTP protocol notes](../ltp.md) |

## Queue and retry

Every output has an async queue that decouples pipeline processing from I/O. You can configure the queue and retry behavior:

```limpid
def output reliable {
    type syslog_tcp
    peer { host "10.0.0.1" port 514 }

    queue {
        type disk                          // memory (default) | disk
        path "/var/lib/limpid/queues/out"  // required for disk queue
        max_size "1GB"                     // optional (default: unlimited)
        capacity 65536                     // channel buffer size (default: 65536)
    }

    retry {
        max_attempts 10                    // default: 5
        initial_wait "1s"                  // default: 1s
        max_wait "5m"                      // default: 60s
        backoff exponential                // exponential (default) | fixed
    }
}
```

`retry` is accepted by every output type. Retry-exhausted payloads are persisted to `control { error_log "..." }` (see [Recovery (error_log)](#recovery-error_log) below) when configured. When `error_log` is unset, the runtime emits a one-line `tracing::error!` summary per failure with no payload (a confidentiality-preserving default) and the event resolves as `Recovered` — `events_failed` counts every terminal failure once, disk vs memory queue agnostic, via `resolve_ack_from_dlq_outcome`. An operator who wants payload metadata or the full JSONL on the tracing side opts in explicitly via `control { error_log_fallback "meta" | "full" }`; that setting only takes effect when `error_log` is also configured (see [Recovery (error_log)](#recovery-error_log) for the ladder).

### Disposition contract

Every event handed to an output resolves to one of three dispositions:

| Disposition | Meaning | Disk queue cursor | Memory queue cursor | Metrics |
|-------------|---------|-------------------|---------------------|---------|
| `Delivered` | Sink confirmed the send. | advances | advances | `events_written++` |
| `Recovered` | Send failed, and either the failure record was durably written to `error_log`, or `error_log` is unset so the operator has declared no durable recovery is required (the tracing fallback runs per the `error_log_fallback` ladder and is best-effort, not load-bearing). | advances | advances | `events_failed++` |
| `Dropped` | Send failed *and* no durable failure record was written (configured DLQ-file write failure, bug / panic in the sink, runtime task abort past shutdown budget, or an ambiguous shutdown-drain failure where the wire state cannot be proved and a `Recovered` disposition would fabricate at-least-once). | **holds — fail-stop wedge; consumer stops accepting new events and replays on next daemon start** | advances (memory queues cannot replay) | `events_failed++`, `events_wedged++` on disk queues |

The **fail-stop wedge** on disk queues is intentional. Holding the cursor guarantees that no event is silently lost on a durable queue; the trade-off is that the affected output's pipeline halts until an operator investigates and restarts the daemon. Both **unbatched sinks** (`file`, `stdout`, `unix_socket`, `syslog_tcp`, `syslog_udp`, `kafka`) and **batched sinks** (`http`, `otlp_http`, `otlp_grpc`) exit the wedge cleanly — unbatched sinks resolve each ack synchronously inside `consume`, so `in-flight == 0` at wedge time; batched sinks take a separate wedge-exit path that resolves parked buffer entries as ambiguous DLQ records without attempting any further send. See [Error Log → Disposition contract and fail-stop wedge on disk queues](../operations/error-log.md#disposition-contract-and-fail-stop-wedge-on-disk-queues) for the operator runbook.

### Memory queue (default)

Fast, but events are lost on process restart.

### Disk queue

Events are persisted to a Write-Ahead Log (WAL) on disk. Survives process restarts.

- Segments are rotated at 16 MiB
- `max_size` limits total disk usage (oldest consumed segments are deleted)
- Cursor position is saved atomically

### Recovery (error_log)

The daemon-wide [`control { error_log "..." }`](../operations/error-log.md) block names a JSONL file that catches payloads the queue/retry chain could not place. Three sink-side paths feed it as Output-flavor records (`kind: "output"`):

- the output's `retry { ... }` budget was exhausted,
- a batched output (`http`, `otlp_http`, `otlp_grpc`) failed to flush its remaining buffer during **graceful shutdown** (one record per still-parked event; see the shutdown caveat below), or exited through the fail-stop wedge with parked buffer entries (one ambiguous-disposition record per parked event),
- the runtime could not hand an event to the output's queue at the pipeline → output boundary (queue closed, disk-queue write error, unknown output).

Each line is one per-event record carrying `event.egress` — the **pipeline-produced payload** at the moment it was handed to the sink (`egress` starts as a clone of `ingress` and is overwritten as the pipeline's process bodies run). Replay via `limpidctl inject output <name> --json` re-injects the event into the named sink's `consume()` path — the pipeline is **bypassed**, but the sink's own transport-level rendering (batched encode, HTTP body framing, OTLP `ResourceLogs` packing, etc.) **does** re-run. See [Error Log → Output flavor](../operations/error-log.md#output-flavor) for the full record shape and producer-site catalog.

When `error_log` is unset, Output-flavor records fall back to a one-line `tracing::error!` summary (site + reason, no payload) and the event resolves as `Recovered`. The payload is not persisted anywhere by default — operators who want the metadata or the full JSONL on the tracing side must explicitly opt in with `control { error_log_fallback "meta" | "full" }`, and that opt-in only takes effect when `error_log` is also configured (see the ladder table below). `limpid --check` emits a recovery-readiness warning so operators notice the missing configuration before the first failure, plus a separate warning when `error_log_fallback` is set while `error_log` is unset (the fallback is inert in that combination).

**`error_log_fallback` ladder.** The tracing fallback line carries different fields depending on the operator's confidentiality choice. All rows preserve the same ack disposition — only the tracing emission differs.

| `error_log` | `error_log_fallback` | tracing line body                                                                              |
|-------------|----------------------|------------------------------------------------------------------------------------------------|
| unset       | (any — value ignored)| one-line summary (site + reason), no payload, no metadata                                      |
| set         | unset / `"off"`      | one-line summary; `error_log` write-failure noted                                              |
| set         | `"meta"`             | structured metadata (`kind`, `fallback`, `reason`, `timestamp`, `size`, `position`); no payload |
| set         | `"full"`             | `event_record = <full JSONL>` — payload may reach journald / log aggregation                    |

The `"full"` value restores the pre-0.7.9 shape (full JSONL on the tracing line) for operators who want a journald-based recovery trail; it exposes the pipeline egress bytes to the tracing subscriber, so treat it as an opt-in for environments where the journald boundary is trusted.

> **Graceful shutdown vs. SIGKILL.** The shutdown-drain path above
> only fires on **graceful** shutdown — `SIGTERM`, `SIGHUP` reload,
> `systemctl stop`, or an explicit `shutdown()` call. The bounded
> final drain runs one attempt per parked payload with a 3-second
> `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` (plus the per-actor in-flight
> cancel). Drain failures whose wire state can be proved
> pre-boundary (e.g. render failure, permission drop before any
> byte hit the transport) land in the error log as Output-flavor
> `Recovered` records; drain failures where the wire state is
> ambiguous (batched send cancelled mid-flight by shutdown, or
> the 3-second attempt timeout firing after the first request
> byte already left the kernel) route through the ambiguous DLQ
> path and force `Dropped` on a disk queue so the fail-stop wedge
> reconciles against the DLQ record on next start, while memory
> queues fold to `Recovered` for lack of a replay path. **`SIGKILL`
> (`kill -9`) cannot run either path** — actor tasks are aborted
> and their stack-local buffers go with them. Operators should
> not send `SIGKILL` directly to the daemon; keep systemd's
> `KillSignal=SIGTERM` default in place.

## Usage in pipelines

`output` is **non-terminal** — it deep-copies the event to the output queue and pipeline execution continues:

```limpid
def pipeline main {
    input syslog
    output archive       // event is copied to archive queue
    output siem          // event is also copied to siem queue
    // pipeline continues — both outputs receive the event
}
```

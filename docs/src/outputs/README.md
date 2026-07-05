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

## Queue and retry

Every output has an async queue that decouples pipeline processing from I/O. You can configure the queue and retry behavior:

```
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

`retry` is accepted by every output type. Retry-exhausted payloads are persisted to `control { error_log "..." }` (see [Recovery (error_log)](#recovery-error_log) below) when configured; otherwise they are dropped with a `tracing::warn!` and an `events_failed` counter increment.

### Disposition contract

Every event handed to an output resolves to one of three dispositions:

| Disposition | Meaning | Disk queue cursor | Memory queue cursor | Metrics |
|-------------|---------|-------------------|---------------------|---------|
| `Delivered` | Sink confirmed the send. | advances | advances | `events_written++` |
| `Recovered` | Send failed, but the failure record was durably written (`error_log` file or a full-payload `tracing::error!` line when `error_log` is unset). | advances | advances | `events_failed++` |
| `Dropped` | Send failed *and* no durable failure record was written (DLQ-write failure, bug / panic in the sink, runtime task abort past shutdown budget). | **holds — fail-stop wedge; consumer stops accepting new events and replays on next daemon start** | advances (memory queues cannot replay) | `events_failed++`, `events_wedged++` on disk queues |

The **fail-stop wedge** on disk queues is intentional. Holding the cursor guarantees that no event is silently lost on a durable queue; the trade-off is that the affected output's pipeline halts until an operator investigates and restarts the daemon. The wedge contract is defined for **unbatched sinks** (`file`, `stdout`, `unix_socket`, `syslog_tcp`, `syslog_udp`, `kafka`); a batched-sink wedge with parked buffer entries is a known limitation and may require `SIGKILL` to unblock the drain — see [Error Log → Disposition contract and fail-stop wedge on disk queues](../operations/error-log.md#disposition-contract-and-fail-stop-wedge-on-disk-queues) for the operator runbook.

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
- a batched output (`http`, `otlp_http`, `otlp_grpc`) failed to flush its remaining buffer during **graceful shutdown** (one record per still-parked event; see the shutdown / SIGKILL caveat below),
- the runtime could not hand an event to the output's queue at the pipeline → output boundary (queue closed, disk-queue write error, unknown output).

Each line is one per-event record carrying `event.egress` — the **pipeline-produced payload** at the moment it was handed to the sink (`egress` starts as a clone of `ingress` and is overwritten as the pipeline's process bodies run). Replay via `limpidctl inject output <name> --json` re-injects the event into the named sink's `consume()` path — the pipeline is **bypassed**, but the sink's own transport-level rendering (batched encode, HTTP body framing, OTLP `ResourceLogs` packing, etc.) **does** re-run. See [Error Log → Output flavor](../operations/error-log.md#output-flavor) for the full record shape and producer-site catalog.

When `error_log` is unset, Output-flavor records fall back to a `tracing::error!` line carrying the full JSONL in an `event_record` structured field — the same shape Process-flavor uses — so `journalctl | jq` can extract and replay the record. This is strictly worse than a dedicated DLQ file (log rotation, aggregation delays, tracing filters) but the payload is not lost. `limpid --check` emits a recovery-readiness warning so operators notice the missing configuration before the first failure.

> **Graceful shutdown vs. SIGKILL.** The shutdown-drain path above
> only fires on **graceful** shutdown — `SIGTERM`, `SIGHUP` reload,
> `systemctl stop`, or an explicit `shutdown()` call. The bounded
> final drain runs one attempt per parked payload with a 3-second
> `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` (plus the per-actor in-flight
> cancel); drain failures land in the error log as Output-flavor
> `Recovered` records. **`SIGKILL` (`kill -9`) cannot run this
> path** — actor tasks are aborted and their stack-local buffers go
> with them. Operators should not send `SIGKILL` directly to the
> daemon; keep systemd's `KillSignal=SIGTERM` default in place.

## Usage in pipelines

`output` is **non-terminal** — it deep-copies the event to the output queue and pipeline execution continues:

```
def pipeline main {
    input syslog
    output archive       // event is copied to archive queue
    output siem          // event is also copied to siem queue
    // pipeline continues — both outputs receive the event
}
```

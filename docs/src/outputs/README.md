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
| [`unix_socket`](./unix-socket.md) | Unix stream socket (Unix only) |
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

`retry` is accepted by every output type. Retry-exhausted payloads are saved to `control { error_log "..." }` (see [Recovery (error_log)](#recovery-error_log) below) only when configured and successfully written. When `error_log` is unset, ordinary retry-exhaustion recovery emits a payload-free error summary and resolves as `Recovered`; ambiguous shutdown recovery instead holds disk-queue positions as `Dropped` (see the disposition contract below) — `events_failed` counts every terminal failure once, disk vs memory queue agnostic. An operator who wants payload metadata or the full JSONL on the tracing side opts in explicitly via `control { error_log_fallback "meta" | "full" }`; that setting only takes effect when `error_log` is also configured (see [Recovery (error_log)](#recovery-error_log) for the ladder).

### HTTP and OTLP failure classification

`http` and `otlp_http` retry connection failures, timeouts, and HTTP 429,
502, 503, or 504. Other unsuccessful status codes, including other 4xx/5xx
and disallowed redirects, permanently exclude that peer for the current
batch. Another configured peer is tried immediately, without a backoff
sleep or spending the transient retry budget. A permanent response does
not prevent that peer from receiving future batches.

`otlp_grpc` retries CANCELLED, DEADLINE_EXCEEDED, ABORTED, OUT_OF_RANGE,
UNAVAILABLE, and DATA_LOSS. RESOURCE_EXHAUSTED is retryable only with
`google.rpc.RetryInfo`; other statuses permanently exclude that peer for
the current batch. See the gRPC and HTTP **Failures** sections of the
[OTLP specification in opentelemetry-proto v1.5.0](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/docs/specification.md).
These rules classify a response from one peer. Trying another configured
peer after a permanent failure is limpid's failover policy, rather than
an additional requirement imposed by that specification.

For retryable HTTP responses, `Retry-After` accepts seconds or an HTTP-date.
For retryable gRPC statuses, `RetryInfo.retry_delay` supplies the delay.
A valid delay replaces the next backoff sleep, capped by `retry.max_wait`.
Past dates and zero delays allow an immediate retry. For a failure already
classified as retryable, absent or malformed delay hints use configured
backoff. RESOURCE_EXHAUSTED without decodable `RetryInfo` is permanent,
so it does not reach that fallback. Hints cannot extend the attempt budget,
and shutdown still interrupts retry sleeps. Permanent failures do not
honor retry hints or retry the same peer within that batch, even if a
later peer fails transiently and triggers another send round.

The batch goes to recovery when every peer has failed permanently, or
a retryable failure exhausts `retry.max_attempts`. The budget counts send
rounds ending in a retryable failure, including the initial round;
immediate failover past permanent responses can contact additional peers
within one round. Shutdown uses one bounded send round, allowing permanent
failover within the existing overall deadline but no transient retries.
Recovery after every peer fails permanently uses the same error-log,
fallback, and queue-disposition rules as exhausted retries. A shutdown
send that is cancelled or times out leaves delivery ambiguous; see the
[disposition contract](#disposition-contract). Each permanent peer failure emits a warning
with the peer index and full cause chain, even if failover later succeeds.
If every peer fails permanently, the recovery reason includes each peer's
permanent failure summary. HTTP 400, 408, and 500, for example, visit each
peer at most once per batch while preserving failover to healthy peers.

### Disposition contract

Every event handed to an output resolves to one of three dispositions:

| Disposition | Meaning | Disk queue cursor | Memory queue progress | Metrics |
|-------------|---------|-------------------|---------------------|---------|
| `Delivered` | Sink confirmed the send. | advances | advances | `events_written++` |
| `Recovered` | Terminal failure handling completed. A DLQ write normally succeeded, or `error_log` is unset. Memory queues also use this disposition when a configured DLQ write fails or shutdown delivery is ambiguous. It does not prove that a durable copy exists or that the destination received nothing. | advances | advances | `events_failed++` |
| `Dropped` | Neither a `Delivered` nor a `Recovered` acknowledgement completed, or a disk-queue failure deliberately withholds a recovered acknowledgement. Causes include missing acknowledgements, configured DLQ-write failure on disk, and ambiguous shutdown sends. A DLQ record may still exist. | **holds — fail-stop wedge; replay on next daemon start; reconcile before restart** | advances; normal DLQ/ambiguous-recovery paths fold this to `Recovered` | `events_failed++`; `events_wedged++` on disk queues |

The **fail-stop wedge** on disk queues is intentional. Holding the cursor preserves the replay position; the trade-off is that the affected output's pipeline halts until an operator investigates and restarts the daemon. Both **unbatched sinks** (`file`, `stdout`, `unix_socket`, `syslog_tcp`, `syslog_udp`, `kafka`) and **batched sinks** (`http`, `otlp_http`, `otlp_grpc`) exit the wedge cleanly — unbatched sinks complete each event before accepting another; batched sinks take a separate wedge-exit path that attempts ambiguous recovery for parked buffer entries without any further send. See [Error Log → Disposition contract and fail-stop wedge on disk queues](../operations/error-log.md#disposition-contract-and-fail-stop-wedge-on-disk-queues) for the operator runbook.

### Memory queue (default)

Fast, but events are lost on process restart.

### Disk queue

Events are persisted to a Write-Ahead Log (WAL) and survive process restarts.
Segments rotate at 16 MiB; the acknowledgement cursor is saved atomically.
`max_size` removes only fully acknowledged segments, oldest first. Unread or
unacknowledged records are retained, so a backlog can exceed the configured
size. Zero or an omitted `max_size` disables size-based cleanup, and even a
non-zero value is not an upper bound on disk usage in this release. Appends
do not wait for a byte-size budget.

For either queue type, `capacity` must be an integer from `1` through
`2305843009213693951` on 64-bit platforms, or `536870911` on 32-bit platforms.
Out-of-range values are rejected at startup and by `--check`. This is a channel
representation limit, not a recommended buffer size or a memory budget.
Disk queues validate `capacity` but do not use it to limit the WAL backlog.
Size strings such as `max_size` must fit an unsigned 64-bit byte count after
expanding binary `KB`/`MB`/`GB` suffixes. Duration strings must fit an unsigned
64-bit second count after expanding minutes; overflow is a configuration error.

### Response failures and replay

A connection failure may occur before or after request bytes reach the peer. An HTTP
non-2xx response (including 3xx) is a failure after the request has reached
the peer; `bytes_written` increases before classifying that response.
Replaying a terminal failure from the DLQ can therefore duplicate delivery.
`Recovered` records recovery handling, not proof that the receiver stored
nothing. See [output recovery](../operations/error-log.md#output-flavor).

### Recovery (error_log)

The daemon-wide [`control { error_log "..." }`](../operations/error-log.md) block names a JSONL file that catches payloads the queue/retry chain could not place. Three sink-side paths feed it as Output-flavor records (`kind: "output"`):

- the output's `retry { ... }` budget was exhausted, or every configured HTTP/OTLP peer failed permanently,
- a batched output (`http`, `otlp_http`, `otlp_grpc`) failed to flush its remaining buffer during **graceful shutdown** (one record per still-parked event; see the shutdown caveat below), or exited through the fail-stop wedge with parked buffer entries (one ambiguous-disposition record per parked event),
- the runtime could not hand an event to the output's queue at the pipeline → output boundary (queue closed, disk-queue write error, unknown output).

Each line is one per-event record carrying `event.egress` — the **pipeline-produced payload** at the moment it was handed to the sink (`egress` starts as a clone of `ingress` and is overwritten as the pipeline's process bodies run). Replay via `limpidctl inject output <name> --json` re-injects the event into the named output — the pipeline is **bypassed**, but the sink's own transport-level rendering (batched encode, HTTP body framing, OTLP `ResourceLogs` packing, etc.) **does** re-run. See [Error Log → Output flavor](../operations/error-log.md#output-flavor) for the full record shape and producer-site catalog.

When `error_log` is unset, output recovery emits a one-line error summary (site + reason, no payload), without a recovery-file copy. Ordinary recovery resolves as `Recovered`; ambiguous shutdown recovery still holds disk positions as `Dropped`, while memory queues advance. The recovery label alone does not establish that a copy was saved — operators who want the metadata or the full JSONL on the tracing side must explicitly opt in with `control { error_log_fallback "meta" | "full" }`, and that opt-in only takes effect when `error_log` is also configured (see the ladder table below). `limpid --check` emits a recovery-readiness warning so operators notice the missing configuration before the first failure, plus a separate warning when `error_log_fallback` is set while `error_log` is unset (the fallback is inert in that combination).

**`error_log_fallback` ladder.** The tracing fallback line carries different fields depending on the operator's confidentiality choice. All rows preserve the same ack disposition — only the tracing emission differs.

| `error_log` | `error_log_fallback` | tracing line body                                                                              |
|-------------|----------------------|------------------------------------------------------------------------------------------------|
| unset       | (any — value ignored)| one-line summary (site + reason), no payload, no metadata                                      |
| set         | unset / `"off"`      | one-line summary; `error_log` write-failure noted                                              |
| set         | `"meta"`             | structured metadata (`kind`, `fallback`, `reason`, `timestamp`, `size`, `position`); no payload |
| set         | `"full"`             | `event_record = <full JSONL>` — payload may reach journald / log aggregation                    |

The `"full"` value includes full JSONL in the daemon error message for operators who want a journald-based recovery trail; it exposes the pipeline egress bytes to the tracing subscriber, so treat it as an opt-in for environments where the journald boundary is trusted.

> **Graceful shutdown vs. SIGKILL.** `SIGTERM`, `SIGHUP` reload, and
> `systemctl stop` run a final drain with one shared 3-second send deadline
> per output, within the overall shutdown budget. The preceding actor join
> has its own maximum wait of 3 seconds. Rendering failures and terminal
> send errors use the normal recovery disposition. Cancelling an in-flight
> send or timing it out leaves delivery ambiguous: disk queues hold their
> cursor (`Dropped`), while memory queues advance (`Recovered`). Inspect
> the DLQ and destination before replaying. `SIGKILL` bypasses the drain
> and loses in-memory work. Keep systemd's `KillSignal=SIGTERM` default.
> See [shutdown limits](../operations/error-log.md#graceful-shutdown-guarantees-and-known-limitations).

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

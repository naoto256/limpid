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

    secondary fallback_output              // optional failover target (bare ident)
}
```

`retry` is accepted by every output type. `secondary` takes a **bare identifier** referencing another `def output` — quoted strings are rejected at `--check`. A `secondary` that names an unknown output, references itself, or forms an indirect cycle (`A -> B -> A`, `A -> B -> C -> A`, …) is also rejected at `--check` so the misconfiguration is caught before deploy.

### Memory queue (default)

Fast, but events are lost on process restart.

### Disk queue

Events are persisted to a Write-Ahead Log (WAL) on disk. Survives process restarts.

- Segments are rotated at 16 MiB
- `max_size` limits total disk usage (oldest consumed segments are deleted)
- Cursor position is saved atomically

### Secondary output

When all retry attempts are exhausted, the event is forwarded to the `secondary` output instead of being dropped. Useful for dead-letter queues.

If the secondary enqueue itself fails, or no `secondary` is configured, the payload is written to `control { error_log "..." }` (see [Recovery (error_log)](#recovery-error_log) below). Without `error_log`, the event is dropped with a `tracing::warn!` and an `events_failed` counter increment only.

### Recovery (error_log)

The daemon-wide [`control { error_log "..." }`](../operations/error-log.md) block names a JSONL file that catches payloads the queue/retry/secondary chain could not place. Three paths feed it:

- the `secondary` enqueue itself failed,
- no `secondary` was configured and the retry budget was exhausted,
- a batched output (e.g. `http`, `otlp_http`, `otlp_grpc`) failed to flush its remaining buffer at shutdown.

Each line is one rendered payload. When `error_log` is unset the daemon falls back to 0.7.7-compatible behaviour (warn + drop), and `limpid --check` emits a warning so the operator notices the silent drop path.

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

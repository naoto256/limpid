# Outputs

Output modules write processed events to external destinations.

## Available types

| Type | Description |
|------|-------------|
| [`file`](./file.md) | Local file with dynamic path templates |
| [`http`](./http.md) | HTTP/HTTPS endpoint (Elasticsearch, Splunk HEC, etc.) |
| [`kafka`](./kafka.md) | Apache Kafka topic (requires `--features kafka`) |
| [`tcp`](./tcp.md) | TCP with persistent connection |
| [`udp`](./udp.md) | UDP datagram |
| [`unix_socket`](./unix-socket.md) | Unix stream socket |
| [`stdout`](./stdout.md) | Standard output (debugging) |
| [`otlp`](./otlp.md) | OTLP logs sender (HTTP/JSON, HTTP/protobuf, gRPC) |

## Queue and retry

Every output has an async queue that decouples pipeline processing from I/O. You can configure the queue and retry behavior:

```
def output reliable {
    type tcp
    address "10.0.0.1:514"

    queue {
        type disk                          // memory (default) | disk
        path "/var/lib/limpid/queues/out"  // required for disk queue
        max_size "1GB"                     // optional (default: unlimited)
        capacity 65536                     // channel buffer size (default: 65536)
    }

    retry {
        max_attempts 10                    // default: 3
        initial_wait "1s"                  // default: 1s
        max_wait "5m"                      // default: 30s
        backoff exponential                // exponential (default) | fixed
    }

    secondary fallback_output              // optional failover target
}
```

### Memory queue (default)

Fast, but events are lost on process restart.

### Disk queue

Events are persisted to a Write-Ahead Log (WAL) on disk. Survives process restarts.

- Segments are rotated at 16 MiB
- `max_size` limits total disk usage (oldest consumed segments are deleted)
- Cursor position is saved atomically

### Secondary output

When all retry attempts are exhausted, the event is forwarded to the `secondary` output instead of being dropped. Useful for dead-letter queues.

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

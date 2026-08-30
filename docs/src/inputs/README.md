# Inputs

Input modules receive log messages from external sources and feed them into pipelines.

## Available types

| Type | Description |
|------|-------------|
| [`raw_udp`](./raw-udp.md) | Byte-exact UDP datagram receiver |
| [`syslog_udp`](./syslog-udp.md) | UDP syslog receiver |
| [`syslog_tcp`](./syslog-tcp.md) | TCP syslog receiver (RFC 6587); optional TLS termination + mTLS |
| [`tail`](./tail.md) | File tailing with rotation detection |
| [`journal`](./journal.md) | systemd journal reader (requires `--features journal`) |
| [`unix_socket`](./unix-socket.md) | Unix datagram socket (`/dev/log`) |
| [`otlp_http`](./otlp-http.md) | OTLP/HTTP logs receiver (`POST /v1/logs`) |
| [`otlp_grpc`](./otlp-grpc.md) | OTLP/gRPC logs receiver (`LogsService.Export`) |

## Common properties

Each `def input` block declares its type via the `type <name>` clause; see the per-input pages for the property set each type accepts.

`rate_limit` (maximum events per second) is supported per input — see the per-input pages for which inputs expose it (currently `raw_udp`, the `syslog_*` receivers, and the `otlp_*` receivers).

## Usage in pipelines

An input is referenced by name in a pipeline definition:

```
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

def pipeline main {
    input fw          // references the input defined above
    output archive
}
```

Multiple pipelines can share the same input. Each pipeline receives an independent copy of every event.

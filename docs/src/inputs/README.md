# Inputs

Input modules receive log messages from external sources and feed them into pipelines.

## Available types

| Type | Description |
|------|-------------|
| [`syslog_udp`](./syslog-udp.md) | UDP syslog receiver |
| [`syslog_tcp`](./syslog-tcp.md) | TCP syslog receiver (RFC 6587) |
| [`syslog_tls`](./syslog-tls.md) | TCP+TLS syslog receiver |
| [`tail`](./tail.md) | File tailing with rotation detection |
| [`journal`](./journal.md) | systemd journal reader (requires `--features journal`) |
| [`unix_socket`](./unix-socket.md) | Unix datagram socket (`/dev/log`) |

## Common properties

All input types support:

| Property | Description |
|----------|-------------|
| `type` | Input type name (required) |
| `rate_limit` | Maximum events per second (optional) |

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

# syslog_udp

Sends events as UDP datagrams to one or more remote syslog endpoints.
Each peer uses its own ephemeral-port-bound socket. Round-robin
rotation and cooldown semantics match [syslog_tcp](./syslog-tcp.md),
with UDP best-effort caveats.

## Configuration

Single destination:

```limpid
def output relay {
    type syslog_udp
    peer { host "10.0.0.1" port 514 }
}
```

Multiple destinations:

```limpid
def output relay {
    type syslog_udp
    peers {
        peer { host "10.0.0.1" port 514 }
        peer { host "10.0.0.2" port 514 }
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `peer` | one of | - | Single-destination block. Mutually exclusive with `peers`. |
| `peers` | one of | - | Multi-destination block. Mutually exclusive with `peer`. |

Exactly one of `peer` or `peers` must be specified.

### peer block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `host` | yes | - | Hostname or IP of the syslog receiver. IPv4 literals (`10.0.0.1`), IPv6 literals (`::1`, `2001:db8::1` — bare or bracketed), and hostnames are all accepted. Hostnames that resolve only to AAAA work as well; limpid binds the ephemeral local socket in the matching address family. |
| `port` | no | `514` | UDP port |

## Multi-destination semantics

Same round-robin and cooldown model as
[syslog_tcp](./syslog-tcp.md#multi-destination-semantics). Note that
UDP send failures only fire for socket-level errors (e.g. ICMP
unreachable observed on `send()`); silent packet loss in transit is not
detected and does not enter cooldown.

## Notes

- UDP provides no delivery guarantee. For reliable delivery use
  [syslog_tcp](./syslog-tcp.md) (with optional per-peer TLS) or
  [http](./http.md) with a disk queue.
- Each peer's socket is bound to an ephemeral local port on first use.
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).

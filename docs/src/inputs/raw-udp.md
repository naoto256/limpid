# raw_udp

Receives arbitrary UDP datagrams without applying syslog PRI or text-format
validation. Use this input for protocols whose payload is opaque to limpid, or
when a pipeline performs its own decoding.

## Configuration

```limpid
def input packets {
    type raw_udp
    bind "0.0.0.0:5514"
    rate_limit 10000
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:514` | Listen address (`host:port`) |
| `rate_limit` | no | unlimited | Maximum events per second |

## Event contract

- One UDP datagram becomes one event. `ingress` and the initial `egress` are
  byte-for-byte copies of the datagram payload.
- Empty datagrams and payloads containing NUL or non-UTF-8 bytes are accepted.
- `source` is the sender's IP address and port, and `received_at` is assigned
  locally when the datagram is received.
- Received bytes and events use the normal input metric families. Raw payloads
  are not counted as invalid merely because they lack a syslog PRI header.

For RFC syslog validation, use [`syslog_udp`](./syslog-udp.md) instead.

## Operational notes

- The input uses a 65,536-byte receive buffer; valid UDP datagrams fit without
  truncation.
- UDP has no delivery acknowledgement. Kernel socket-buffer contents that have
  not been read when the daemon shuts down cannot be drained by the runtime.
- Binding to port 514 requires `CAP_NET_BIND_SERVICE` or root privileges.

# syslog_udp

Receives syslog messages as UDP datagrams.

## Configuration

```
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
    rate_limit 10000
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:514` | Listen address (`host:port`) |
| `rate_limit` | no | unlimited | Maximum events per second |

## Notes

- Messages must start with a valid PRI header (`<N>`). Invalid messages are dropped and counted as `events_invalid`.
- Maximum message size: 65,536 bytes (UDP datagram limit).
- Binding to port 514 requires `CAP_NET_BIND_SERVICE` or root privileges.

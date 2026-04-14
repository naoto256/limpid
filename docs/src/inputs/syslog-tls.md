# syslog_tls

Receives syslog messages over TCP with TLS encryption.

## Configuration

```
def input secure {
    type syslog_tls
    bind "0.0.0.0:6514"
    framing auto
    rate_limit 10000
    max_connections 1024
    tls {
        cert "/etc/limpid/certs/server.crt"
        key  "/etc/limpid/certs/server.key"
        ca   "/etc/limpid/certs/ca.crt"
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:6514` | Listen address |
| `framing` | no | `auto` | Same as [syslog_tcp](./syslog-tcp.md) |
| `rate_limit` | no | unlimited | Maximum events per second |
| `max_connections` | no | `1024` | Maximum simultaneous connections |

### tls block

| Property | Required | Description |
|----------|----------|-------------|
| `cert` | yes | Path to PEM-encoded server certificate |
| `key` | yes | Path to PEM-encoded private key |
| `ca` | no | Path to CA certificate for client verification |

## Notes

- When `ca` is specified, clients must present a valid certificate signed by that CA (mutual TLS).
- Without `ca`, any client can connect (server-only TLS).
- Same framing, idle timeout, and message size limits as [syslog_tcp](./syslog-tcp.md).

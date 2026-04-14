# syslog_tcp

Receives syslog messages over TCP with RFC 6587 framing support.

## Configuration

```
def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
    framing auto
    rate_limit 10000
    max_connections 1024
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:514` | Listen address |
| `framing` | no | `auto` | `auto`, `octet_counting`, or `non_transparent` |
| `rate_limit` | no | unlimited | Maximum events per second |
| `max_connections` | no | `1024` | Maximum simultaneous TCP connections |

## Framing modes

Per [RFC 6587](https://www.rfc-editor.org/rfc/rfc6587):

- **`auto`** (default) — auto-detects per connection based on the first byte:
  - Digit (1-9) → octet counting
  - `<` → non-transparent framing (LF/CRLF/NUL delimited)
- **`octet_counting`** — `MSG-LEN SP SYSLOG-MSG` format
- **`non_transparent`** — messages delimited by LF, CRLF, or NUL

## Notes

- PRI validation is enforced on all messages.
- Idle connections are closed after 300 seconds.
- Maximum message size: 1 MiB.
- Connections exceeding `max_connections` are rejected immediately.

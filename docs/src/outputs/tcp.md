# tcp

Sends events to a remote TCP endpoint with persistent connection and automatic reconnection.

## Configuration

```
def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `address` | yes | — | Target address (`host:port`) |
| `framing` | no | `octet_counting` | `octet_counting` or `non_transparent` |

Alternatively, use `host` and `port` separately:

```
def output remote {
    type tcp
    host "10.0.0.1"
    port 514
}
```

## Framing

- **`octet_counting`** (default) — `MSG-LEN SP SYSLOG-MSG` per RFC 6587
- **`non_transparent`** — messages terminated by LF

## Notes

- The TCP connection is established on the first event and reused for subsequent events.
- If the connection is broken, it is automatically re-established on the next event.
- The connection is not retried in a loop — retry logic is handled by the [queue](../outputs/README.md#queue-and-retry).

# unix_socket

Sends events to a Unix stream socket with persistent connection and automatic reconnection.

## Configuration

```
def output local_forward {
    type unix_socket
    path "/var/run/other/input.sock"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | Path to the Unix stream socket |

## Notes

- Each frame is the `egress` bytes verbatim followed by a `\n`. Non-UTF-8 payloads are written unchanged rather than lossily normalised to U+FFFD, so binary payloads round-trip to the peer without silent corruption.
- Connection is established on first use and reused.
- Automatically reconnects if the connection breaks.
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).

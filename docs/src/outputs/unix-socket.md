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

- Connection is established on first use and reused.
- Automatically reconnects if the connection breaks.

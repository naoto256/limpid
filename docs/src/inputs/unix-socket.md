# unix_socket

Receives syslog messages from a Unix datagram socket. Used to receive messages from `logger` and local applications via `/dev/log`.

## Configuration

```
def input local {
    type unix_socket
    path "/dev/log"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | Path to the Unix datagram socket |

## Notes

- The socket file is created with mode `0666` (world-writable) so any local process can send messages.
- If the configured `path` is a symlink, the input refuses to start (it will not follow the link). Otherwise, stale socket files at that path are removed automatically on startup.
- PRI validation is enforced on all messages.
- Works with the `logger` command: `logger "hello from limpid"`

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
- The stale-cleanup path only unlinks actual socket nodes at `path`. If the configured `path` points at a symlink, a regular file, a directory, a FIFO, or a device node, the input refuses to start with a diagnostic that names the observed shape — it will not follow a symlink or silently delete a real file that happens to share the path. Only a leftover socket inode from a previous run is replaced automatically.
- PRI validation is enforced on all messages.
- Works with the `logger` command: `logger "hello from limpid"`

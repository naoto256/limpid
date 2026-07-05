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

- The socket file is created with mode `0o666` (world-writable) so any local process can send messages. `chmod 0o666` failure at startup is **fatal** — the input refuses to listen on a socket whose mode does not match the operator-facing contract, rather than silently downgrading who can `sendto` the inode. Daemon startup fails with the underlying `set_permissions` error.
- **Parent-directory safety** is enforced at daemon startup. The configured `path`'s parent must not be group-writable or world-writable — the input binds a world-writable socket, so a parent that lets an outside-the-owner process create files at that path would let an attacker swap the socket between shutdown and next bind. `/dev` (`0o755` on standard POSIX systems, for the flagship `/dev/log` deploy) passes. `/tmp` (`0o1777`) does **not**: sticky protects unlink of files the attacker doesn't own, but a swap attack plants an attacker-owned node. `/tmp/foo.sock` is unsupported — use `/dev/log` or a packaged runtime directory. Failure surfaces at startup with a diagnostic naming the observed mode and the remediation.
- The stale-cleanup path only unlinks actual socket nodes at `path`. If the configured `path` points at a symlink, a regular file, a directory, a FIFO, or a device node, the input refuses to start with a diagnostic that names the observed shape — it will not follow a symlink or silently delete a real file that happens to share the path. Only a leftover socket inode from a previous run is replaced automatically.
- At shutdown, the input records the `(dev, ino)` of the socket it bound and refuses to unlink the path if the on-disk inode has been swapped since bind. Under the safe-parent contract above no outside-the-owner writer can produce the swap; this check is defense-in-depth, not the load-bearing guard.
- PRI validation is enforced on all messages.
- Works with the `logger` command: `logger "hello from limpid"`

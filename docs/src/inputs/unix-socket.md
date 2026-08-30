# unix_socket

Receives syslog messages from a Unix datagram socket. Used to receive messages from `logger` and local applications via `/dev/log`.

## Configuration

```limpid
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
- **Parent-directory safety** is enforced at daemon startup as three independent checks on the configured `path`'s parent directory:
  - **Final component symlink** — the final parent component is checked with `symlink_metadata`. A symlink parent lets an attacker redirect the bind target between this preflight and the daemon's actual `bind`, so it is rejected up front. **Ancestor** components may still be symlinks — modern Linux ships `/var/run` as a symlink to `/run`, and packaged deployments rely on that compat. Ancestor path identity is a **deployment contract**: point `path` at a real directory whose ancestor chain resolves under a directory the operator controls.
  - **Owner** — the parent must be owned by the daemon's own effective uid. An untrusted owner retains rename/unlink rights inside the directory regardless of mode bits, and can swap the socket inode between the stale-cleanup stat and the follow-up unlink, or between shutdown and next bind. `/dev` (root-owned) is trusted **only when the daemon runs as root** — the classic `/dev/log` deploy shape. A non-root daemon has no write permission on `/dev` at the packaged `0o755` anyway, so bind would fail post-validation; non-root deploys should point `path` at a daemon-owned runtime directory instead.
  - **Mode** — the parent must not be group-writable or world-writable. The input binds a world-writable socket, so a parent that lets an outside-the-owner process create files at that path would let an attacker plant a swap target. `/dev` (`0o755` on standard POSIX systems, for the flagship `/dev/log` deploy) passes. `/tmp` (`0o1777`) does **not**: sticky protects unlink of files the attacker doesn't own, but a swap attack plants an attacker-owned node. `/tmp/foo.sock` is unsupported — use `/dev/log` or a packaged runtime directory.
  Failure surfaces at startup with a diagnostic naming whether the symlink shape, owner, or mode failed and the observed value.
- The stale-cleanup path only unlinks actual socket nodes at `path`. If the configured `path` points at a symlink, a regular file, a directory, a FIFO, or a device node, the input refuses to start with a diagnostic that names the observed shape — it will not follow a symlink or silently delete a real file that happens to share the path. Only a leftover socket inode from a previous run is replaced automatically.
- At shutdown, the input records the `(dev, ino)` of the socket it bound and refuses to unlink the path if the on-disk inode has been swapped since bind. Under the safe-parent contract above no outside-the-owner writer can produce the swap; this check is defense-in-depth, not the load-bearing guard.
- PRI validation is enforced on all messages.
- Works with the `logger` command: `logger "hello from limpid"`

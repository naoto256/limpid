# unix_socket

Sends events to a Unix stream socket with persistent connection and automatic reconnection.

## Configuration

```limpid
def output local_forward {
    type unix_socket
    path "/var/run/other/input.sock"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | Path to the Unix stream socket |
| `expected_peer_uid` | no | *(daemon euid + root)* | Username the socket's listener must run as. See [Peer credential trust](#peer-credential-trust). |

## Notes

- Each frame is the `egress` bytes verbatim followed by a `\n`. Non-UTF-8 payloads are written unchanged rather than lossily normalised to U+FFFD, so binary payloads round-trip to the peer without silent corruption.
- Connection is established on first use and reused.
- Automatically reconnects if the connection breaks.
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).

## Peer credential trust

The `unix_socket` output is a **connect-side** sink — the daemon connects to a socket that belongs to another service (systemd `journald` at `/dev/log`, `rsyslogd`, a local collector, …). The bind-side trust boundaries used by the DLQ file, the file output, and the control / input `unix_socket` (parent directory owner, refusal of symlinked paths) do **not** apply here: the peer service owns its socket path, and typical packaged deployments break every bind-side predicate — `/dev/log` is a symlink to `/run/systemd/journal/dev-log` on systemd systems, and its parent `/dev` is root-owned. Path-shape checks would refuse the most common shape.

Instead, defence lives at the socket credential surface. On every `connect(2)` — including the reconnects `write_with_reconnect` performs after a peer restart — the daemon reads the listener's uid via `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED` (macOS) and refuses the connection if that uid is not in the allowed set. This defends against a **non-root co-tenant** who binds a squatter socket at the configured `path` in the window between two peer-service restarts and would otherwise capture every subsequent event's payload.

The allowed set is:

- **`expected_peer_uid` unset** (default): `{daemon euid, root}`. `journald` runs as root, so `/dev/log` is accepted; a daemon-owned collector matches the euid arm. Root is trusted here because an attacker who can already `bind` as root has crossed a larger boundary than this check defends.
- **`expected_peer_uid "syslog"`** (or any user name): the default set is **replaced**, not extended. Root is refused too. Use this lock-down mode when the deployment has a known dedicated collector uid and the operator wants a socket-squatter-by-root to also be refused. Numeric-string uids are rejected — configure by name, matching the file output's `owner` semantics.

If a `connect()` observes an unexpected uid it fails loudly (structured error with the observed uid, the allowed set, and the `expected_peer_uid` remedy). The failure flows through the normal retry / DLQ path so nothing is silently dropped.

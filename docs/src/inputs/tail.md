# tail

Follows a log file, emitting each new line as an event. Detects log rotation and optionally persists the **acked offset** across restarts — see the `state_file` property below for the at-least-once semantics that follow from this.

## Configuration

```limpid
def input app_log {
    type tail
    path "/var/log/app/current.log"
    state_file "/var/lib/limpid/tail/app"
    poll_interval "1s"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | Path to the file to follow |
| `state_file` | no | none | Path to persist the **acked file offset** (the highest offset whose downstream pipeline ack has fired). The read cursor may run ahead of the acked offset for in-flight events; on restart, anything between the acked offset and the prior read cursor is re-read, so delivery is **at-least-once**. |
| `poll_interval` | no | `1s` | How often to check for new data |

## Rotation detection

The tail input detects two forms of log rotation:

- **Inode change** — the file was replaced (e.g., `logrotate` without `copytruncate` — the new file gets a fresh inode)
- **File truncation** — the file was truncated to zero (e.g., `logrotate` with `copytruncate`, which preserves the inode and truncates in place)

In both cases, reading resets to the beginning of the new file.

## Notes

- On first start without a `state_file`, reading begins at the end of the file (new data only).
- Empty lines are skipped.
- Incomplete lines (no trailing newline) are held until the next poll.
- The source address for tail events is `127.0.0.1:0` (a placeholder address with port 0; tail has no network peer).
- Since 0.7.8, the persisted offset advances only after the pipeline worker finishes processing the corresponding line. A crash mid-processing leaves the on-disk cursor at the last *acked* offset, so the next start re-reads any in-flight lines — this is the at-least-once recovery contract documented under [Recovery readiness](../operations/error-log.md#recovery-readiness-check---check).

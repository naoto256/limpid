# tail

Follows a log file, emitting each new line as an event. Detects log rotation and optionally persists the read position across restarts.

## Configuration

```
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
| `state_file` | no | none | Path to persist read position (survives restarts) |
| `poll_interval` | no | `1s` | How often to check for new data |

## Rotation detection

The tail input detects two forms of log rotation:

- **Inode change** — the file was replaced (e.g., `logrotate` with `copytruncate`)
- **File truncation** — the file was truncated to zero

In both cases, reading resets to the beginning of the new file.

## Notes

- On first start without a `state_file`, reading begins at the end of the file (new data only).
- Empty lines are skipped.
- Incomplete lines (no trailing newline) are held until the next poll.
- The source address for tail events is `127.0.0.1`.

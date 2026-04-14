# journal

Reads entries from the systemd journal. Linux only.

## Build requirement

```bash
sudo apt install libsystemd-dev
cargo build --release -p limpid --features journal
```

## Configuration

```
def input system {
    type journal
    match "SYSLOG_FACILITY=10"
    state_file "/var/lib/limpid/journal/cursor"
    poll_interval "1s"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `match` | no | none | Journal match filter (e.g., `SYSLOG_FACILITY=10`) |
| `state_file` | no | none | Path to persist journal cursor (survives restarts) |
| `poll_interval` | no | `1s` | How often to poll for new entries |

## Output format

Journal entries are formatted as syslog-like messages:

```
IDENTIFIER[PID]: MESSAGE
```

For example: `sshd[1234]: Accepted publickey for user`

Fields used (in order of preference):
- `SYSLOG_IDENTIFIER` or `_COMM` for the identifier
- `SYSLOG_PID` or `_PID` for the process ID

## Notes

- On first start without a `state_file`, reading begins at the end of the journal.
- The cursor is saved atomically (write-to-temp + rename).
- The source address for journal events is `127.0.0.1`.

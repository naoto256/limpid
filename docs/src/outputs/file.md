# file

Appends event messages to a local file. Supports dynamic path templates and file permission control.

## Configuration

```
def output archive {
    type file
    path "/var/log/limpid/archive.log"
    mode "0640"
    owner "syslog"
    group "adm"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | File path (supports templates) |
| `mode` | no | system default | Octal file permissions (e.g., `"0640"`) |
| `owner` | no | process user | File owner (requires `CAP_CHOWN`) |
| `group` | no | process group | File group |

Permissions are applied only when the file is first created.

## Dynamic path templates

The `path` property supports `${...}` placeholders that are resolved per event:

| Template | Expands to | Example |
|----------|------------|---------|
| `${source}` | Source IP address | `192.0.2.3` |
| `${facility}` | Facility number | `16` |
| `${severity}` | Severity number | `6` |
| `${date}` | `YYYY-MM-DD` | `2026-04-15` |
| `${year}` | 4-digit year | `2026` |
| `${month}` | 2-digit month | `04` |
| `${day}` | 2-digit day | `15` |
| `${fields.xxx}` | Field value | `FW01` |

Example:

```
def output per_source {
    type file
    path "/var/log/limpid/${source}/${date}.log"
}
```

Parent directories are created automatically. Field values are sanitized to prevent path traversal.

## Notes

- Each line is one event message followed by a newline.
- For log rotation, use `logrotate` with `copytruncate` or `create` + SIGHUP.

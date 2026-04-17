# CLI

## limpid

```bash
# Run as daemon
limpid --config /etc/limpid/limpid.conf

# Validate configuration and exit
limpid --check --config /etc/limpid/limpid.conf

# Test a pipeline with sample data
limpid --test-pipeline <name> --config /etc/limpid/limpid.conf \
  --input '{"raw": "<134>test message"}'

# Enable debug trace logging
limpid --debug --config /etc/limpid/limpid.conf
```

### Options

| Flag | Description |
|------|-------------|
| `--config <path>` | Main configuration file (default: `/etc/limpid/limpid.conf`) |
| `--check` | Validate configuration and exit |
| `--test-pipeline <name>` | Test a named pipeline with sample data |
| `--input <json>` | Sample event for test mode (JSON) |
| `--debug` | Enable trace-level logging |

### Test mode input format

```json
{
  "raw": "<134>Apr 15 10:30:00 myhost sshd: test",
  "source": "192.0.2.3:514",
  "facility": 16,
  "severity": 6,
  "fields": {
    "custom_field": "value"
  }
}
```

All fields except `raw` are optional. `source` can be `ip:port` or just `ip`.

## limpidctl

```bash
# Stream events from an output
limpidctl tap output ama

# Stream events entering an input
limpidctl tap input fw_syslog

# Stream events after a process
limpidctl tap process parse_cef

# Stream full Event JSON (one per line) — useful for piping to jq
limpidctl tap output ama --json

# List available tap points
limpidctl list
limpidctl list --json

# Show metrics
limpidctl stats
limpidctl stats --json

# Health check
limpidctl health
limpidctl health --json
```

### Global options

| Flag | Description |
|------|-------------|
| `--socket <path>` | Control socket path (default: `/var/run/limpid/control.sock`) |

See [Debug Tap](./tap.md) for details.

## limpid-prometheus

Prometheus exporter — converts limpid's JSON stats to Prometheus text exposition format.

```bash
limpid-prometheus --bind 127.0.0.1:9100 --socket /var/run/limpid/control.sock
```

| Flag | Description |
|------|-------------|
| `--bind <addr>` | HTTP bind address (default: `127.0.0.1:9100`) |
| `--socket <path>` | Control socket path (default: `/var/run/limpid/control.sock`) |

| Endpoint | Response |
|----------|----------|
| `GET /health` | `OK` (plain text) |
| `GET /metrics` | Prometheus text format (`text/plain; version=0.0.4`) |

See [Metrics](./metrics.md) for the full list of exported metrics.

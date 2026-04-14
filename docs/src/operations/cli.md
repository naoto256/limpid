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

## limpid-tap

```bash
# Stream events from an output
limpid-tap output ama

# Stream events entering an input
limpid-tap input fw_syslog

# Stream events after a process
limpid-tap process parse_cef

# List available tap points
limpid-tap --list

# Show metrics
limpid-tap --stats
limpid-tap --stats --json

# Health check
limpid-tap --health
```

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

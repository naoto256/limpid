# Debug Tap

`limpidctl tap` lets you stream events from any point in the pipeline in real time, without restarting the daemon or modifying configuration.

## Usage

```bash
# Stream events from an output queue
sudo limpidctl tap output ama

# Stream events entering an input
sudo limpidctl tap input splunk_udp

# Stream events after a named process
sudo limpidctl tap process parse_cef

# List all available tap points
sudo limpidctl list
```

By default, tap emits each event's `message` bytes as a line of text. Add `--json` to emit the full Event (timestamp, source, facility, severity, raw, message, fields) as one JSON object per line:

```bash
# Full Event JSON, one per line — pipe to jq for inspection
sudo limpidctl tap output ama --json | jq .

# Extract just severity + message
sudo limpidctl tap input splunk_udp --json | jq -r '[.severity, .message] | @tsv'
```

## How it works

Tap points are registered for every input, process, and output. When you connect with `limpidctl tap`, events are broadcast to your terminal via the control socket.

**Zero overhead when not tapping** — the only cost is an atomic load (subscriber count check) per event per tap point. No events are cloned or serialized unless someone is actually listening.

## Filtering

`limpidctl tap` streams all events from the tap point. For filtering, pipe to standard Unix tools:

```bash
# Only FortiGate events
sudo limpidctl tap output ama | grep Fortinet

# Only high-severity (raw PRI-prefixed text)
sudo limpidctl tap input syslog | grep -E '<[0-3]>'

# Structured filter via full-Event JSON
sudo limpidctl tap output siem --json | jq 'select(.severity <= 3)'
```

## Metrics

```bash
# Human-readable
sudo limpidctl stats

# JSON (for scripts)
sudo limpidctl stats --json
```

Example output:

```
Inputs:
  syslog_tcp                    177 received         0 invalid
  syslog_udp                    177 received         0 invalid

Pipelines:
  forward                       177 received       171 finished         6 dropped         0 discarded
  archive                       177 received       166 finished        11 dropped         0 discarded

Outputs:
  siem                          171 written         0 failed         0 retries
  fw02                          166 written         0 failed         0 retries
```

## Control socket

`limpidctl` connects to the daemon's Unix control socket (default: `/var/run/limpid/control.sock`). Use `--socket` to specify a different path:

```bash
limpidctl --socket /custom/path/control.sock stats
```

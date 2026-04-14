# Debug Tap

`limpid-tap` lets you stream events from any point in the pipeline in real time, without restarting the daemon or modifying configuration.

## Usage

```bash
# Stream events from an output queue
sudo limpid-tap output ama

# Stream events entering an input
sudo limpid-tap input splunk_udp

# Stream events after a named process
sudo limpid-tap process parse_cef

# List all available tap points
sudo limpid-tap --list
```

## How it works

Tap points are registered for every input, process, and output. When you connect with `limpid-tap`, events are broadcast to your terminal via the control socket.

**Zero overhead when not tapping** — the only cost is an atomic load (subscriber count check) per event per tap point. No events are cloned or serialized unless someone is actually listening.

## Filtering

`limpid-tap` streams all events from the tap point. For filtering, pipe to standard Unix tools:

```bash
# Only FortiGate events
sudo limpid-tap output ama | grep Fortinet

# Only high-severity
sudo limpid-tap input syslog | grep -E '<[0-3]>'

# Parse as JSON
sudo limpid-tap output siem | jq .
```

## Metrics

```bash
# Human-readable
sudo limpid-tap --stats

# JSON (for scripts)
sudo limpid-tap --stats --json
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

`limpid-tap` connects to the daemon's Unix control socket (default: `/var/run/limpid/control.sock`). Use `--socket` to specify a different path:

```bash
limpid-tap --socket /custom/path/control.sock --stats
```

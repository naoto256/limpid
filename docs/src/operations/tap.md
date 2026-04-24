# Debug Tap

`limpidctl tap` lets you stream events from any point in the pipeline in real time, without restarting the daemon or modifying configuration.

## Usage

```bash
# Stream events from an output queue
sudo limpidctl tap output ama

# Stream events entering an input
sudo limpidctl tap input splunk_udp

# Stream events after a named process
sudo limpidctl tap process enrich_fortigate

# List all available tap points
sudo limpidctl list
```

By default, tap emits each event's `egress` bytes as a line of text. Add `--json` to emit the full Event (timestamp, source, ingress, egress, workspace) as one JSON object per line:

```bash
# Full Event JSON, one per line — pipe to jq for inspection
sudo limpidctl tap output ama --json | jq .

# Extract just source + egress
sudo limpidctl tap input splunk_udp --json | jq -r '[.source, .egress] | @tsv'
```

## How it works

Tap points are registered for every input, process, and output. When you connect with `limpidctl tap`, events are broadcast to your terminal via the control socket.

**Zero overhead when not tapping** — the only cost is an atomic load (subscriber count check) per event per tap point. No events are cloned or serialized unless someone is actually listening.

## Filtering

`limpidctl tap` streams all events from the tap point. For filtering, pipe to standard Unix tools:

```bash
# Only FortiGate events
sudo limpidctl tap output ama | grep Fortinet

# Only high-severity (PRI-prefixed text)
sudo limpidctl tap input syslog | grep -E '<[0-3]>'

# Structured filter via full-Event JSON (severity lives inside the egress bytes;
# decode the leading <PRI> here, or rely on a workspace field set in your pipeline)
sudo limpidctl tap output siem --json | jq 'select(.workspace.cef_severity != null and (.workspace.cef_severity | tonumber) <= 3)'
```

## Inject

`limpidctl inject` is the symmetric counterpart of `tap` — instead of reading events from a pipeline point, it pushes events into one.

- `inject input <name>` — events are written to that input's event channel and flow through every pipeline referencing the input (bypassing the input module itself).
- `inject output <name>` — events are pushed directly into the output's queue, bypassing pipelines entirely.

Process injection is not supported: a process by itself has no pipeline context.

```bash
# Raw mode — each stdin line becomes one event's ingress bytes.
# Source is set to 127.0.0.1:0 (same convention used by the `tail` and `journal` inputs).
limpidctl inject input fw_syslog < raw.log
limpidctl inject output ama < messages.log

# JSON mode — each stdin line is a full Event object
# (the same format emitted by `tap --json`). Invalid lines are
# logged at warn level and skipped; the rest are still injected.
limpidctl inject input fw_syslog --json < events.jsonl
limpidctl inject output ama --json < events.jsonl
```

On success, `limpidctl inject` prints the number of events injected:

```json
{"injected": 1234}
```

### Replay with tap → inject

Because `tap --json` and `inject --json` share the same Event JSON schema, you can record traffic from one daemon and replay it into another (or back into the same one):

```bash
# Capture 1000 events from a live input
limpidctl tap input fw_syslog --json | head -n 1000 > replay.jsonl

# Later, replay them into a staging daemon's equivalent input
limpidctl --socket /run/limpid-staging.sock inject input fw_syslog --json < replay.jsonl
```

This is useful for reproducing parse failures, load-testing a new pipeline, or seeding a development daemon with realistic traffic.

### Replaying with original timing

By default, `inject --json` streams events as fast as stdin allows. That is fine for reproducing a single parse failure, but it does not reproduce workloads where timing matters — rate-limit behaviour, output backpressure under spikes, or filter saturation all depend on the **cadence** of incoming events, not just their content.

`--replay-timing` replays events with the same wall-clock gaps they had at capture time, using each event's top-level `timestamp` field (emitted by `tap --json`):

```bash
# Real-time replay — gaps between events match the capture
limpidctl inject input fw_syslog --json --replay-timing < replay.jsonl

# 10× faster — replay a one-hour capture in six minutes
limpidctl inject input fw_syslog --json --replay-timing=10x < replay.jsonl

# 0.2× (5× slower) — useful for stepping through a burst in detail
limpidctl inject input fw_syslog --json --replay-timing=0.2x < replay.jsonl

# `realtime` is a synonym for `1x`
limpidctl inject input fw_syslog --json --replay-timing=realtime < replay.jsonl
```

The first event is sent immediately and anchors the schedule. Each subsequent event is held until `(event.timestamp − first.timestamp) / factor` of wall-clock time has elapsed, then sent.

**Requires `--json`.** Raw line mode has no timestamps to replay against — passing `--replay-timing` without `--json` is an error.

**Error handling is strict by design** (no hidden behaviour):

| Situation | Behaviour |
|---|---|
| Event has no `timestamp`, or it does not parse as RFC 3339 | Abort with an error on that event |
| Invalid factor (e.g. `--replay-timing=bogus`, `0x`, negative) | Abort before connecting |
| `timestamp` goes backwards between events | Log a warning to stderr, send immediately, continue. Ordering follows the input JSONL — `inject` does **not** reorder |
| Wall clock falls behind the schedule (slow stdin, backpressure) | Catch up by sending with zero delay. A single warning is logged on stderr the first time this happens |

Filtering to a time range is intentionally not a flag — use `jq` before piping:

```bash
jq -c 'select(.timestamp >= "2026-01-01T12:00:00Z" and .timestamp < "2026-01-01T13:00:00Z")' \
  replay.jsonl | limpidctl inject input fw_syslog --json --replay-timing
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
Pipelines:
  forward                       177 received       171 finished         6 dropped         0 discarded
  archive                       177 received       166 finished        11 dropped         0 discarded

Inputs:
  syslog_tcp                    177 received         0 invalid         0 injected
  syslog_udp                    177 received         0 invalid         5 injected

Outputs:
  siem                          171 received         0 injected       171 written         0 failed         0 retries
  fw02                          166 received         0 injected       166 written         0 failed         0 retries
```

Inject counters make synthetic/replay traffic distinguishable from real receipts. See [Metrics](./metrics.md) for details.

## Control socket

`limpidctl` connects to the daemon's Unix control socket (default: `/var/run/limpid/control.sock`). Use `--socket` to specify a different path:

```bash
limpidctl --socket /custom/path/control.sock stats
```

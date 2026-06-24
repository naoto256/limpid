# Error Log (Dead-Letter Queue)

When a `process` statement raises a runtime error — unknown identifier, type mismatch, regex compile failure, parser blowup on malformed input — the event is set aside in a **dead-letter queue (DLQ)** rather than forwarded with the original `ingress` unchanged. The same DLQ also receives events routed by an explicit [`error` statement](../pipelines/drop-finish-error.md#error): a snippet parser dispatcher hitting an unsupported subtype, or a process detecting a missing-required-field contract violation, can call `error "..."` to land the event in the DLQ with an operator-authored reason. Operators audit the failures, fix the offending config or parser, and replay the events.

This page covers the on-disk format, the `control { error_log "..." }` opt-in, and the replay recipe. The corresponding metrics (`events_errored`, `events_errored_unwritable`) are documented under [Metrics](./metrics.md).

## Why a DLQ instead of forwarding or discarding

Three behaviours were considered:

| Behaviour | Pre-0.5 | 0.5.1 | 0.5.2+ |
|-----------|---------|-------|--------|
| Forward original `ingress` | ✅ | — | — |
| Discard (data loss) | — | ✅ | — |
| **Write to DLQ + counter** | — | — | ✅ |

- **Forward original `ingress`** turns wrap / enrichment bugs into data-shape regressions at the receiving SIEM (rsyslog-style "best effort"). The bug is silent until something downstream complains.
- **Discard** makes the bug visible (counter goes up) but is itself a strong failure mode: a security telemetry pipeline that drops events to a config typo is the wrong default.
- **DLQ** preserves the data and the bug signal: `events_errored` ticks up *and* the original event is recoverable. This is the Logstash / Fluentd `@ERROR` pattern.

The runtime cannot guess what the operator intended at the failure point — `egress` may have been partially rewritten by earlier processes in the chain, the next process expected a workspace key that was never produced, etc. So the DLQ deliberately preserves only the *original* event (ingress / source / received_at) and lets the operator re-run from scratch after the fix.

## Configuring the DLQ

The DLQ file is opt-in via the `control { error_log "..." }` property:

```limpid
control {
    socket    "/var/run/limpid/control.sock"
    error_log "/var/log/limpid/errored.jsonl"
}
```

When `error_log` is **unset**, the same record is emitted as a structured `tracing::error!` line — operators using `systemd` can still recover via `journalctl -u limpid -o json | jq …`. The data is never silently lost.

The recommended deployment is the explicit file path: a dedicated DLQ file is easier to monitor, easier to rotate, and decouples replay volume from journald rate limits.

### Startup validation

At daemon start (and on `SIGHUP` reload), limpid stat()s the parent directory of `error_log` and refuses to start if it doesn't exist or isn't a directory. Operator typos surface before any event hits the failure path, not after the first runtime error. The file itself does not need to exist — the daemon creates it on the first failure.

If the directory is reachable but the daemon can't *write* to it (wrong owner, read-only filesystem), startup still succeeds; the failure surfaces as `events_errored_unwritable` increments at runtime. See [When the DLQ write itself fails](#when-the-dlq-write-itself-fails) for the diagnosis path.

### Permissions and rotation

The daemon opens the file with `OpenOptions::create(true).append(true)` per write, so:

- The file is created on first failure if it doesn't exist (parent directory must exist and be writable by the daemon user — checked at startup).
- `logrotate` with `copytruncate` works without a SIGHUP handshake — the daemon picks up the new inode on the next failure.
- Concurrent failures from multiple pipeline workers serialise through an in-process `Mutex` inside the writer. POSIX `O_APPEND` only guarantees atomic append for writes ≤ `PIPE_BUF` (Linux: 4 KiB), and DLQ records carrying base64-encoded binary ingress easily exceed that — so limpid does not rely on the kernel-level guarantee.

### Recommended `logrotate` configuration

The DLQ has no in-process size cap; sustained failures can fill the disk. Pair it with a `logrotate` entry:

```
/var/log/limpid/errored.jsonl {
    daily
    rotate 14
    compress
    delaycompress
    copytruncate
    notifempty
    missingok
    create 0640 limpid adm
    maxsize 1G
}
```

Key choices:

- `copytruncate` — limpid reopens the inode every write, so a normal rotate-and-rename works too, but `copytruncate` is the simplest setup that doesn't require any signal handshake.
- `maxsize 1G` — caps the live file even when `daily` hasn't fired yet. A pipeline producing failures at 10k events/sec with 1 KiB records would fill 1 GiB in ~100 seconds; tune to your environment.
- `rotate 14 + compress` — two weeks of rotated history is usually enough to catch and replay everything between an incident and the operator noticing it.

Operators with stricter retention needs (compliance: hold N days of forensic-quality records) should size accordingly and consider shipping the rotated archives to long-term storage.

## Record format

One JSON object per line:

```json
{
  "timestamp": "2026-04-27T03:28:39.178046123Z",
  "reason": "unknown identifier: timestamp",
  "process": "wrap_journal",
  "pipeline": "journal_forward",
  "event": {
    "source": {"ip": "10.0.0.1", "port": 514},
    "received_at": 1745719719178046000,
    "ingress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello"
  }
}
```

| Field | Meaning |
|-------|---------|
| `timestamp` | RFC3339 with nanosecond precision; wall-clock at which the error was raised. |
| `reason` | Stringified `ProcessError`. Stable enough for `grep` / classification but not a stable API. |
| `process` | Failed process or recovery-path discriminator. A named `def process` invocation surfaces its name; an inline `process { ... }` block surfaces `(inline)`. For records originating from an output's recovery path the value is one of four discriminators: `(output <name>)` — retry exhausted (PR-O); `(output <name> shutdown)` — batched output's shutdown drain (PR-P); `(pipeline)` — explicit `error` from pipeline routing; `(output enqueue)` — output enqueue failure (PR-I). |
| `pipeline` | Pipeline name (`def pipeline <name>`). Empty for output-originated records (`(output ...)`, `(output ... shutdown)`, `(output enqueue)`); populated only when the record originates inside a pipeline. |
| `event.source` | Originating peer as `{ip, port}` object. Same shape as `tap --json` and as the DSL `source` ident. |
| `event.received_at` | i64 unix nanoseconds (matches OTLP `time_unix_nano`). Same shape as `tap --json`. |
| `event.ingress` | Original wire bytes. UTF-8-clean payloads serialise as a JSON string; non-UTF-8 payloads use the `$bytes_b64` marker the rest of the JSON layer already uses for `tap --json`. |

Example `reason` values across the discriminators: `"unknown identifier: timestamp"` (process runtime error), `"output retry exhausted after 5 attempts: connection refused"` (`(output <name>)`), `"output shutting down; draining queue"` (`(output <name> shutdown)`), `"output queue full; enqueue failed"` (`(output enqueue)`).

## Recovery paths into the DLQ

The DLQ receives records from four distinct paths. The `process` field above is the discriminator:

1. **Process runtime error / explicit `error`** — a `process` statement raised an error, or pipeline routing executed `error <expr?>`. `process` is the failed `def process` name (or `(inline)`), or `(pipeline)` for pipeline-level `error`. `pipeline` is populated. (Original behaviour.)
2. **Output retry exhausted** (PR-O) — an output exhausted its `retry` budget against the destination. `process = "(output <name>)"`, `pipeline` empty. The event carries the post-pipeline `egress` that the output attempted to deliver.
3. **Batched output shutdown drain** (PR-P) — a batched output (`otlp_*`, `http`) was shut down while events were still buffered and could not flush them. `process = "(output <name> shutdown)"`, `pipeline` empty. Synthetic-event metadata constraints apply: `received_at` reflects the shutdown moment for events that never carried their own; per-event source / workspace state may be a representative of the batch rather than per-record.
4. **Output enqueue failure** (PR-I) — the pipeline could not hand an event to an output's queue (queue full, output stopped). `process = "(output enqueue)"`, `pipeline` empty. The record preserves the event as it was at the pipeline → output boundary.

All four paths converge on the same JSONL file and the same replay recipe — `jq` on the `process` field selects which recovery path you are replaying.

### Recovery readiness check (`--check`)

Since 0.7.8 (PR-R), `limpid --check` emits a recovery-readiness warning when any output declares `retry`, declares a `secondary`, or is a batched OTLP/HTTP output and the `control { error_log }` is unset. Without `error_log`, recovery paths 2–4 above fall back to `tracing::error!` and the records are only recoverable through journald — sufficient for triage, but harder to replay. The warning catches the missing configuration before the first failure.

`event.egress` and `event.workspace` are intentionally **not** included — at the failure point they may hold partial state from earlier processes in the chain, which would confuse `inject --json` replay. The replay path re-runs the pipeline from scratch on `ingress`.

Format stability: pre-1.0 we may add new top-level fields, and existing keys may still be reshaped if the underlying DSL changes (the `event.source` field changed from a flat `"ip:port"` string to a `{ip, port}` object in v0.5.6, alongside the corresponding DSL change). After 1.0 the format will be locked.

## Replay

Once the offending config or parser is fixed, replay errored events with `jq` + `limpidctl inject --json`:

```bash
# Replay all errored events for one pipeline:
jq -c 'select(.pipeline == "journal_forward") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay everything:
jq -c '.event' /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay only failures of a specific process:
jq -c 'select(.process == "wrap_journal") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay events where the failure reason matches a pattern:
jq -c 'select(.reason | test("parse_json")) | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json
```

The `event` sub-object is exactly what `Event::from_json` (and therefore `inject --json`) needs to reconstruct a fresh Event: `egress` defaults to `ingress`, `workspace` starts empty. Replay is "as if the event just arrived for the first time" — no risk of partial-state confusion.

After replay, archive the DLQ file so the next failure window starts clean:

```bash
mv /var/log/limpid/errored.jsonl \
   /var/log/limpid/errored.jsonl.replayed-$(date +%Y%m%dT%H%M%S)
```

(Recreating the file is unnecessary — the daemon will recreate it on the next failure.)

## Rehearsing replay without the daemon

`limpid --test-pipeline` prints the JSONL record that *would* be written, on a synthetic event, after the trace:

```bash
$ echo 'sample event' \
    | limpid --test-pipeline journal_forward --config /etc/limpid/limpid.conf
=== Pipeline: journal_forward ===
[input] → ingress: <134>sample event
[process]  wrap_journal → error: process failed: unknown identifier: timestamp (event → error_log)

[error_log]  {"timestamp":"...","reason":"...","process":"wrap_journal","pipeline":"journal_forward","event":{"source":{"ip":"127.0.0.1","port":0},"received_at":...,"ingress":"<134>sample event"}}
```

This is useful for confirming the JSONL shape, the `pipeline` / `process` labels, and that the original ingress is captured correctly — all without booting the daemon or touching any file.

## When the DLQ write itself fails

`events_errored_unwritable` counts the cases where the daemon raised an error trying to write to the configured `error_log` file (disk full, permissions, NFS hiccup, rotation race). The runtime falls back to `tracing::error!` with the full JSONL record on the standard log channel so the data is still preserved — but this is alarm-level: a non-zero counter means the replay path may be incomplete, and the next failure may not have a corresponding line in the file.

Investigate immediately:

- Is the parent directory writable by the daemon user?
- Is the disk full? (`df`)
- Is a rotation tool deleting the file mid-write? (Switch the rotator to `copytruncate` or `nocreate`.)
- Is the file path on a network filesystem with intermittent connectivity?

Once the underlying issue is fixed, the next errored event lands in the file again and the counter stops increasing; existing records are unaffected.

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
| `process` | Failed process or recovery-path discriminator. A named `def process` invocation surfaces its name; an inline `process { ... }` block surfaces `(inline)`. For records originating from an output's or pipeline-skeleton's recovery path the value is one of five discriminators: `(output <name>)` — retry exhausted; `(output <name> shutdown)` — batched output's shutdown drain; `(pipeline)` — explicit `error` statement from pipeline routing; `(pipeline body)` — pipeline-skeleton expression eval failure (`if` condition, `switch` discriminant, `error <expr>` arg, or `process` function args); `(output enqueue)` — output enqueue failure. |
| `pipeline` | Pipeline name (`def pipeline <name>`). Empty for output-originated retry / shutdown records (`(output <name>)`, `(output <name> shutdown)`). Populated for the in-pipeline shapes: process runtime errors, `(pipeline)`, `(pipeline body)`, and `(output enqueue)` — the latter carries the name of the pipeline that failed to hand the event off. |
| `event.source` | Originating peer as `{ip, port}` object. Same shape as `tap --json` and as the DSL `source` ident. |
| `event.received_at` | i64 unix nanoseconds (matches OTLP `time_unix_nano`). Same shape as `tap --json`. |
| `event.ingress` | Original wire bytes. UTF-8-clean payloads serialise as a JSON string; non-UTF-8 payloads use the `$bytes_b64` marker the rest of the JSON layer already uses for `tap --json`. |

Example `reason` values across the discriminators: `"unknown identifier: timestamp"` (process runtime error), `"output retry exhausted after 5 attempts: connection refused"` (`(output <name>)`), `"output shutting down; draining queue"` (`(output <name> shutdown)`), `"unsupported subtype: vpc-flow-v3"` (`(pipeline)` — explicit `error <expr>`), `"unknown identifier: workspace.cef.severityy"` (`(pipeline body)` — expression eval in an `if`/`switch`/process-args slot), `"output enqueue failed for: mysink (queue closed, disk write error, or unknown output)"` (`(output enqueue)`).

## Recovery paths into the DLQ

The DLQ receives records from five distinct paths. The `process` field above is the discriminator:

1. **Process runtime error / explicit `error`** — a `process` statement raised an error, or pipeline routing executed `error <expr?>`. `process` is the failed `def process` name (or `(inline)`), or `(pipeline)` for pipeline-level `error`. `pipeline` is populated. (Original behaviour.)
2. **Pipeline-skeleton eval failure** — an expression embedded in the pipeline skeleton itself raised an error: an `if` condition, a `switch` discriminant, the argument of an explicit `error <expr>` statement, or one of the arguments passed to a `process` function call. These eval slots run outside of any `process { ... }` body, so the existing process-error path does not catch them; the orchestrator routes them through the same DLQ writer with `process = "(pipeline body)"`. `pipeline` is populated. The reason carries the underlying expression error verbatim.
3. **Output retry exhausted** — an output exhausted its `retry` budget against the destination. `process = "(output <name>)"`, `pipeline` empty. The event carries the post-pipeline `egress` that the output attempted to deliver.
4. **Batched output shutdown drain** — a batched output (`otlp_*`, `http`) was shut down while events were still buffered and could not flush them. `process = "(output <name> shutdown)"`, `pipeline` empty. Synthetic-event metadata constraints apply: `received_at` reflects the shutdown moment for events that never carried their own; per-event source / workspace state may be a representative of the batch rather than per-record.
5. **Output enqueue failure** — the pipeline could not hand an event to an output's queue (queue full, unknown output, disk-queue write error). `process = "(output enqueue)"`, and `pipeline` is the name of the pipeline that failed to enqueue — the only output-discriminator shape that keeps the originating pipeline name. The record preserves the event as it was at the pipeline → output boundary.

All five paths converge on the same JSONL file and the same replay recipe — `jq` on the `process` field selects which recovery path you are replaying.

### Recovery readiness check (`--check`)

Since 0.7.8, `limpid --check` emits a recovery-readiness warning when any output declares `retry` or is a batched OTLP/HTTP output and the `control { error_log }` is unset. Without `error_log`, recovery paths 3–5 above fall back to a `tracing::warn!`/`error!` line that names the output but **does not serialize the event payload** — the record itself is dropped and is not recoverable from journald. (Paths 1–2 are different: `write_errored_to_dlq` does emit the full JSONL on a tracing line when `error_log` is unset, so for process errors `journalctl | jq` still works as a fallback — just harder to replay than a dedicated file.) The warning catches the missing configuration before the first failure.

Since 0.7.8, the cursor a `tail` / `journal` input persists to its `state_file` advances on **pipeline-worker completion**, not on channel hand-off. A crash mid-processing now leaves the on-disk cursor pointing to the last *processed* line, so the next start re-reads any events that were in flight — closing the previous at-most-once gap and moving recovery toward at-least-once.

The five DLQ paths above are not a full safety net for output-side in-flight loss, though: path 4 only covers the shutdown-drain of a batched output, and path 5 only covers an *enqueue* failure. An event that was *successfully* enqueued to a memory queue and is still sitting in that queue (or being processed by the output worker) at the moment the process crashes is **not** recovered by either path — memory queues are not a durability layer, and the input cursor has already moved past the event because the pipeline worker acked it before the output queue handed it off. For full at-least-once across a process restart with in-flight queued events, configure a per-output **disk queue** — disk queues survive the crash and replay on restart. Memory-queue events in flight at crash time are lost.

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

## Replay and triage runbook

The basic `jq | limpidctl inject` recipe above is enough when you already know which records to replay and why they failed. In an incident — DLQ growth alert just fired, you don't yet know which of the four recovery paths is dominating, and the file has tens of thousands of lines — work through this runbook instead. It walks from "what is in the file" to "what to replay and what to fix first."

### 1. Reading the DLQ

Tail the live file with `jq` to watch new failures arrive:

```bash
tail -F /var/log/limpid/errored.jsonl | jq -c '{ts: .timestamp, process, reason}'
```

The `process` field is the single discriminator that tells you which of the five [recovery paths](#recovery-paths-into-the-dlq) emitted the record. A representative line per path:

```jsonc
// 1. Process runtime error / explicit `error` — `pipeline` is populated.
{"process":"wrap_journal","pipeline":"journal_forward","reason":"unknown identifier: timestamp", ...}
{"process":"(inline)","pipeline":"fortinet_in","reason":"parse_json failed: expected `}` at line 1 column 87", ...}
{"process":"(pipeline)","pipeline":"audit_in","reason":"unsupported subtype: vpc-flow-v3", ...}

// 2. Pipeline-skeleton eval failure — `pipeline` is populated.
{"process":"(pipeline body)","pipeline":"journal_forward","reason":"unknown identifier: workspace.cef.severityy", ...}

// 3. Output retry exhausted — `pipeline` empty.
{"process":"(output mysink)","pipeline":"","reason":"output write failed after 5 attempts: connection refused", ...}

// 4. Batched output shutdown drain — `pipeline` empty.
{"process":"(output otlp_main shutdown)","pipeline":"","reason":"shutdown flush failed: deadline exceeded", ...}

// 5. Output enqueue failure — `pipeline` carries the originating pipeline.
{"process":"(output mysink)","pipeline":"mypipe","reason":"output enqueue failed for: mysink (queue closed, disk write error, or unknown output)", ...}
```

Note the asymmetry: paths 3 and 4 have an empty `pipeline` field, because by the time the event reached the output it had already left its source pipeline. Paths 1, 2, and 5 keep `pipeline` populated. For path-3/4 records, filter on `process` (not `pipeline`). For path-5 records, either filter is valid — `process == "(output enqueue)"` scopes by the recovery shape, `pipeline == "<name>"` scopes by the originating pipeline.

### 2. Triage flow

**Step 1 — aggregate by path.** The first question is always "what is dominating the file?":

```bash
jq -r '.process' /var/log/limpid/errored.jsonl | sort | uniq -c | sort -rn
```

Pair it with a reason breakdown for the top offender:

```bash
jq -r 'select(.process == "(output mysink)") | .reason' \
    /var/log/limpid/errored.jsonl | sort | uniq -c | sort -rn | head
```

**Step 2 — time-bucketed growth.** Is the failure ongoing or did it stop? Bucket the last hour by minute:

```bash
jq -r '.timestamp[:16]' /var/log/limpid/errored.jsonl \
    | awk -v cutoff="$(date -u -v-1H +%Y-%m-%dT%H:%M 2>/dev/null \
                       || date -u -d '1 hour ago' +%Y-%m-%dT%H:%M)" \
        '$0 >= cutoff' \
    | uniq -c
```

A flat curve over the last 5 minutes means the upstream issue resolved itself — you are now only dealing with cleanup. A still-climbing curve means fix the root cause before you replay anything, or you will land the same events back in the DLQ.

**Step 3 — root-cause heuristics by path.**

| Discriminator | Most common causes | First place to look |
|---|---|---|
| `(output <name>)` (retry exhausted) | backend down, network partition, auth expired, sustained backpressure | downstream health, `events_failed` / `retries` on the output ([Metrics](./metrics.md)) |
| `(output <name> shutdown)` | daemon `kill -9` or restart while a batched output (`otlp_*`, `http`) still had buffered events | `journalctl -u limpid` around the shutdown window |
| `(output enqueue)` | output queue full, output task stopped, disk-backed queue write error | output `events_received` vs. `events_written`, disk space, queue config |
| `(pipeline body)` | typo in an `if` condition / `switch` discriminant, undefined workspace key referenced from a `process` arg or an `error <expr>` slot | the `reason` string + the pipeline DSL around the failing expression |
| `<pipeline_process_name>` / `(inline)` / `(pipeline)` | DSL bug, parser blowup on a new input shape, missing-required-field contract violation | the `reason` string + the offending `event.ingress` payload |

**Step 4 — transient vs. permanent.** Replay is only safe for *transient* causes — the kind where re-running the pipeline now will succeed. Apply this rule:

- **Transient (replay fixes it):** backend was down and is back, daemon was restarted, queue was full and has drained, a config typo was corrected.
- **Permanent (fix config first, then replay):** DSL bug, parser cannot handle a new vendor format, output points at the wrong destination. Replaying without the fix just re-fills the DLQ.

If you cannot tell, pull *one* record, run it through `limpid --test-pipeline` (see [Rehearsing replay without the daemon](#rehearsing-replay-without-the-daemon)), and confirm it now succeeds before mass-replaying.

### 3. Replay

The [basic recipes](#replay) above cover the common shape. A few patterns worth calling out for incident use:

```bash
# Path-scoped replay: only retry-exhausted records for one output.
jq -c 'select(.process == "(output mysink)") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Time-windowed replay: only failures after the fix landed at 14:05 UTC.
jq -c 'select(.timestamp >= "2026-04-27T14:05:00Z") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Reason-pattern replay: only the specific bug class you just fixed.
jq -c 'select(.reason | test("parse_json")) | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json
```

**Shutdown-drain caveat.** Records with `process` ending in `shutdown` carry partly synthetic metadata: `event.source` is `127.0.0.1:0` and `event.received_at` is the shutdown wall-clock, not the original arrival time. The `event.ingress` field on these records is the *rendered* batched-output payload (already wrapped, encoded, batched), not the wire bytes that originally entered the pipeline. Re-injecting them:

- stamps a new `received_at` from the inject time,
- replaces the original source IP/port with `127.0.0.1:0` (or whatever the receiving input synthesizes),
- and runs the pipeline again *on the already-rendered payload*, which is almost never what you want for a stateful wrap/format process.

For shutdown-drain records, prefer `limpidctl inject output <name>` (direct queue inject, bypassing the pipeline) so the payload goes straight back to the downstream destination as captured. Use input replay only when you are sure the pipeline is idempotent on its own output.

**Fail-fast pilot.** Before any large replay, validate the fix on a single record:

```bash
jq -c 'select(.process == "(output mysink)") | .event' \
    /var/log/limpid/errored.jsonl \
    | head -1 \
    | limpidctl inject input <input_name> --json
```

Tap the input or the failing process ([Debug Tap](./tap.md)) in another shell to watch what happens. If the one record succeeds, fan out to the full file; if it fails the same way, stop and fix the root cause before continuing.

**Archive before replaying again.** Always rotate or move the DLQ file after a replay batch — otherwise the next replay will double-process everything that was already replayed and succeeded:

```bash
mv /var/log/limpid/errored.jsonl \
   /var/log/limpid/errored.jsonl.replayed-$(date -u +%Y%m%dT%H%M%SZ)
```

### 4. Operational concerns

The daemon does **not** bound the DLQ file size — that is operator responsibility, and a stuck failure mode can fill the disk in minutes. Watch it:

```bash
du -h /var/log/limpid/errored.jsonl
wc -l /var/log/limpid/errored.jsonl
```

For rotation, see the [Recommended `logrotate` configuration](#recommended-logrotate-configuration) above. A retention policy on the rotated archives — for example, compress after 1 day, ship to long-term storage at 7 days, delete at 30 — sits on top of that `logrotate` block via a separate cron / archival job; it is intentionally not embedded in `logrotate` itself because retention is environment-specific (compliance hold, cold-storage budget, replay window) and should not be coupled to in-process rotation.

**Alerting on DLQ growth.** The metric to alert on is `events_errored` (pipeline metric), and `events_errored_unwritable` is the secondary alarm that the DLQ writer itself is failing — see [Metrics](./metrics.md) for the full list. A Prometheus rule:

```promql
# Sustained failure: more than 10 events/sec into the DLQ for 5 minutes.
rate(limpid_pipeline_events_errored_total[5m]) > 10

# Alarm: the DLQ writer itself can't write — replay is partial.
increase(limpid_pipeline_events_errored_unwritable_total[5m]) > 0
```

(Confirm the exact exported metric names against your `limpid-prometheus` exporter; the in-process counter names are `events_errored` and `events_errored_unwritable`.)

**Multi-instance DLQ aggregation.** When several limpid daemons each write their own `error_log` file, central triage means shipping each file to a single host and replaying from there. The simple shape: a `filebeat` / `vector` / `rsyslog` collector tails each daemon's DLQ and forwards the lines into a central archive bucket; replay runs against a `jq` query over the aggregated archive and injects back into the daemon whose `pipeline` field matches. Detailed multi-instance topology is deferred to a separate runbook.

### 5. Anti-patterns and pitfalls

- **Do not use the DLQ as a general log channel.** It exists for replayable failures. Routing successful events into it (via `error "..."` on the happy path, or by mis-configuring a process to always raise) makes the file grow without bound and buries the real failures.
- **Do not blind-replay shutdown-drain records into an input.** The metadata is synthetic and the `ingress` is the rendered batched-output payload (see the caveat under [Replay](#3-replay)). Use `limpidctl inject output <name>` for those, or verify the downstream is in a state where pipeline re-processing of the rendered payload is safe.
- **Do not replay before you fix the root cause.** A still-broken pipeline turns a 10 000-line DLQ into a 20 000-line DLQ — `events_errored` keeps climbing because every replayed event fails the same way it did the first time.
- **Do not skip the pilot.** Even a "trivial" config fix has rolled back into the DLQ at scale because something downstream — a quota, a stale auth token, a per-IP rate limit — kicks in only on the bulk traffic. One-record pilot, then fan out.
- **Do not replay without archiving the source file.** Without `mv`-ing it aside, the next replay rerun re-processes everything, including the records you just successfully replayed; you cannot tell from the DLQ alone which records have already been re-injected.
- **Do not let the DLQ file grow unbounded.** Pair it with `logrotate` from day one, alert on `events_errored_unwritable`, and treat a non-zero unwritable counter as a P1 — the disk filled up, the directory permissions changed, or NFS dropped, and the next failure may be lost.

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

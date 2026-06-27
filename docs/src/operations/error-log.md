# Error Log (Dead-Letter Queue)

When a `process` statement raises a runtime error — unknown identifier, type mismatch, regex compile failure, parser blowup on malformed input — the event is set aside in a **dead-letter queue (DLQ)** rather than forwarded with the original `ingress` unchanged. The same DLQ also receives events routed by an explicit [`error` statement](../pipelines/drop-finish-error.md#error): a snippet parser dispatcher hitting an unsupported subtype, or a process detecting a missing-required-field contract violation, can call `error "..."` to land the event in the DLQ with an operator-authored reason. Operators audit the failures, fix the offending config or parser, and replay the events.

Output-side failures (retry budget exhausted, shutdown-drain leftovers, enqueue failures) land in the same file, with a different record shape that preserves the pre-rendered wire payload — so the replay tooling can hand them straight back to the sink without re-running the whole pipeline.

This page covers the on-disk format, the `control { error_log "..." }` opt-in, and the replay recipes. The corresponding metrics (`events_errored`, `events_errored_unwritable`) are documented under [Metrics](./metrics.md).

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

The runtime cannot guess what the operator intended at the failure point — `egress` may have been partially rewritten by earlier processes in the chain, the next process expected a workspace key that was never produced, etc. So the DLQ deliberately preserves only the *original* event (ingress / source / received_at) for the Process flavor, and the pre-rendered wire payload (with `egress`) for the Output flavor, and lets the operator re-run from the appropriate boundary after the fix.

## Configuring the DLQ

The DLQ file is opt-in via the `control { error_log "..." }` property:

```limpid
control {
    socket    "/var/run/limpid/control.sock"
    error_log "/var/log/limpid/errored.jsonl"
}
```

When unset, the fallback differs by flavor:

- **Process flavor** — the daemon emits a `tracing::error!` line that includes the full failure JSONL inline, so the data is still preserved on the standard log channel. Replay is awkward (you have to `jq` over journald, no `limpidctl inject` shortcut) but the original event is recoverable.
- **Output flavor** — the daemon emits a `tracing::warn!` / `error!` line that names the output and the reason, but **does not serialize the event payload**. The record is effectively dropped; it is not recoverable from journald. The retry-exhaustion, shutdown-drain, and enqueue-failure paths all behave this way when `error_log` is unset.

For any pipeline that uses `retry { ... }` or a batched output (`http`, `otlp_http`, `otlp_grpc`), `limpid --check` raises a recovery-readiness warning when `error_log` is not configured; see [Recovery readiness check](#recovery-readiness-check---check) below.

The path must be in a directory the daemon user can write to. limpid validates this at startup (`--check` and daemon start both fail with a clear message if the parent directory is missing or non-writable).

### Recommended `logrotate` configuration

A typical operator setup keeps the live file capped and the rotated archives compressed:

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

Each line is a sum-typed JSON record. Since 0.7.8 the record carries an explicit `schema_version: 2` and a `kind` discriminator (`"process"` or `"output"`) that selects a per-kind block at the top level. Process flavor records carry `process: { name }` and a minimal event (ingress only); Output flavor records carry `output: { name }` and the pre-rendered `event.egress`.

### Process flavor

A pipeline-side failure (process body raised, pipeline-skeleton eval failed, explicit `error <expr>`) emits a Process record. Replay re-enters at the input layer — the pipeline is re-run from scratch against the original ingress bytes.

```json
{
  "schema_version": 2,
  "timestamp": "2026-04-27T03:28:39.178046123Z",
  "reason": "unknown identifier: timestamp",
  "pipeline": "journal_forward",
  "kind": "process",
  "process": { "name": "wrap_journal" },
  "event": {
    "source": {"ip": "10.0.0.1", "port": 514},
    "received_at": 1745719719178046000,
    "ingress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello"
  }
}
```

### Output flavor

A sink-side failure (retry budget exhausted, batched-output shutdown drain, runtime-side enqueue failure) emits an Output record. Replay hands the pre-rendered payload directly to the named output's queue — the sink re-routes via its own `consume()` path, no pipeline re-run.

```json
{
  "schema_version": 2,
  "timestamp": "2026-04-27T03:31:02.998742000Z",
  "reason": "output write failed after 5 attempts: connection refused",
  "pipeline": "",
  "kind": "output",
  "output": { "name": "mysink" },
  "event": {
    "source": {"ip": "10.0.0.1", "port": 514},
    "received_at": 1745719719178046000,
    "ingress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello",
    "egress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello\n"
  }
}
```

### Common fields

| Field | Meaning |
|-------|---------|
| `schema_version` | Integer `2`. Identifies the v2 sum-typed shape; v1 is a hard break (see [Schema migration v1 → v2](#schema-migration-v1--v2) below). |
| `timestamp` | RFC3339 with nanosecond precision; wall-clock at which the failure was raised. |
| `reason` | Stringified failure reason. Stable enough for `grep` / classification but not a stable API. The runbook below maps reason patterns back to producer sites. |
| `pipeline` | Pipeline name (`def pipeline <name>`). Populated for every Process record; for Output records it carries the originating pipeline only when the failure happened *at the pipeline → output boundary* (= enqueue failure). Retry-exhausted and shutdown-drain Output records have an empty `pipeline` field because the event had already left its source pipeline by then. |
| `kind` | Discriminator: `"process"` or `"output"`. Selects which per-kind block is present and which `event.*` fields are populated. |
| `event.source` | Originating peer as `{ip, port}` object. Same shape as `tap --json` and as the DSL `source` ident. |
| `event.received_at` | i64 unix nanoseconds (matches OTLP `time_unix_nano`). Same shape as `tap --json`. |
| `event.ingress` | Original wire bytes. UTF-8-clean payloads serialise as a JSON string; non-UTF-8 payloads use the `$bytes_b64` marker the rest of the JSON layer already uses for `tap --json`. |

### Process-flavor extras

| Field | Meaning |
|-------|---------|
| `process` | `{ "name": "<site>" }` block. `name` is the failing `def process` name, `(inline)` for an inline `process { ... }` block, `(pipeline)` for a pipeline-statement `error <expr>`, or `(pipeline body)` for a pipeline-skeleton expression failure (`if` / `switch` / `error <expr>` arg / `process` function args). |

`event` carries only `{ source, received_at, ingress }` — no `egress`, no `workspace`. Replay re-runs the pipeline from scratch on `ingress`.

### Output-flavor extras

| Field | Meaning |
|-------|---------|
| `output` | `{ "name": "<output_name>" }` block. `name` is the `def output` name. The flavor is intentionally address-free — no peer, endpoint, partition, topic, path, URL, target, or workspace fragment leaks into the record. Replay hands the event back to the sink via `limpidctl inject output <name>`, and the sink re-routes via its own `consume()` path. |
| `event.egress` | Pre-rendered wire bytes (= what the sink attempted to deliver). Same UTF-8 / `$bytes_b64` encoding rules as `ingress`. The sink interprets `egress` as already-encoded payload (no re-render). |

`workspace` is intentionally **not** serialised on Output records — sink-side routing state is forbidden from the recovery record by design. See [Address-free Output records](#address-free-output-records) below for the rationale.

### Producer sites

Seven producer sites map to the two flavors. The runbook section below maps each one to its triage signal (`reason` pattern) and most common causes.

Process flavor (4 sites — replay via `inject input`):

1. **`<process_name>`** — an explicit `process` body raised via `error <expr>` or a process-internal failure.
2. **`(inline)`** — an inline `process { ... }` block raised the same way.
3. **`(pipeline body)`** — an `if` condition / `switch` discriminant / explicit `error <expr>` argument / `process` function argument failed to evaluate before reaching a process body.
4. **`(pipeline)`** — `error <expr>` at the pipeline (statement) level raised, including the dispatcher pattern where a snippet routes on an unrecognised subtype.

Output flavor (3 sites — replay via `inject output`):

5. **`<output_name>`** — the output exhausted its `retry { ... }` budget against the destination. A batched output's per-event render failure inside `flush()` is also routed here with `reason = "render failed during batch flush: ..."`. `pipeline` is empty.
6. **`<output_name> shutdown`** — a batched output (`http`, `otlp_http`, `otlp_grpc`) was shut down with events still buffered and the final flush failed. The `shutdown()` impl walks the remaining `(Event, QueueAckHandle)` buffer entries (one record per parked event) through this writer. `pipeline` is empty. The per-event `source`, `received_at`, `ingress`, and `egress` come from the original `Event` that was parked in the buffer at `consume()` time; nothing is synthesised. (Earlier 0.7.7 drafts of this flavor carried synthetic shutdown-time metadata; the 0.7.8 ack-lifecycle work parks the source `Event` alongside the ack handle so each shutdown-drain record now reflects the real per-event provenance.)
7. **`<output_name> enqueue`** — `runtime.rs` could not hand an event to the named output's queue (queue closed, disk-queue write error, unknown output). `pipeline` is the name of the originating pipeline — the only Output-flavor site that keeps it populated. Per-failed-output split: a single pipeline-eval result with N failed-output enqueues produces N records (one per failing output).

The `reason` field distinguishes sites 5 / 6 / 7 within a single output name: retry exhaustion uses `"output write failed after N attempts: ..."`, shutdown drain uses `"shutdown flush failed: ..."`, enqueue failure uses `"output enqueue failed (queue closed, disk write error, or unknown output)"`. A batched output's per-event render failure inside `flush()` uses `"render failed during batch flush: ..."`. The runbook's [root-cause heuristics table](#step-3--root-cause-heuristics-by-site) lists the full patterns.

### Address-free Output records

The Output record carries `output: { name }` and nothing more. No peer address, endpoint URL, partition, topic, path, target, key, or workspace fragment leaks into the recovery record.

Why: replay calls `limpidctl inject output <name>`, which hands the event back to the sink. The sink re-routes via the same `consume()` path it uses for live traffic — round-robin peer selection, retry budget, batching, headers, all of it. There is no "send this exact record to this exact peer" mode; the sink owns routing. Carrying address details on the record would create two failure modes: (a) replay vs. live diverging when the operator updates the sink config between failure and replay, and (b) sensitive routing metadata (peer hostnames, internal endpoint URLs) leaking into the DLQ file.

The trade-off: an operator who needs to know *which peer failed* for an `<output_name>` retry-exhaustion record reads the daemon log (`journalctl -u limpid` around the timestamp), not the DLQ record.

### Schema stability

`schema_version: 2` is the operator-visible discriminator. Pre-1.0, the schema may add fields to existing kinds, or add new kinds, both of which bump `schema_version`. Field renames within an existing kind also bump it. After 1.0 the format will be locked under semantic versioning.

`event.source` changed shape from a flat `"ip:port"` string to a `{ip, port}` object in v0.5.6 (independent of `schema_version`, but worth knowing if you're reading captures from that era).

### Recovery readiness check (`--check`)

Since 0.7.8, `limpid --check` emits a recovery-readiness warning when any output declares `retry` or is a batched OTLP/HTTP output and `control { error_log }` is unset. Without `error_log`, the Output-flavor recovery paths (retry exhausted, shutdown drain, enqueue failure) fall back to a `tracing::warn!` / `error!` line that names the output but **does not serialize the event payload** — the record itself is dropped and is not recoverable from journald. (Process-flavor records are different: the runtime emits the full JSONL on the tracing line when `error_log` is unset, so for process errors `journalctl | jq` still works as a fallback — just harder to replay than a dedicated file.) The warning catches the missing configuration before the first failure.

Since 0.7.8, the cursor a `tail` / `journal` input persists to its `state_file` advances on **pipeline-worker completion**, not on channel hand-off. A crash mid-processing now leaves the on-disk cursor pointing to the last *processed* line, so the next start re-reads any events that were in flight — closing the previous at-most-once gap and moving recovery toward at-least-once.

The seven producer sites above are not a full safety net for output-side in-flight loss. Sites 5 / 6 only cover the events the output explicitly resolved as failures; an event that was *successfully* enqueued to a memory queue and is still sitting in that queue (or being processed by the output worker) at the moment the process crashes is **not** recovered by any of them — memory queues are not a durability layer. For full at-least-once across a process restart with in-flight queued events, configure a per-output **disk queue** — disk queues survive the crash and replay on restart. Memory-queue events in flight at crash time are lost.

## Replay

Once the offending config or parser is fixed, replay errored records with `jq` + `limpidctl inject`. The recipe depends on the flavor: Process records replay through the input layer (full pipeline re-run), Output records replay through the named output's queue (sink re-route only).

### Process flavor — replay via `inject input`

```bash
# Replay all errored events for one pipeline:
jq -c 'select(.kind == "process" and .pipeline == "journal_forward") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay all Process-flavor records (any pipeline):
jq -c 'select(.kind == "process") | .event' /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay only failures of a specific process site:
jq -c 'select(.kind == "process" and .process.name == "wrap_journal") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Replay events where the failure reason matches a pattern:
jq -c 'select(.kind == "process" and (.reason | test("parse_json"))) | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json
```

The `event` sub-object is exactly what `Event::from_json` (and therefore `inject input --json`) needs to reconstruct a fresh Event: `egress` defaults to `ingress`, `workspace` starts empty. Replay is "as if the event just arrived for the first time" — no risk of partial-state confusion.

### Output flavor — replay via `inject output`

```bash
# Replay all retry-exhausted records for one output:
jq -c 'select(.kind == "output" and .output.name == "mysink") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject output mysink --json

# Replay all Output-flavor records, fanned out by output name:
jq -c 'select(.kind == "output") | "\(.output.name)\t\(.event | @json)"' \
    /var/log/limpid/errored.jsonl \
    | while IFS=$'\t' read -r name event_json; do
        echo "$event_json" | limpidctl inject output "$name" --json
    done
```

The `event` sub-object for Output records carries `egress` (the pre-rendered wire bytes the sink had tried to deliver). `inject output --json` hands this straight to the sink's `consume()` — no re-rendering, no pipeline re-run. The sink applies its current peer/retry/batching configuration as if the event had just been enqueued normally.

### Archive after replaying

After replay, archive the DLQ file so the next failure window starts clean:

```bash
mv /var/log/limpid/errored.jsonl \
   /var/log/limpid/errored.jsonl.replayed-$(date -u +%Y%m%dT%H%M%SZ)
```

(Recreating the file is unnecessary — the daemon will recreate it on the next failure.)

## Replay and triage runbook

The recipes above are enough when you already know which records to replay and why they failed. In an incident — DLQ growth alert just fired, you don't yet know which producer site is dominating, and the file has tens of thousands of lines — work through this runbook instead. It walks from "what is in the file" to "what to replay and what to fix first."

### 1. Reading the DLQ

Tail the live file with `jq` to watch new failures arrive:

```bash
tail -F /var/log/limpid/errored.jsonl | jq -c '{ts: .timestamp, kind, name: (.process.name // .output.name), reason}'
```

The `kind` field + the per-kind `name` together identify the producer site at a glance.

A representative line per site, condensed:

```jsonc
// Process flavor (kind="process", populated `pipeline`, ingress-only event)
{"kind":"process","process":{"name":"wrap_journal"},"pipeline":"journal_forward","reason":"unknown identifier: timestamp", ...}
{"kind":"process","process":{"name":"(inline)"},"pipeline":"fortinet_in","reason":"parse_json failed: expected `}` at line 1 column 87", ...}
{"kind":"process","process":{"name":"(pipeline)"},"pipeline":"audit_in","reason":"unsupported subtype: vpc-flow-v3", ...}
{"kind":"process","process":{"name":"(pipeline body)"},"pipeline":"journal_forward","reason":"unknown identifier: workspace.cef.severityy", ...}

// Output flavor (kind="output", pipeline empty for retry / shutdown, populated for enqueue, egress present)
{"kind":"output","output":{"name":"mysink"},"pipeline":"","reason":"output write failed after 5 attempts: connection refused", ...}
{"kind":"output","output":{"name":"otlp_main"},"pipeline":"","reason":"shutdown flush failed: deadline exceeded", ...}
{"kind":"output","output":{"name":"mysink"},"pipeline":"mypipe","reason":"output enqueue failed (queue closed, disk write error, or unknown output)", ...}
```

Note the asymmetry on `pipeline`: Process records always populate it; Output records populate it only for enqueue failures (the originating pipeline is known at that boundary), and leave it empty for retry exhaustion and shutdown drain (the event had already left its source pipeline).

### 2. Triage flow

**Step 1 — aggregate by site.** The first question is always "what is dominating the file?":

```bash
# Group by (kind, name, reason-pattern) — the three things that uniquely identify a site.
jq -r '[.kind, (.process.name // .output.name), (.reason | split(":")[0])] | @tsv' \
    /var/log/limpid/errored.jsonl \
    | sort | uniq -c | sort -rn | head
```

Pair it with a per-site reason breakdown for the top offender:

```bash
jq -r 'select(.kind == "output" and .output.name == "mysink") | .reason' \
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

**Step 3 — root-cause heuristics by site.**

| `kind` | `name` | `reason` pattern | Most common causes | First place to look |
|---|---|---|---|---|
| `output` | `<output_name>` | `output write failed after N attempts: …` | backend down, network partition, auth expired, sustained backpressure | downstream health, `events_failed` / `retries` on the output ([Metrics](./metrics.md)) |
| `output` | `<output_name>` | `render failed during batch flush: …` | render bug, missing workspace key the renderer depended on (rare; deterministic) | the failing process / template, not the output |
| `output` | `<output_name>` | `shutdown flush failed: …` | daemon `kill -9` or restart while a batched output (`otlp_*`, `http`) still had buffered events | `journalctl -u limpid` around the shutdown window |
| `output` | `<output_name>` | `output enqueue failed (queue closed, disk write error, or unknown output)` | output queue full, output task stopped, disk-backed queue write error, unknown output name in the pipeline body | output `events_received` vs `events_written`, disk space, queue config |
| `process` | `(pipeline body)` | varies | typo in an `if` condition / `switch` discriminant, undefined workspace key referenced from a `process` arg or an `error <expr>` slot | the `reason` string + the pipeline DSL around the failing expression |
| `process` | `<process_name>` / `(inline)` / `(pipeline)` | varies | DSL bug, parser blowup on a new input shape, missing-required-field contract violation | the `reason` string + the offending `event.ingress` payload |

**Step 4 — transient vs. permanent.** Replay is only safe for *transient* causes — the kind where re-running now will succeed. Apply this rule:

- **Transient (replay fixes it):** backend was down and is back, daemon was restarted, queue was full and has drained, a config typo was corrected.
- **Permanent (fix config first, then replay):** DSL bug, parser cannot handle a new vendor format, output points at the wrong destination. Replaying without the fix just re-fills the DLQ.

If you cannot tell, pull *one* record, run it through `limpid --test-pipeline` (see [Rehearsing replay without the daemon](#rehearsing-replay-without-the-daemon)), and confirm it now succeeds before mass-replaying.

### 3. Replay

The [basic recipes](#replay) above cover the common shapes. A few patterns worth calling out for incident use:

```bash
# Site-scoped replay: only retry-exhausted records for one output.
jq -c 'select(.kind == "output" and .output.name == "mysink") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject output mysink --json

# Time-windowed replay: only failures after the fix landed at 14:05 UTC.
jq -c 'select(.kind == "process" and .timestamp >= "2026-04-27T14:05:00Z") | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json

# Reason-pattern replay: only the specific bug class you just fixed.
jq -c 'select(.kind == "process" and (.reason | test("parse_json"))) | .event' \
    /var/log/limpid/errored.jsonl \
    | limpidctl inject input <input_name> --json
```

**Shutdown-drain caveat.** Output-flavor records with `reason` starting with `shutdown flush failed: ...` are *per-event* records, not per-batch — the batched output's shutdown helper walks every still-buffered `(Event, QueueAckHandle)` entry and writes one record per parked event, so `event.source`, `event.received_at`, `event.ingress`, and `event.egress` all reflect the original per-event provenance (no synthetic shutdown-time metadata). `event.egress` carries the per-event pre-rendered payload (= the bytes the output had built for the wire on each event, before the unsent batch wrapper was applied), so `inject output <name>` is the correct replay path — the sink takes the per-event pre-rendered bytes and re-routes via its `consume()` path, applying the current batch wrapper / headers / compression as if the event had just been enqueued. Do **not** route shutdown-drain records through `inject input <name>`: doing so feeds the per-event pre-rendered payload back into the pipeline as raw `ingress`, which is almost never what you want.

**OTLP `partial_success` attribution caveat.** For `output otlp_http` and `output otlp_grpc`, Output-flavor records with `reason == "collector reported partial_success rejection"` are an **approximate** attribution of a batch-level rejection: the OTLP response carries a rejected *count*, not the identity of each rejected log record. limpid splits the batch into Delivered + Recovered along the trailing N entries (where N = `rejected_log_records`) and writes one DLQ record per Recovered tail entry, but the collector did not identify those exact events. Metric totals (`events_written`, `events_failed`) are accurate; per-event provenance in `event.*` is correct (it is the original Event); the *attribution* — which specific event was rejected — is not. For replay purposes treat these records as a batch-level rejection split into per-event records, not as proof of which records the collector rejected. `inject output <name>` still works the same way; if the underlying cause is a payload-shape issue, the rejection will simply re-occur on the rejected subset of the replay (and the same approximate split will apply on the new response).

**Fail-fast pilot.** Before any large replay, validate the fix on a single record:

```bash
jq -c 'select(.kind == "output" and .output.name == "mysink") | .event' \
    /var/log/limpid/errored.jsonl \
    | head -1 \
    | limpidctl inject output mysink --json
```

Tap the output ([Debug Tap](./tap.md)) in another shell to watch what happens. If the one record succeeds, fan out to the full file; if it fails the same way, stop and fix the root cause before continuing.

**Archive before replaying again.** Always rotate or move the DLQ file after a replay batch — otherwise the next replay rerun re-processes everything, including the records you just successfully replayed:

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

**Alerting on DLQ growth.** Two counters together cover both flavors: pipeline-side failures (Process records + the enqueue-failure subset of Output records) bump `events_errored` on the pipeline, while sink-side failures (retry exhausted, shutdown drain, batched-output per-event render failures) bump `events_failed` on the originating output. `events_errored_unwritable` is the secondary alarm that the DLQ writer itself is failing — see [Metrics](./metrics.md) for the full list. A Prometheus rule:

```promql
# Sustained pipeline-side failure (Process flavor + output enqueue):
# more than 10 events/sec into the DLQ for 5 minutes.
rate(limpid_pipeline_events_errored_total[5m]) > 10

# Sustained sink-side failure (Output flavor retry / shutdown / render):
# any single output averaging > 10 failures/sec for 5 minutes.
rate(limpid_output_events_failed_total[5m]) > 10

# Alarm: the DLQ writer itself can't write — replay is partial.
increase(limpid_pipeline_events_errored_unwritable_total[5m]) > 0
```

(Confirm the exact exported metric names against your `limpid-prometheus` exporter; the in-process counter names are `events_errored`, `events_failed` (per output), and `events_errored_unwritable`.)

**Multi-instance DLQ aggregation.** When several limpid daemons each write their own `error_log` file, central triage means shipping each file to a single host and replaying from there. The simple shape: a `filebeat` / `vector` / `rsyslog` collector tails each daemon's DLQ and forwards the lines into a central archive bucket; replay runs against a `jq` query over the aggregated archive and injects back into the daemon whose `pipeline` (Process flavor) or `output.name` (Output flavor) matches. Detailed multi-instance topology is deferred to a separate runbook.

### 5. Anti-patterns and pitfalls

- **Do not use the DLQ as a general log channel.** It exists for replayable failures. Routing successful events into it (via `error "..."` on the happy path, or by mis-configuring a process to always raise) makes the file grow without bound and buries the real failures.
- **Do not feed Output-flavor records into `inject input`.** The `event.egress` is already-rendered batched-output bytes — re-running the pipeline on it almost never produces a useful result. Use `inject output <name>` for Output records and `inject input <name>` for Process records.
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

[error_log]  {"schema_version":2,"timestamp":"...","reason":"...","pipeline":"journal_forward","kind":"process","process":{"name":"wrap_journal"},"event":{"source":{"ip":"127.0.0.1","port":0},"received_at":...,"ingress":"<134>sample event"}}
```

This is useful for confirming the JSONL shape, the `kind` / per-kind name discriminator, and that the original ingress is captured correctly — all without booting the daemon or touching any file. The Output-flavor shape can be observed by triggering a sink retry exhaustion against an unroutable peer; `--test-pipeline` does not directly emit Output records (it stops at pipeline-side disposition).

## When the DLQ write itself fails

`events_errored_unwritable` counts the cases where the daemon raised an error trying to write to the configured `error_log` file (disk full, permissions, NFS hiccup, rotation race). The runtime falls back to `tracing::error!` with the full JSONL record on the standard log channel so the data is still preserved — but this is alarm-level: a non-zero counter means the replay path may be incomplete, and the next failure may not have a corresponding line in the file.

Investigate immediately:

- Is the parent directory writable by the daemon user?
- Is the disk full? (`df`)
- Is a rotation tool deleting the file mid-write? (Switch the rotator to `copytruncate` or `nocreate`.)
- Is the file path on a network filesystem with intermittent connectivity?

Once the underlying issue is fixed, the next errored event lands in the file again and the counter stops increasing; existing records are unaffected.

## Schema migration v1 → v2

The 0.7.8 release introduces `schema_version: 2` as a **hard break**. v1 records (no `schema_version`, top-level `process` string field as the discriminator, no `kind`, no per-kind block) are not readable by v2 tooling, and v2 records are not readable by v1 tooling.

The break was necessary because v1 conflated two unrelated failure shapes into one `process` field — pipeline-side failures (`wrap_journal`, `(inline)`, `(pipeline body)`, `(pipeline)`) and sink-side failures (`(output mysink)`, `(output mysink shutdown)`, and the special enqueue form `(output enqueue)` whose output name lived only inside the `reason` string). Operators replaying the file had to special-case the `(output ...)` prefix and pick a different `limpidctl inject` flag for each, with no machine-readable signal — just an in-band string convention. v2 lifts the flavor into a `kind` discriminator and gives each flavor its own block, so replay tooling can `select(.kind == "output")` and route to `inject output` deterministically.

### What to do with pre-0.7.8 captures

The recommended path is **archive, don't migrate**. v1 capture files predate the recovery-readiness gates, the unified `event.egress` carry, and (for Output-flavor records) the `inject output` replay command itself — so replaying them under the new daemon is operationally weaker than letting the 0.7.8 daemon re-fail any still-affected events into the new v2 file.

```bash
# At upgrade time: rotate the existing v1 file aside.
mv /var/log/limpid/errored.jsonl \
   /var/log/limpid/errored.jsonl.v1.archived-$(date -u +%Y%m%dT%H%M%SZ)
```

The 0.7.8 daemon recreates `errored.jsonl` on the first v2 failure.

### If you must replay a v1 capture

When a specific incident requires replaying a v1 file (e.g. a known-good backend that was down during the upgrade window has come back), you can split it into Process and Output halves with `jq` and translate each:

v1 used three different `process`-field encodings for the three sink-side sites:

- `(output <name>)` — retry exhausted; the bare output name is recoverable from the parenthetical.
- `(output <name> shutdown)` — shutdown drain; same, with a `" shutdown"` suffix.
- `(output enqueue)` — enqueue failure; **the output name does not appear in `process`**. It is buried inside the `reason` string (`output enqueue failed for: <names> (queue closed, disk write error, or unknown output)`), and a single pipeline-eval result with multiple failing outputs would produce one v1 record naming every failed output in `reason`, not one record per failing output. v2 records each failing output separately, so a faithful round-trip means parsing the `reason` string and splitting.

The recipes below cover retry-exhausted and shutdown-drain; v1 enqueue records need manual handling and are skipped.

```bash
# v1 → v2 Process-flavor translation.
# (v1 records whose `process` field does NOT start with "(output ".)
jq -c 'select((.process | startswith("(output ")) | not) | {
    schema_version: 2,
    timestamp,
    reason,
    pipeline,
    kind: "process",
    process: { name: .process },
    event: { source: .event.source, received_at: .event.received_at, ingress: .event.ingress }
}' /var/log/limpid/errored.jsonl.v1.archived-* \
    > /tmp/v1-process.jsonl

# v1 → v2 Output-flavor translation — retry-exhausted + shutdown-drain ONLY.
# (v1 `(output enqueue)` records are skipped; see the manual paragraph below.)
jq -c 'select(.process | startswith("(output ") and .process != "(output enqueue)") | {
    schema_version: 2,
    timestamp,
    reason,
    pipeline,
    kind: "output",
    # Strip the "(output " prefix and the trailing " shutdown)" / ")" suffix
    # to recover the bare output name. (No " enqueue)" handling — those are
    # filtered out above.)
    output: { name: (.process
                     | sub("^\\(output\\s+"; "")
                     | sub("\\s+shutdown\\)$"; "")
                     | sub("\\)$"; "")) },
    event: .event  # v1 already carried egress on output records
}' /var/log/limpid/errored.jsonl.v1.archived-* \
    > /tmp/v1-output.jsonl

# Spot-check the v1 enqueue records that need manual handling.
jq -c 'select(.process == "(output enqueue)")' \
    /var/log/limpid/errored.jsonl.v1.archived-* \
    > /tmp/v1-enqueue-manual.jsonl
wc -l /tmp/v1-enqueue-manual.jsonl
```

The translation is lossy in two ways: (a) the v1 `(output <name>)` and `(output <name> shutdown)` site distinctions are flattened to the same `output.name`, surviving only in `reason`; (b) v1 records that do not match either prefix shape (older daemon variants) are silently dropped. v1 enqueue records are written to a separate file because reconstructing the output name (or names) from the `reason` string is fragile — operators who genuinely need to replay enqueue-failure captures should read the file by hand, identify the intended output(s), and `inject output <name>` the events one cohort at a time. Both translated halves (`/tmp/v1-process.jsonl`, `/tmp/v1-output.jsonl`) can be fed straight through the v2 replay recipes above (`inject input` for the Process file, `inject output` per name for the Output file).

Re-archive the translated halves alongside the original v1 file once replay is complete.

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

The runtime cannot guess what the operator intended at the failure point — `egress` may have been partially rewritten by earlier processes in the chain, the next process expected a workspace key that was never produced, etc. So the DLQ deliberately preserves only the *original* event (key / ingress / source / received_at) for the Process flavor, and the pre-rendered wire payload (with `egress`) for the Output flavor, and lets the operator re-run from the appropriate boundary after the fix.

## Configuring the DLQ

The DLQ file is opt-in via the `control { error_log "..." }` property:

```limpid
control {
    socket    "/var/run/limpid/control.sock"
    error_log "/var/log/limpid/errored.jsonl"
}
```

When `error_log` is unset, the daemon emits a one-line `tracing::error!` summary per failure — site + reason, no payload. Both flavors (Process and Output) behave the same on the unset path: the operator has declared no durable recovery is required, and the tracing side stays payload-free by default. To attach metadata or the full JSONL to the tracing line, opt in via `control { error_log_fallback "meta" | "full" }` — the ladder is documented in [Tracing fallback ladder](#tracing-fallback-ladder-error_log_fallback) below.

For any pipeline that uses `retry { ... }` or a batched output (`http`, `otlp_http`, `otlp_grpc`), `limpid --check` raises a recovery-readiness warning when `error_log` is not configured; see [Recovery readiness check](#recovery-readiness-check---check) below. A separate `--check` warning fires when `error_log_fallback` is set but `error_log` is unset (the fallback is inert in that combination).

### Tracing fallback ladder (`error_log_fallback`)

The `error_log` file is a 0o600 confidentiality boundary the operator has already tightened. The tracing fallback lands in journald / whatever log aggregation is attached, whose access controls are usually weaker, so what appears there is a separate operator decision. `control { error_log_fallback "..." }` picks one of three states:

| Value    | Line body                                                                                                  |
|----------|------------------------------------------------------------------------------------------------------------|
| `"off"`  | (default) one-line failure summary — `kind`, `output` / `pipeline` name, `site`, `reason`. No payload, no metadata. |
| `"meta"` | structured metadata — adds `fallback = "meta"`, `timestamp`, `size` (bytes of the recoverable payload), and `position` (queue kind + numeric offset/seq only, no filesystem path). Still no payload bytes, no full JSONL, no headers, no operator-populated labels. |
| `"full"` | pre-0.7.9 shape — adds `event_record` carrying the full JSONL (ingress / egress bytes included). This exposes the pipeline egress bytes to the tracing subscriber; use only in environments where the journald boundary is trusted. |

**Row-A rule.** When `error_log` is unset the fallback value is ignored — the operator has declared "no durable recovery needed" by omitting `error_log`, and honouring a stray `error_log_fallback "full"` on that path would contradict the declaration. Runtime, startup, and `--check` all enforce this ordering; startup and `--check` emit a warning surfacing the inert combination.

**Disposition invariance.** The ladder shapes the *tracing emission* only. The ack disposition (Delivered / Recovered / Dropped), the disk-queue fail-stop wedge, and the memory-queue fold to Recovered are unchanged by the fallback value — an operator can move up or down the ladder without altering queue cursor semantics.

The path must be in a directory the daemon user can write to, and the **parent directory itself** must not be a symlink — the daemon inspects the final parent component with `symlink_metadata` and refuses a symlink parent because it would let an attacker redirect DLQ writes between the preflight and the runtime write path. Ancestor components may still be symlinks (e.g. `/var/run` → `/run` on modern Linux); ancestor path identity is a **deployment contract**, and pointing `error_log` at a `/run/limpid/...` path avoids the symlink final-parent shape.

Daemon startup validates this by actually opening the DLQ file — `O_CREAT|O_EXCL|O_WRONLY|O_APPEND|O_NOFOLLOW` with `mode(0o600)` if it does not exist yet, `O_WRONLY|O_APPEND|O_NOFOLLOW|O_NONBLOCK` if it does — then, on the create branch, follows up with `fchmod(0o600)` and an `fstat` re-verify on the fresh fd to close the umask window (the mode argument to `open(2)` is masked by the process umask; `fchmod(2)` is not). On the existing branch the same `fstat` runs against the opened fd, closing the TOCTOU gap between the earlier `symlink_metadata` and the `open(2)`. Startup refuses to start if any step fails — `EACCES`, `EPERM`, `EROFS`, an SELinux / AppArmor confinement, a shape mismatch, or a mode mismatch. Daemon startup is the only caller of this preflight; a successful preflight against an absent path leaves an empty 0o600 file at the configured path (the same state the runtime would have produced on the first real failure). `limpid --check` does not run this preflight — configuration validation must not touch the filesystem beyond a read-only stat.

The path itself, if it already exists, must be a regular 0o600 file. Daemon startup and every runtime write refuse a symlink, a FIFO, a socket, a directory, or a device node at the DLQ path with a diagnostic that names the observed shape — an operator typo that pointed `error_log` at `/dev/log`, a stale FIFO from a debugging session, or a socket file left over from another daemon is caught before any records are appended to the wrong endpoint.

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
    create 0600 syslog syslog
    maxsize 1G
}
```

Key choices:

- `create 0600 syslog syslog` — the packaged unit runs limpid as `User=syslog` / `Group=syslog`, and the DLQ writer's runtime contract is *exactly* `0o600` on the on-disk file **owned by the daemon's euid** (the fstat re-verify refuses a foreign-uid inode, see [Trust boundaries](./error-log.md#trust-boundaries) if you have configured a different user). For custom deploys running under a different user, substitute `<daemon-user>:<daemon-group>` in both places. If logrotate creates the fresh post-rotation file at any other mode or owner, the next DLQ write is refused with a `existing file mode 0o... does not match configured mode 0o600` or `existing file is owned by uid ...` diagnostic and `events_errored_unwritable` bumps until the operator aligns the file. Match the rotator's `create` mode + owner with the runtime contract to avoid that failure.
- `copytruncate` — limpid reopens the inode every write, so a normal rotate-and-rename works too, but `copytruncate` is the simplest setup that doesn't require any signal handshake.
- `maxsize 1G` — caps the live file even when `daily` hasn't fired yet. A pipeline producing failures at 10k events/sec with 1 KiB records would fill 1 GiB in ~100 seconds; tune to your environment.
- `rotate 14 + compress` — two weeks of rotated history is usually enough to catch and replay everything between an incident and the operator noticing it.

Operators with stricter retention needs (compliance: hold N days of forensic-quality records) should size accordingly and consider shipping the rotated archives to long-term storage.

## Record format

Each line is a sum-typed JSON record. The current format is `schema_version: 3`; the `kind` discriminator (`"process"` or `"output"`) selects a per-kind block at the top level. Both flavors carry the immutable event key. Process flavor records carry `process: { name }` and a minimal event (ingress only); Output flavor records carry `output: { name }` and the pre-rendered `event.egress`.

### Process flavor

A pipeline-side failure (process body raised, pipeline-skeleton eval failed, explicit `error <expr>`) emits a Process record. Replay re-enters at the input layer — the pipeline is re-run from scratch against the original ingress bytes.

```json
{
  "schema_version": 3,
  "timestamp": "2026-04-27T03:28:39.178046123Z",
  "reason": "unknown identifier: timestamp",
  "pipeline": "journal_forward",
  "kind": "process",
  "process": { "name": "wrap_journal" },
  "event": {
    "key": "0198a3b4-4d7e-7c20-8b11-9f4e6a2d1357",
    "source": {"ip": "10.0.0.1", "port": 514},
    "received_at": 1777260519178046000,
    "ingress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello"
  }
}
```

### Output flavor

A sink-side failure (retry budget exhausted, batched-output shutdown drain, runtime-side enqueue failure) emits an Output record. The serialised `event.egress` is the **pipeline-produced payload** — the value `egress` held at the moment the sink was handed the event, after any process bodies overwrote the initial `ingress` clone. Replay hands the event back to the named output's `consume()` path: the pipeline is **bypassed**, but the sink's transport-level rendering (batched encode, HTTP body framing, OTLP packing, …) still re-runs.

```json
{
  "schema_version": 3,
  "timestamp": "2026-04-27T03:31:02.998742000Z",
  "reason": "output write failed after 5 attempts: connection refused",
  "pipeline": "",
  "kind": "output",
  "output": { "name": "mysink" },
  "event": {
    "key": "0198a3b4-4d7e-7c20-8b11-9f4e6a2d1357",
    "source": {"ip": "10.0.0.1", "port": 514},
    "received_at": 1777260519178046000,
    "ingress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello",
    "egress": "<134>1 2026-04-27T03:28:39Z host app 1234 - - hello\n"
  }
}
```

### Common fields

| Field | Meaning |
|-------|---------|
| `schema_version` | Integer `3`. Identifies the event-identity shape; v2 records omit `event.key` (see [Schema migration v2 → v3](#schema-migration-v2--v3) below). |
| `timestamp` | RFC3339 with nanosecond precision; wall-clock at which the failure was raised. |
| `reason` | Stringified failure reason. Stable enough for `grep` / classification but not a stable API. The runbook below maps reason patterns back to producer sites. |
| `pipeline` | Pipeline name (`def pipeline <name>`). Populated for every Process record; for Output records it carries the originating pipeline only when the failure happened *at the pipeline → output boundary* (= enqueue failure). Retry-exhausted and shutdown-drain Output records have an empty `pipeline` field because the event had already left its source pipeline by then. |
| `kind` | Discriminator: `"process"` or `"output"`. Selects which per-kind block is present and which `event.*` fields are populated. |
| `event.key` | Canonical hyphenated lowercase UUIDv7. Immutable across fan-out, capture, and replay. |
| `event.source` | Originating peer as `{ip, port}` object. Same shape as `tap --json` and as the DSL `source` ident. |
| `event.received_at` | i64 unix nanoseconds (matches OTLP `time_unix_nano`). Same shape as `tap --json`. |
| `event.ingress` | Original wire bytes. UTF-8-clean payloads serialise as a JSON string; non-UTF-8 payloads use the `$bytes_b64` marker the rest of the JSON layer already uses for `tap --json`. |

### Process-flavor extras

| Field | Meaning |
|-------|---------|
| `process` | `{ "name": "<site>" }` block. `name` is the failing `def process` name, `(inline)` for an inline `process { ... }` block, `(pipeline)` for a pipeline-statement `error <expr>`, or `(pipeline body)` for a pipeline-skeleton expression failure (`if` condition, `switch` scrutinee, `error <expr>` argument, or a body expression evaluated inside the pipeline). |

`event` carries only `{ key, source, received_at, ingress }` — no `egress`, no `workspace`. Replay re-runs the pipeline from scratch on `ingress` while retaining the same event identity.

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
3. **`(pipeline body)`** — an `if` condition / `switch` discriminant / explicit `error <expr>` argument failed to evaluate before reaching a process body.
4. **`(pipeline)`** — `error <expr>` at the pipeline (statement) level raised, including the dispatcher pattern where a snippet routes on an unrecognised subtype.

Output flavor (3 sites — replay via `inject output`):

5. **`<output_name>`** — the output exhausted its `retry { ... }` budget against the destination. A batched output's per-event render failure inside `flush()` is also routed here with `reason = "render failed during batch flush: ..."`. `pipeline` is empty.
6. **`<output_name> shutdown`** — a batched output (`http`, `otlp_http`, `otlp_grpc`) was **gracefully shut down** (`SIGTERM`, `SIGHUP` reload, `systemctl stop`, or an explicit `shutdown()` API call) with events still buffered and the bounded final drain failed. The drain runs one flush attempt per payload bounded by `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` (3 s) plus a per-actor in-flight cancel; transport timeout, in-flight cancel, or retry exhaustion all route here. The `shutdown()` impl walks the remaining `(Event, QueueAckHandle)` buffer entries (one record per parked event) through this writer. `pipeline` is empty. The per-event `key`, `source`, `received_at`, `ingress`, and `egress` come from the original `Event` that was parked in the buffer at `consume()` time; nothing is synthesised. (Earlier 0.7.7 drafts of this flavor carried synthetic shutdown-time metadata; the 0.7.8 ack-lifecycle work parks the source `Event` alongside the ack handle so each shutdown-drain record now reflects the real per-event provenance.) **`SIGKILL` (`kill -9`) cannot reach this path** — actor tasks are aborted and the stack-local buffer is lost without an error_log write. Production deployments must not send `SIGKILL` directly to the daemon; keep systemd's `KillSignal=SIGTERM` default.
7. **`<output_name> enqueue`** — `runtime.rs` could not hand an event to the named output's queue (queue closed, disk-queue write error, unknown output). `pipeline` is the name of the originating pipeline — the only Output-flavor site that keeps it populated. Per-failed-output split: a single pipeline-eval result with N failed-output enqueues produces N records (one per failing output).

The `reason` field distinguishes sites 5 / 6 / 7 within a single output name: retry exhaustion uses `"output write failed after N attempts: ..."`, shutdown drain uses `"shutdown flush failed: ..."`, enqueue failure uses `"output enqueue failed (queue closed, disk write error, or unknown output)"`. A batched output's per-event render failure inside `flush()` uses `"render failed during batch flush: ..."`. The runbook's [root-cause heuristics table](#step-3--root-cause-heuristics-by-site) lists the full patterns.

### Address-free Output records

The Output record carries `output: { name }` and nothing more. No peer address, endpoint URL, partition, topic, path, target, routing key, or workspace fragment leaks into the recovery record.

Why: replay calls `limpidctl inject output <name>`, which hands the event back to the sink. The sink re-routes via the same `consume()` path it uses for live traffic — round-robin peer selection, retry budget, batching, headers, all of it. There is no "send this exact record to this exact peer" mode; the sink owns routing. Carrying address details on the record would create two failure modes: (a) replay vs. live diverging when the operator updates the sink config between failure and replay, and (b) sensitive routing metadata (peer hostnames, internal endpoint URLs) leaking into the DLQ file.

The trade-off: an operator who needs to know *which peer failed* for an `<output_name>` retry-exhaustion record reads the daemon log (`journalctl -u limpid` around the timestamp), not the DLQ record.

### Schema stability

`schema_version: 3` is the operator-visible discriminator. Pre-1.0, the schema may add fields to existing kinds, or add new kinds, both of which bump `schema_version`. Field renames within an existing kind also bump it. After 1.0 the format will be locked under semantic versioning.

`event.source` changed shape from a flat `"ip:port"` string to a `{ip, port}` object in v0.5.6 (independent of `schema_version`, but worth knowing if you're reading captures from that era).

### Recovery readiness check (`--check`)

Since 0.7.8, `limpid --check` emits a recovery-readiness warning when any output declares `retry` or is a batched OTLP/HTTP output and `control { error_log }` is unset. Without `error_log`, every Output-flavor recovery path (retry exhausted, shutdown drain, enqueue failure) emits only a one-line `tracing::error!` summary — the payload is not persisted anywhere unless `error_log` is set and `error_log_fallback` is opted up the [ladder](#tracing-fallback-ladder-error_log_fallback). The warning catches the missing configuration before the first failure.

A separate `--check` warning fires when `control { error_log_fallback "..." }` is set while `control { error_log "..." }` is unset — the fallback is a confidentiality opt-in for the tracing side of a *configured* DLQ path, and has no effect on its own. Either add `error_log` to activate the fallback, or drop `error_log_fallback` to silence the warning.

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
| `output` | `<output_name>` | `shutdown flush failed: …` | **graceful** shutdown (`SIGTERM` / `SIGHUP` / `systemctl stop`) while a batched output (`otlp_*`, `http`) still had buffered events and the bounded 3 s drain attempt did not succeed. Note: `SIGKILL` (`kill -9`) does **not** reach this path — it leaves no DLQ record at all. | `journalctl -u limpid` around the shutdown window |
| `output` | `<output_name>` | `output enqueue failed (queue closed, disk write error, or unknown output)` | output queue full, output task stopped, disk-backed queue write error, unknown output name in the pipeline body | output `events_received` vs `events_written`, disk space, queue config |
| `process` | `(pipeline body)` | varies | typo in an `if` condition / `switch` discriminant, undefined workspace key referenced from an `error <expr>` slot | the `reason` string + the pipeline DSL around the failing expression |
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

**Shutdown-drain caveat.** Output-flavor records with `reason` starting with `shutdown flush failed: ...` are *per-event* records, not per-batch — the batched output's shutdown helper walks every still-buffered `(Event, QueueAckHandle)` entry and writes one record per parked event, so `event.key`, `event.source`, `event.received_at`, `event.ingress`, and `event.egress` all reflect the original per-event provenance (no synthetic shutdown-time metadata). `event.egress` carries the per-event pre-rendered payload (= the bytes the output had built for the wire on each event, before the unsent batch wrapper was applied), so `inject output <name>` is the correct replay path — the sink takes the per-event pre-rendered bytes and re-routes via its `consume()` path, applying the current batch wrapper / headers / compression as if the event had just been enqueued. Do **not** route shutdown-drain records through `inject input <name>`: doing so feeds the per-event pre-rendered payload back into the pipeline as raw `ingress`, which is almost never what you want.

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

**Alerting on DLQ growth.** Two counters together cover both flavors: pipeline-side failures (Process records + the enqueue-failure subset of Output records) bump `events_errored` on the pipeline, while sink-side failures (retry exhausted, shutdown drain, batched-output per-event render failures) bump `events_failed` on the originating output. `events_errored_unwritable` is the secondary alarm that the DLQ writer itself is failing — split across two labels, one for each side — and `events_wedged` (output-side) fires when a disk-queue Dropped disposition has stopped the consumer from accepting new events. See [Metrics](./metrics.md) for the full list. A Prometheus rule:

```promql
# Sustained pipeline-side failure (Process flavor + output enqueue):
# more than 10 events/sec into the DLQ for 5 minutes.
rate(limpid_pipeline_events_errored_total[5m]) > 10

# Sustained sink-side failure (Output flavor retry / shutdown / render):
# any single output averaging > 10 failures/sec for 5 minutes.
rate(limpid_output_events_failed_total[5m]) > 10

# Alarm: pipeline-side DLQ writer can't write — replay is partial.
increase(limpid_pipeline_events_errored_unwritable_total[5m]) > 0

# Alarm: sink-side DLQ writer can't write on this output — the
# same recovery gap on the output-flavor path.
increase(limpid_output_events_errored_unwritable_total[5m]) > 0

# Alarm: disk-queue fail-stop wedge fired on this output — the
# consumer has stopped accepting new events and will replay from
# the wedged cursor on next daemon start. Page on this.
increase(limpid_output_events_wedged_total[5m]) > 0
```

The Prometheus metric names above are the authoritative form emitted by `limpid-prometheus`; the equivalents on the JSON stats endpoint are `events_errored`, `events_failed` (per output), `events_errored_unwritable` (both pipeline and output sides), and `events_wedged` (output-side only).

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

[error_log]  {"schema_version":3,"timestamp":"...","reason":"...","pipeline":"journal_forward","kind":"process","process":{"name":"wrap_journal"},"event":{"key":"0198a3b4-4d7e-7c20-8b11-9f4e6a2d1357","source":{"ip":"127.0.0.1","port":0},"received_at":...,"ingress":"<134>sample event"}}
```

This is useful for confirming the JSONL shape, the `kind` / per-kind name discriminator, and that the original ingress is captured correctly — all without booting the daemon or touching any file. The Output-flavor shape can be observed by triggering a sink retry exhaustion against an unroutable peer; `--test-pipeline` does not directly emit Output records (it stops at pipeline-side disposition).

## When the DLQ write itself fails

`events_errored_unwritable` counts the cases where the daemon raised an error trying to write a DLQ record to the configured `error_log` file (disk full, permissions, NFS hiccup, rotation race). The counter is split across two labels that share the metric name; both must be watched:

- **Pipeline-side** (`limpid_pipeline_events_errored_unwritable_total{pipeline=...}`) — the Process-flavor path and the output-enqueue subset of Output-flavor records failed to land in `error_log`. Both routed through `runtime::write_errored_to_dlq`.
- **Output-side** (`limpid_output_events_errored_unwritable_total{output=...}`) — a sink-side Output-flavor DLQ write (retry exhaustion, shutdown drain, batched render failure, partial-success reject) failed to land in `error_log`. Routed through `modules::route_event_to_dlq`, which bumps the per-output counter and returns `Dropped`; on a disk queue that triggers the [fail-stop wedge](../outputs/README.md#disposition-contract) — the wedge holds the cursor for a replay on next daemon start rather than silently advancing past a DLQ-failed event.

In both cases the daemon emits a `tracing::error!` line whose body is shaped by the operator's `error_log_fallback` [ladder](#tracing-fallback-ladder-error_log_fallback) — payload-free summary by default, structured metadata on `"meta"`, or the full JSONL on `"full"`. This is alarm-level regardless of the ladder state: a non-zero counter on either label means the replay path may be incomplete, and the next failure may not have a corresponding line in the file.

Investigate immediately:

- Is the parent directory writable by the daemon user?
- Is the disk full? (`df`)
- Did rotation leave an incompatible node or mode/owner at the path? The DLQ writer opens a fresh fd per write (`create_new` + `fstat` verify), so rotation does not need a `SIGHUP` reload — but if the rotator recreated the file at a different mode (`0o644` instead of `0o600`) or a different owner (a foreign uid, or a group other than the daemon's), the fstat check refuses to append. Use `copytruncate` (which preserves the inode and its mode + owner), or `create 0600 syslog syslog` matching the packaged unit's runtime contract exactly (substitute `<daemon-user>:<daemon-group>` for custom deploys). `nocreate` also works but leaves the runtime to materialise the file on the next failure, which pushes the deploy check onto the failure path.
- Is the file path on a network filesystem with intermittent connectivity?

Once the underlying issue is fixed, the next errored event lands in the file again and the counter stops increasing; existing records are unaffected.

## Disposition contract and fail-stop wedge on disk queues

Every event handed to an output resolves to one of three `AckDisposition` values:

- **`Delivered`** — the output confirmed the send. Cursor advances on both memory and disk queues; `events_written` ticks.
- **`Recovered`** — the send failed and either the failure record was durably written to `error_log`, or `error_log` is unset so the operator has declared no durable recovery is required. The tracing-side fallback line runs per the `error_log_fallback` [ladder](#tracing-fallback-ladder-error_log_fallback) — payload-free summary by default, structured metadata on `"meta"`, full JSONL on `"full"` — and is best-effort, not load-bearing. Cursor advances on both memory and disk queues; `events_failed` ticks.
- **`Dropped`** — the output could not confirm the send *and* could not durably record the failure. Cursor **holds** on a disk queue and **advances** on a memory queue; `events_failed` ticks.

Three paths reach `Dropped`:

- A bug in an output's `consume` implementation that returns before signalling the handle (the handle's `Drop` then fires `Dropped`, guarded by a `debug_assert!` in test builds).
- A panic inside `consume` while it holds the handle.
- An **intentional** `resolve_dropped()` call from a sink whose DLQ write failed — the failure has no durable trace, so cursor advancement would be a silent loss.

On a **disk queue**, observing `Dropped` triggers the **fail-stop wedge**: the queue consumer stops accepting new events, holds the on-disk cursor at the offending position, emits a `tracing::error!` line, and bumps `events_wedged`. On the next daemon start the disk queue replays from the wedged position, giving the operator a chance to fix the underlying bug / DLQ health / etc. before the same event reaches a healthy output.

**Scope of the wedge contract**: the fail-stop wedge covers both **unbatched sinks** (`file`, `stdout`, `unix_socket`, `syslog_tcp`, `syslog_udp`, `kafka`) and **batched sinks** (`http`, `otlp_http`, `otlp_grpc`). Unbatched sinks resolve each event's ack synchronously inside their `consume` call, so `in-flight == 0` at wedge time and the consumer drains cleanly on its own. Batched sinks accept events into their own internal `(Event, QueueAckHandle)` buffer before flushing; on wedge exit the queue consumer takes a separate `shutdown_wedged()` path that signals the flusher actor to exit, then routes every still-parked buffer entry through the shutdown-batch ambiguous DLQ path with the disposition forced to `Dropped` regardless of the DLQ write result. On a disk queue the ambiguous disposition keeps the wedged cursor at the batch's position for next-start replay; on a memory queue it folds to `Recovered` inside the disposition helper so the ack drain does not hang on messages that will never arrive. No further send is attempted on the wedged path — replaying a parked handle through the still-buggy sink would risk the same Dropped outcome and prolong the wedge.

On a **memory queue** the wedge does not fire — memory queues cannot replay on restart, so holding the cursor would only cause loss without a recovery path. The consumer bumps `events_failed` and moves on; when a DLQ write failure was the cause the event is actually lost, and `events_errored_unwritable` is the operator alarm signal for that loss rather than a durable trace of the event itself.

For operators, the practical implications:

- **`events_wedged` is a fail-stop alarm.** Non-zero means a disk-queue consumer has stopped accepting new events. Investigate the output: check for panics (`panicked at ...` lines with `RUST_BACKTRACE=1`), DLQ health (`events_errored_unwritable`), and disk / permission errors on the DLQ file. **Restart the daemon after fixing the underlying issue** so the disk queue replays from the wedge point.
- **`events_wedged` is not self-healing.** Poison-message bugs (a specific event that reliably panics one output) will replay the same event on every start and re-wedge. Fix or delete the underlying record before the restart, or operator intervention becomes a loop.
- **`events_errored_unwritable` on the output pairs with the pipeline-side counter of the same name.** Both surface DLQ-write failure; the output-side counter fires on the sink-side path (`route_event_to_dlq` in `crates/limpid/src/modules/mod.rs`), the pipeline-side counter fires on the pipeline runtime path (`write_errored_to_dlq` in `crates/limpid/src/runtime.rs`). Alarm on both.
- **`events_failed` remains the aggregate failure counter.** A step increase without a matching increase in `events_errored`, DLQ file size, or `events_wedged` means events are failing at the send stage but recovering successfully — expected under transient network trouble, worth watching under a healthy DLQ.

Shutdown fallthrough — the runtime SIGTERM budget elapsing while a send is still in flight — falls into the same `Dropped` path: the task is aborted, the parked handle drops, and on a disk queue the position holds for replay on next start. Network unbatched sinks (`kafka`, `syslog_tcp`, `syslog_udp`, `unix_socket`) narrow the shutdown race to the pre-send phase (connect / handshake / rotation); the send phase itself runs to completion or times out via its per-write timeout (`PEER_WRITE_TIMEOUT` / `message.timeout.ms`), and only that timeout — or a runtime task abort — can cut a send short. The `file` output remains deliberately shutdown-unaware end-to-end for the same reason: partial-append duplication on retry is worse than a Dropped-then-replayed event.

## Graceful shutdown guarantees and known limitations

The runtime's graceful-shutdown sequence (`SIGTERM`, `SIGHUP` reload, `systemctl stop`, or an explicit `shutdown()` API call) is bounded by `runtime::Daemon::SHUTDOWN_TIMEOUT` (10 s) and pipes every in-daemon event through a defined disposition — there is no silent-loss window inside the runtime's own channels.

### What the shutdown drain guarantees

- **Output-queue backend-aware drain.** Memory-backed output queues are closed and drained via `recv().await`-until-`None`, the tokio-documented pattern for consuming every value already sent or being sent by a sender still holding a channel permit. A pipeline worker whose `send()` had reserved a permit but not yet written its value is not silently lost — the send still becomes visible before the drain terminates. Disk-backed output queues do not drain unread WAL entries into the 10 s shutdown window at all; the WAL cursor holds and the unread backlog replays on next start.
- **Pipeline-worker channel drain.** The pipeline worker's input channel (`event_rx`) uses the same close + recv-until-None pattern, so an input task whose `send()` was mid-flight when shutdown fired lands on the worker. Late input sends that lose the race surface via `journalctl` at `info!` level rather than a silent stop.
- **Bounded batched-sink ownership.** Batched outputs (`http`, `otlp_http`, `otlp_grpc`) hold at most `batch_size × 2` events in memory via a per-sink permit pool. Queue backpressure survives a stalled downstream: a durable disk backlog cannot dissolve into shutdown-window RAM through an unbounded parked buffer.
- **No fabricated at-least-once on ambiguous wire state.** Any sink whose drain-time failure may have partially reached the peer — stream-oriented singletons (`unix_socket`, `syslog_tcp`), the kafka broker after `producer.send(...)`, and batched sinks (`http`, `otlp_http`, `otlp_grpc`) whose in-flight `policy.send(...)` was cancelled by shutdown or hit the 3 s attempt timeout — routes its failure to `Dropped`, not `Recovered`. On a disk queue the fail-stop wedge holds the cursor for next-start reconciliation; on a memory queue the disposition folds to `Recovered` inside the disposition helper (no replay path exists). A DLQ record is written in every case for the operator's audit trail — the disposition change prevents the pattern where a partial-wire success is compounded by a DLQ-driven replay and the downstream receives the same batch twice.

### Known limitations

Two boundaries the shutdown drain does not cover. Both are protocol-level and both are called out here so operators know where the guarantees end.

1. **Wire buffer between kernel and daemon.** Bytes that reached the kernel socket buffer for a limpid input (`syslog_udp`, `syslog_tcp`, `unix_socket`) but were not yet read by the daemon at the moment `SIGTERM` fired are outside the shutdown drain's reach. UDP has no protocol-level ack and cannot recover this class in principle; stream syslog cannot recover it without a per-connection application ack from the upstream, which is not part of the protocol. Operators who need the wire buffer to survive limpid restarts must run an ack-aware collector upstream (OTLP with retries, kafka with acks, or a queueing forwarder) rather than rely on plain syslog.
2. **Delivery-completeness upgrade for the race window.** The current shutdown ordering routes any event that lost the shutdown-vs-send race to the DLQ. For singleton sinks that failed before crossing the wire boundary the disposition is `Recovered` — the operator can run `limpidctl inject output` to replay. For ambiguous-wire failures (both singleton and batched, described above) the disposition is `Dropped` on a disk queue; next-start replay reconciles the wedged position against the DLQ record. Either way the event is not silently lost, but the outcome is a downgrade from full "delivery" — reconciliation cost falls on the operator. A full phase-split of the shutdown ordering — inputs stop, pipeline workers drain, output senders close, output consumers drain — would upgrade the pre-boundary class to `Delivered`. It is queued for a follow-up release.

Both limitations are surfaced deliberately. `events_failed` on the output ticks for every event that took the DLQ path; the DLQ record captures the payload. Neither is treated as an excuse for further silent gaps inside the runtime.

**`SIGKILL` (`kill -9`) still bypasses the entire graceful-shutdown path.** Actor tasks are aborted with their handles unresolved, and batched outputs lose whatever was mid-flush. Production deployments must keep systemd's `KillSignal=SIGTERM` default and give the daemon its 10 s budget.

## Schema migration v2 → v3

Version 3 adds `event.key`, the immutable UUIDv7 shared with `tap --json` and
the queue persistence format. New records retain the same key when replayed,
so a failure can be correlated with observations made before and after the
DLQ boundary.

Version 2 records remain replayable: their event object has no key, so limpid
assigns a UUIDv7 when it first reads that event object. No rewrite is required
unless external tooling validates `schema_version`; such tooling should accept both
versions during migration and treat a missing v2 key as unavailable rather
than synthesising one independently.

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

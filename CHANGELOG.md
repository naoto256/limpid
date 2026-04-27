# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and
runtime shape converge. After 1.0, changes will follow semver strictly.

## [0.5.4] - 2026-04-27
> User-defined pure functions (`def function`) with let-form bodies

### Added — `def function` for pure expression functions

User-defined functions are now a top-level definition kind, alongside
`def input` / `def output` / `def process` / `def pipeline`. The body
is zero or more `let` bindings followed by a required trailing
expression that becomes the return value. Designed for the small
mapping / lookup helpers that vendor parsers reuse — protocol number
→ name, severity string → OCSF `severity_id`, action string →
activity_id — and for the small chains of intermediate values that
make those mappings readable.

```
def function normalize_proto(num) {
    switch num {
        6  { "tcp" }
        17 { "udp" }
        1  { "icmp" }
        default { null }
    }
}

def function severity_id_from_label(s) {
    let lowered = lower(trim(s))
    switch lowered {
        "critical" { 5 }
        "high"     { 4 }
        "medium"   { 3 }
        "low"      { 2 }
        "info"     { 1 }
        default    { 1 }
    }
}

def process parse_fortigate_cef_traffic {
    workspace.limpid = {
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        severity_id: severity_id_from_label(workspace.cef.severity),
        ...
    }
}
```

User-defined functions register into the same `FunctionRegistry` as
built-in primitives — call sites dispatch through the standard
`(namespace, name)` lookup, the analyzer arity-checks them the same
way, and they compose anywhere an expression goes (HashLit values,
function arguments, binary operands, output templates, pipeline-level
`if` conditions). Function names must be bare identifiers; the dot
namespace is reserved for schema-bound built-ins.

`let` is the assignment form for local-scope variables in the body —
each `let x = …` line binds (or reassigns) `x` in the same scope.
Re-binding the same name simply overwrites the prior value; there is
no separate declaration step, no `let mut`, and no `x = …`
re-assignment syntax. Each let RHS sees parameters and earlier lets;
the trailing expression sees everything.

To keep functions pure, the analyzer rejects function bodies that:

- read from the Event (`ingress`, `egress`, `source`, `received_at`,
  `error`, any `workspace.*` path) — anywhere in the body, including
  inside a `let` RHS;
- reference a free variable that's neither a parameter nor an
  Event-bound name (a `config.foo` or bare `result` typo surfaces at
  `--check` time instead of failing at runtime);
- call into a user-defined `def process` (process bodies have side
  effects functions can't tolerate); or
- participate in a function-to-function call cycle (direct
  self-recursion or mutual recursion through a chain). If recursion
  is genuinely needed, use `def process` instead.

All four are hard errors at `--check` time — the config fails to load
and the daemon won't start until they're fixed.

Side effects (`workspace.x = …`, `egress = …`, `drop` / `finish` /
`output` routing, statement-form `if` / `switch` / `foreach`
/ `try-catch`) are rejected at the parser level — function body
grammar accepts only `let` bindings and a trailing expression, so
those statement forms simply aren't in the grammar.

A new expression-form `switch` lands at the same time. Each arm
body is one expression; the matching arm's value is the value of
the whole `switch`. Distinct from the statement-form `switch` in
process / pipeline bodies (which routes events / mutates
workspace). Use the expression form inside `def function` bodies,
inside `let` RHS, or anywhere a value is expected.

## [0.5.3] - 2026-04-27
> limpidctl stats surfaces errored counters

### Fixed — `limpidctl stats` shows `events_errored` / `events_errored_unwritable`

The 0.5.2 pipeline metrics gained `events_errored` and
`events_errored_unwritable` but the human-readable `limpidctl stats`
renderer wasn't updated — the JSON form (`limpidctl stats --json`,
control socket, Prometheus) carried both, the default text form
silently dropped them. Operators saw zero on `stats` while the
real number was hiding in the JSON.

The columns now render when they're non-zero:

```
Pipelines:
  ama_forward         89 received  35 finished  23 dropped   0 discarded  31 errored
  splunk_archive      62 received  38 finished  24 dropped   0 discarded
```

Steady-state pipelines (no errors) keep the compact row — a column
of zeros across every pipeline in the common case is just noise. A
non-zero `events_errored_unwritable` adds a second column on top of
`errored`.

## [0.5.2] - 2026-04-27
> Dead-letter queue for process errors

### Changed — process runtime errors route to a dead-letter queue (revising 0.5.1)

0.5.1 changed the pipeline so that a `process` runtime error caused
the event to be **discarded** with a counter increment. That was
appropriate for surfacing the silent corruption that 0.5.0's
"warn-and-continue" produced, but for a log pipeline default-discard
is itself a strong failure mode — security telemetry should not lose
events to a config bug at the receiving SIEM.

The 0.5.2 default sets the failed event aside in a **dead-letter
queue** (DLQ) so the operator can audit, fix the offending config,
and replay:

- New `control { error_log "/var/log/limpid/errored.jsonl" }`
  property opts in to a JSONL file. Each errored event becomes one
  line:

  ```json
  {
    "timestamp": "...",
    "reason": "...",
    "process": "wrap_journal",
    "pipeline": "journal_forward",
    "event": {"source": "...", "received_at": ..., "ingress": "..."}
  }
  ```

  The `event` sub-object is exactly what `limpidctl inject --json`
  needs to reconstruct a fresh Event, so replay is:

  ```bash
  jq -c '.event' /var/log/limpid/errored.jsonl \
      | limpidctl inject input <name> --json
  ```

- When `error_log` is **unset**, the same record is emitted as a
  structured `tracing::error!` line so the data is never silently
  lost — it just lives in journald / stderr instead of a dedicated
  file. Operators using the daemon under systemd can still recover
  via `journalctl -u limpid -o json | jq …`.

- New `events_errored_unwritable` counter (and
  `limpid_pipeline_events_errored_unwritable_total` Prometheus
  metric): subset of `events_errored` for which the DLQ write itself
  failed (disk full, permissions, rotation race). The runtime falls
  back to the tracing channel; alarm on this counter — non-zero
  means the replay path may be incomplete.

- The pipeline-runtime trace now reads `event → error_log` instead
  of `event discarded`. `--test-pipeline` prints the would-be JSONL
  record after the trace so operators can rehearse the replay
  recipe without booting the daemon.

The downstream behaviour is unchanged from 0.5.1: errored events
still don't reach any output, so there is no shape regression in the
production stream. What changes is that the events are now
**recoverable**.

### Fixed — DLQ writer hardening (audit follow-up)

- **Concurrent line interleave**: multiple pipeline workers calling
  `ErrorLogWriter::write` no longer race. POSIX `O_APPEND` atomicity
  only covers writes ≤ `PIPE_BUF` (Linux: 4 KiB), and DLQ records
  carrying base64-encoded binary `ingress` easily exceed that. An
  in-process `tokio::sync::Mutex` serialises the open + write
  sequence so each JSONL line is written whole.
- **Startup path validation**: `error_log` parent directory is
  stat()'d at daemon start; a typo'd / missing path is rejected
  before any event reaches the failure path. Previously the typo
  surfaced as `events_errored_unwritable` ticks at first failure.
- **Rotation guidance**: `operations/error-log.md` now ships a
  recommended `logrotate` configuration (`copytruncate` + `maxsize
  1G`) so the DLQ has a documented disk-fill ceiling. In-process
  rotation is deferred to v0.6.0; operator-side `logrotate` covers
  the realistic blast radius for v0.5.2.

## [0.5.1] - 2026-04-27
> Analyzer strictness + pipeline error handling

### Breaking — process runtime errors discard the event

When a `process` statement raises a runtime error (unknown identifier,
type mismatch, regex compile failure, …) the pipeline now **discards**
the event and increments a new `events_errored` counter, instead of
emitting a `WARN` and forwarding the event with its original `ingress`
unchanged.

The previous fallback ("warn-and-pass-through") combined poorly with
the analyzer gap that let unresolved bare identifiers slip past
`--check`: a config that referenced a renamed Event field
(e.g. pre-0.5 bare `timestamp`) loaded fine, then failed every event
at runtime — but the original ingress was forwarded downstream, so
the operator's wrap / enrichment process was silently bypassed.

Operators now see the failure in `events_errored` (and via the new
`limpid_pipeline_events_errored_total` Prometheus metric / per-trace
`error: ... (event discarded)` line), rather than discovering it
hours later at the receiving SIEM. Configs that intend partial
processing should use `try { ... } catch { ... }` to express that
intent explicitly.

The same routing applies to inline `process { ... }` bodies, which
previously bubbled the error up to the runtime as a Result and lost
the event without incrementing any pipeline counter.

### Added — analyzer flags unknown bare identifiers

`--check` now warns when a `process` body or expression references an
identifier that doesn't resolve to a reserved event ident
(`ingress`, `egress`, `source`, `received_at`, `error`), a `let`
binding, or a `workspace.*` path. The warning carries `DiagKind::UnknownIdent`
so `--ultra-strict` promotes it to an error in CI.

A bare `timestamp` reference — the most common 0.4→0.5 migration miss
— gets a targeted help line pointing at both alternatives:
`received_at` for the wall-clock event time, `timestamp()` for the
current instant. Other unknown idents fall back to the levenshtein
suggestion engine ("did you mean `ingress`?").

The `type` property of an `output` block (its bare-ident value is a
module-name reference resolved at config-load time, not a runtime
expression) is exempt — flagging `stdout`, `tcp`, etc. as unknown
would be a false positive.

## [0.5.0] - 2026-04-26
> OTLP transport + DSL surface freeze

### Changed — design principles restructured (still five)

The five design principles have been reorganised so each one carries
its own architectural weight, rather than mixing principles with
operating rules. The renumbered set:

1. **Zero hidden behavior** *(unchanged)*
2. **I/O is dumb transport** *(unchanged)*
3. **Only `egress` crosses hop boundaries** *(was Principle 4)*
4. **Atomic events through the pipeline** *(new)* — formalises the
   invariant that the pipeline never operates on bundles or fans out:
   inputs split wire-level batches into atomic Events, process snippets
   are 1-in-1-out (or 0 via `drop` / `finish`), outputs rebundle at the
   emit boundary. The OTLP envelope split, the `syslog_*` line split,
   the `batch_level` mode on the OTLP output — all are this one
   principle in different transports.
5. **Safety and operational transparency** *(new)* — formalises the
   software-construction stance that surfaces in every limpid feature:
   `--check` static analysis, `tap`/`inject`/`--test-pipeline` for
   verify-and-replay, `SIGHUP` atomic reload with rollback, retry +
   secondary + disk-WAL on outputs, `Drop` hooks for shutdown
   visibility. Principle 1 covers config-time transparency; Principle
   5 covers runtime transparency and recoverability.

What used to be Principles 3 (domain knowledge in DSL) and 5 (schema
identity by namespace) are now under a new *Operating rules* section
in the same document — they are concrete consequences of Principles 1
and 2 rather than independent architectural commitments. Anything
that previously cited *"per Principle 3"* should now cite *"per the
Domain knowledge in DSL operating rule"* or, more usefully, the
Principle the rule is derived from.

This is a docs-only change in v0.5.0; no code is affected. Pre-1.0,
this kind of clarification is expected.

### Added — OpenTelemetry Protocol (OTLP) support

OTLP becomes a first-class transport across both ingest and emit, with
all three OTLP wire formats supported:

- **Inputs**: [`otlp_http`](docs/src/inputs/otlp-http.md) (`POST /v1/logs`,
  `application/x-protobuf` and `application/json`) and
  [`otlp_grpc`](docs/src/inputs/otlp-grpc.md) (`opentelemetry.proto.collector.logs.v1.LogsService.Export`).
  Each LogRecord becomes one Event with `ingress` set to a singleton
  ResourceLogs (1 Resource + 1 Scope + 1 LogRecord), preserving full
  upstream context per Principle 2.
- **Output**: [`otlp`](docs/src/outputs/otlp.md) with
  `protocol "http_json" | "http_protobuf" | "grpc"`, `batch_size`,
  `batch_timeout`, `headers {}`, and TLS via system roots / custom CA.
- **Primitives** (in the new `otlp.*` namespace):
  `otlp.encode_resourcelog_protobuf` /
  `otlp.decode_resourcelog_protobuf` /
  `otlp.encode_resourcelog_json` /
  `otlp.decode_resourcelog_json`. HashLit shape mirrors the proto3
  tree with snake_case keys; JSON form applies the canonical OTLP/JSON
  conventions (camelCase, u64-as-string, bytes-as-hex).

The hop contract is "egress = singleton ResourceLogs proto bytes":
the process layer owns semantic conversion (severity mapping,
OCSF→OTLP shape) via DSL snippets; Rust ships only the mechanical
wire encode / decode (Principle 3).

### Added — OTLP throughput controls

Four orthogonal defense / throughput layers on the OTLP/HTTP input,
each opt-in (default unlimited) so existing configs are unaffected:

- **`body_limit`** *(default `16MB`)* — bytes per request. Larger
  bodies are rejected with HTTP 413 *Payload Too Large* before any
  decode work runs. axum's `DefaultBodyLimit` shows up in the layer
  chain, replacing axum's own 2 MiB default which is too small for
  collector-to-collector batches.
- **`max_concurrent_requests`** — in-flight request cap (semaphore).
  Worst-case decode memory becomes
  `max_concurrent_requests × body_limit`, turning the open-ended
  decode-amplification path into a known quantity. Excess requests
  fail-fast with HTTP 503 *Service Unavailable* (OTLP senders retry,
  so backpressuring the socket would amplify overload).
- **`request_rate_limit`** — sustained req/sec (token bucket, reuses
  the existing `RateLimiter`). Smooths burst above the configured
  rate; pairs with the concurrency cap because a token bucket allows
  full burst-equal-to-rate at idle.
- **`rate_limit`** — sustained events/sec, per-emitted-LogRecord. Same
  implementation as `syslog_*`, applied after request decode and
  split, so it caps pipeline-send rate independent of how the events
  arrived.

`otlp_grpc` gets `rate_limit` on the same axis. Per-RPC throttling
on the gRPC side relies on tonic's HTTP/2 stream limits and the
existing `rate_limit` after split — no new property.

### Added — `otlp_grpc` server-side TLS / mTLS

Optional `tls { cert key ca }` block on the input. With `cert` + `key`
the server presents a certificate; adding `ca` switches into mutual
TLS mode where every client must present a certificate signed by that
CA root. Mirrors the same block shape as `syslog_tls` (now parsed via
a shared `TlsConfig::from_properties_block` helper). PEM files are
loaded via `spawn_blocking` so a slow disk does not stall the tokio
reactor at startup.

For the output, gRPC client-side TLS already shipped in the initial
OTLP push; this release closes the symmetric server-side gap.

### Added — `otlp` output `batch_level` merging

Three settings, all producing OTLP that is semantically identical at
the receiver — they differ only in wire framing and CPU/wire-size
trade-off:

- **`none`** *(default)* — one ResourceLogs entry per buffered Event.
  Cheapest CPU, largest wire. Suitable when `batch_size = 1` or the
  collector tolerates redundancy.
- **`resource`** — Events sharing a Resource collapse into a single
  ResourceLogs entry; their ScopeLogs sit side-by-side under it.
- **`scope`** — as `resource` plus Events sharing a Scope inside the
  same Resource collapse into a single ScopeLogs whose
  `log_records[]` accumulates everything. Smallest wire, slightly
  higher CPU (Resource and Scope equality scans).

Resource and Scope equality is order-insensitive on attribute lists
because proto3 makes no canonical-order promise on the wire.

### Added — `otlp` output retry with exponential backoff

`retry { max_attempts initial_wait max_wait backoff }` block on the
output, parsed via the same `RetryConfig` shared with the file / tcp
/ http outputs. Internal retry is necessary specifically for the OTLP
output because it batches Events from multiple `write()` calls into
one request — without an internal retry, a single transient ship
failure would lose the entire drained batch (the queue layer's
per-event retry only re-pushes the most recent Event). Exhausted
retries bubble the error up so the queue's secondary / drop policy
still applies. Doubling under exponential backoff is `saturating_mul`
for explicit overflow safety.

### Added — `Value::Bytes` variant in the DSL

The DSL runtime value type gains a first-class `Bytes(bytes::Bytes)`
arm, replacing the `serde_json::Value`-based representation that
silently corrupted non-UTF-8 byte streams via `from_utf8_lossy` /
`String::into_bytes()`. User-facing surface is preserved:

- DSL syntax / semantics unchanged.
- `ingress` / `egress` reads return `Value::String` for UTF-8-clean
  data (the historical case) and only switch to `Value::Bytes` for
  non-UTF-8 content (which the previous code was already mangling).
- Existing primitives keep their return shapes.
- `tap --json` / persistence still emit JSON; `Value::Bytes` is
  encoded as `{"$bytes_b64": "..."}` with `$`-prefix key escaping
  for round-trip safety. The marker is internal; `to_json` /
  `parse_json` reject it.

Cross-primitive Bytes rules: text-only primitives (`upper`, `lower`,
`regex_*`, `contains`, `format`, `to_int`, `to_json`, template
interpolation, property traversal) error on Bytes — the
"気を利かせない" rule. Hash primitives (`md5`/`sha1`/`sha256`) and
`len` accept Bytes natively. `Bytes + Bytes` concatenates byte-wise.

New conversion primitives at the text/binary boundary:
- **`to_bytes(s, encoding="utf8")`** — `utf8` (default) / `hex` / `base64`.
- **`to_string(b, encoding="utf8", strict=true)`** — `utf8` strict (errors
  on invalid UTF-8) or lossy, plus `hex` / `base64` printable forms.

### Breaking — `Event.timestamp` renamed to `Event.received_at`

The `Event` struct field, the reserved DSL identifier, the `format()`
template placeholder, and the JSON serialisation key are all renamed
from `timestamp` to `received_at`. The semantic clarification is that
this field is **strictly the wall-clock time at which this hop received
the event** — input modules never overwrite it from payload contents
(Principle 2: input is dumb transport). Source-claimed event times,
when extractable from the wire, surface in workspace fields like
`syslog_timestamp` / `cef_rt` / `pan_generated_time` via parser
primitives.

The old name was generic enough that some snippets and configs were
treating it as if it carried the source-claimed event time, which it
never reliably does.

**Migration** (mechanical sed across configs and any captured `tap --json`
files):

```sh
find /etc/limpid -name '*.limpid' -exec sed -i \
    -e 's/\${timestamp}/\${received_at}/g' \
    -e 's/%{timestamp}/%{received_at}/g' \
    -e 's/strftime(timestamp,/strftime(received_at,/g' \
    {} +

# Captured tap --json files: rewrite the top-level key
jq -c '.received_at = .timestamp | del(.timestamp)' \
    old-capture.jsonl > new-capture.jsonl
```

There is no deprecation alias — `${timestamp}` and `%{timestamp}` are
hard errors (analyzer / runtime) on v0.5.0+. The 0.5.0 release window
is the right moment for the cut because pre-1.0 breaking changes are
still expected.

### Breaking — schema parsers no longer prefix workspace keys

`syslog.parse` and `cef.parse` previously emitted keys with a
`<schema>_` prefix (`syslog_hostname`, `cef_name`, …) on the rationale
that workspace dumps would stay self-describing when several parsers
populated the same event. In practice the prefix collided with the
*capture* idiom — `workspace.s = syslog.parse(ingress)` produced
`workspace.s.syslog_hostname`, double-prefixed — and made schema
parsers behave inconsistently with format primitives (`parse_json`,
`parse_kv`) which always emit raw keys.

Both schema parsers now return un-prefixed keys (`hostname`,
`appname`, `version`, `name`, …). Namespacing is the operator's job
and is the recommended pattern:

```limpid
workspace.syslog = syslog.parse(ingress)   // workspace.syslog.hostname, ...
workspace.cef    = cef.parse(ingress)      // workspace.cef.version, workspace.cef.src, ...
```

Bare invocation still works (`syslog.parse(ingress)` merges keys flat
into `workspace`) but is collision-prone and discouraged. CEF
extension keys (`src`, `dst`, `act`, …) were never prefixed — those
names are part of the CEF spec and continue verbatim.

**Migration**: rewrite any references to `workspace.syslog_*` /
`workspace.cef_*` in configs and snippets. The capture form is
mechanically equivalent and clearer:

```sh
# 1. capture once at the top of each process body:
#      workspace.syslog = syslog.parse(ingress)
#      workspace.cef    = cef.parse(ingress)
# 2. rewrite the references:
sed -i 's/workspace\.syslog_/workspace.syslog./g; s/workspace\.cef_/workspace.cef./g' \
    /etc/limpid/**/*.limpid
```

### Breaking — `cef.parse` requires `CEF:` at position 0

Previously `cef.parse` located `CEF:` anywhere in the input (via
`find`) so a `<PRI>` syslog wrapper was silently skipped. This
overlapped responsibilities — header stripping is syslog's job, not
CEF's — and could match the literal string `CEF:` if it appeared
elsewhere in the payload.

`cef.parse` now requires the input to start with `CEF:`, erroring
with `cef.parse(): input does not start with \`CEF:\`` otherwise.
The canonical pattern when CEF is transported over syslog is:

```limpid
workspace.syslog = syslog.parse(ingress)
workspace.cef    = cef.parse(workspace.syslog.msg)
```

CEF arriving on transports without a syslog wrapper (HTTP, file
tail, …) is unaffected — `CEF:` is at position 0 already.

### Breaking — `syslog.parse` PRI parsing aligned with RFC 5424 §6.2.1

`syslog.parse` now validates the leading `<PRI>` header strictly: 1–3
ASCII digits, value 0–191, framed by `<` and `>` at the start of the
input. Inputs the previous parser tolerated silently — `<malformed
text>...` (non-digit content), `<999>...` (out-of-range), `<>...`
(empty PRI) — now error with `syslog.parse(): no PRI header`,
matching the behaviour of the sibling `syslog.strip_pri` /
`syslog.set_pri` / `syslog.extract_pri` primitives which already used
the strict scanner.

If you have a flow that depended on the old lax behaviour to ingest
non-syslog payloads via `syslog.parse`, switch to a different parser
(`parse_kv`, `regex_parse`, or a snippet) — calling `syslog.parse` on
something that isn't syslog has no defined output anyway.

### Added — `syslog.parse` emits `pri`, `facility`, `severity`, `timestamp`

Beyond the structural fields, `syslog.parse` now returns:

- **`pri`** (Int, 0–191) — the raw `<PRI>` value
- **`facility`** (Int, 0–23) — `pri / 8`
- **`severity`** (Int, 0–7) — `pri % 8`
- **`timestamp`** (String) — the source-claimed wire timestamp from
  the RFC 5424 / RFC 3164 header (previously dropped silently)

`pri` / `facility` / `severity` are always present (the parser errors
when no valid PRI is found, per the breaking change above). The
timestamp surfaces source-claimed event time for snippets that need
it — e.g. for the OCSF `time` field or the OTLP `time_unix_nano` —
without forcing a separate `extract_pri` + parse pass. The lighter
`syslog.extract_pri` is still available for callers that only need
the PRI byte without tokenising the rest of the header.

### Breaking — `output file` path templates are stricter

The `path` template renderer in the `file` output gained four guards
that reject configs the previous lax renderer accepted silently. Each
fires before any byte hits disk, per Principle 1 (zero hidden
behaviour).

- **Per-interpolation slash strip.** Every `${...}` result has
  forward and back slashes replaced with `_`, so an interpolation
  cannot smuggle a path separator into the rendered path. The
  invariant is "one interpolation = one path component"; directory
  structure has to live in the literal parts of the template.
- **`..` rejected anywhere in the rendered path.** After all
  interpolations resolve, the path is split on `/` and any component
  exactly equal to `..` causes the write to error rather than being
  silently rewritten.
- **Empty interpolation rejected.** An interpolation that evaluates
  to the empty string errors instead of producing surprise paths
  like `/foo//bar` or `/foo/.log`.
- **Trailing-slash / no-filename rejected.** A rendered path that
  ends in `/` (no filename component) errors before the auto-mkdir
  runs, so a stray template like `/var/log/${workspace.host}/`
  cannot create empty directories silently.

Configs that depended on any of these silent rewrites should
sanitise the inputs upstream (`regex_replace`, explicit fallbacks in
a `process` block) and reference the cleaned workspace key from the
template. Worked examples are in the
[`output file`](docs/src/outputs/file.md) reference.

### Breaking — `format()` primitive removed

The `format(template)` primitive — which expanded `%{...}` placeholders against the current event — has been removed. The `${expr}` interpolation that any string literal supports is strictly more capable: it accepts any DSL expression rather than the limited `%{event.x}` / `%{workspace.x}` set, and it's resolved at parse time so typos are caught by `--check`.

**Migration**: rewrite `format("...")` calls to interpolated string literals.

```limpid
// before
egress = format("[%{source}] %{workspace.cef_name}: %{egress}")

// after
egress = "[${source}] ${workspace.cef.name}: ${egress}"
```

The `%{...}` syntax is gone entirely; `${expr}` is the single template form.

### Breaking — `to_json()` requires an argument

`to_json()` (no argument) used to serialise the entire `Event` (received_at + source + ingress + egress + workspace) as JSON — the same shape as `tap --json`. In practice operators almost always wanted the workspace alone (the parsed/enriched form to ship downstream), so the no-arg default was a hidden footgun.

`to_json` now requires exactly one argument. The most common pattern:

```limpid
egress = to_json(workspace)
```

For the old whole-event behaviour, build the shape explicitly: `to_json({received_at: received_at, source: source, workspace: workspace})`.

### Added — `parse_kv` separator argument

`parse_kv(text, separator)` lets the caller pass a single-byte
separator (default `' '`). Comma-separated KV payloads — common in
Cisco ASA, Microsoft Defender, and various OEM telemetry — now
parse without a regex pre-pass:

```limpid
workspace.kv = parse_kv(workspace.syslog.msg, ",")
// "a=1,b=2,c=\"three,four\"" → {a: "1", b: "2", c: "three,four"}
```

Quoted values still work and may contain the separator (e.g. a comma
inside a quoted string when separator is comma). The defaults hash
literal can sit either as the second argument (when separator is the
default space) or as the third (after an explicit separator).

### Breaking / Added — `Value::Timestamp` first-class DSL type

The DSL gains a typed `Value::Timestamp(DateTime<Utc>)` value arm.
Inputs in any timezone (RFC3339 with offset, naive + explicit `tz`
argument, etc.) are normalised to UTC at the boundary, so the
runtime never has to reason about mixed offsets.

Previously every timestamp travelled through the runtime as an
RFC3339 `Value::String` — type-unsafe, repeated parse cost, and easy
to typo into `contains(received_at, "2026")` (silently false because
of substring semantics).

Now:

- **`received_at`** → `Value::Timestamp` (was `Value::String`)
- **`timestamp()`** (new, replaces `now()`) → `Value::Timestamp`
- **`strptime(value, fmt[, tz])`** → `Value::Timestamp` (was String)
- **`strftime(timestamp, fmt[, tz])`** — first argument must be a
  `Value::Timestamp` (was String, parsed RFC3339 internally).
  Passing a string is a clear type error: `strftime(): first argument
  must be a timestamp, got string`.
- **`to_int(timestamp)`** → unix nanoseconds (`i64`), matching OTLP
  `time_unix_nano`. So `to_int(received_at)` is the natural way to
  get an epoch-nanos number.
- **String coercion** of `Value::Timestamp` (e.g. `${received_at}`,
  `to_string()`-style paths) renders RFC3339 — the user-visible
  surface is unchanged from 0.4 for type-correct configs.

DSL syntax does **not** change. Existing type-correct expressions
(`strftime(received_at, "%Y-%m-%d", "local")`, `${received_at}`) keep
working byte-for-byte. Only code that round-tripped timestamps
through string operations (`contains(received_at, "...")`,
`len(received_at)`, regex on `received_at`) errors at the analyzer or
runtime — those were always meaningless on a timestamp and now fail
loudly.

`now()` is removed; rename call sites to `timestamp()`. The new name
matches the value type it returns and reads consistently with
`received_at`.

### Breaking — `tap --json` and `inject --json` use unix nanoseconds for `received_at`

`tap --json` previously emitted `received_at` as an RFC3339 string;
it now emits an `i64` of unix nanoseconds, matching OTLP
`time_unix_nano`. `inject --json` reads the same wire form.
Pre-0.5 captures (`*.jsonl` files holding RFC3339 strings) need to
be migrated before replay:

```bash
jq -c '.received_at = (.received_at | sub("\\.\\d+"; "") | strptime("%Y-%m-%dT%H:%M:%S%z") | mktime * 1000000000)' \
    old-capture.jsonl > new-capture.jsonl
```

(For sub-second precision use a real script — `jq` doesn't carry
nanos. The simpler migration is to discard old captures; nothing
about pipeline correctness depends on replaying historical traffic
through the new format.)

### Added — host / version primitives

- **`hostname()`** → `String` — the local machine's hostname, resolved at every call via `gethostname(2)`. Useful for tagging events with the forwarder's identity (`workspace.forwarded_by = hostname()`) and populating OTLP `host.name` resource attributes.
- **`version()`** → `String` — the limpid daemon's version baked in at compile time (e.g. `"0.5.0"`). Useful for provenance markers and OTLP `service.version`.

`hostname()` was previously referenced in the OTLP example block in the docs but was not actually implemented — that drift is closed.

### Added — `starts_with` / `ends_with` string predicates

Two new flat primitives complement `contains`:

- **`starts_with(haystack, needle)`** — `true` if `haystack` begins with `needle`.
- **`ends_with(haystack, needle)`** — `true` if `haystack` ends with `needle`.

Use these when *position* matters — e.g. dispatching to the right
parser based on a leading prefix (`starts_with(workspace.syslog.msg,
"CEF:")`) — rather than `contains`, which matches anywhere and would
fire on a literal `CEF:` string buried elsewhere in the payload.

### Added — DSL primitives

- **`to_int(x)`** — coerce a value to `i64` (strings, floats, bools, nulls);
  returns `null` on unparseable input. Primary use: casting CEF extension
  values and CSV column strings to numeric OCSF fields (ports, session IDs).
- **`find_by(array, key, value)`** — locate the first object in an array
  whose `key` field equals `value`. No type coercion; `null` on no match.
  Designed for identity-based access to schemas that ship arrays-of-objects
  (MDE evidence, OCSF observables).
- **`csv_parse(text, field_names)`** — parse a single CSV row into an object
  keyed by the supplied field names, with RFC 4180 quoting. Replaces the
  `regex_parse` workaround for vendors (most notably Palo Alto) that emit
  100+-field positional CSV syslog records.
- **`len(x)`** — cardinality for `Array` (elements), `String` (Unicode
  characters), `Object` (top-level keys). Scalars return `null`.
- **`append(arr, v)` / `prepend(arr, v)`** — return a new array with `v`
  added at the back / front. Input is unchanged; callers re-bind.

### Added — DSL arrays (positionless collections)

- **Array literals** (`[a, b, c]`, `[]`, mixed types, nesting, trailing
  commas) are now first-class expressions, evaluating to `Value::Array`
  at runtime. Grammar, AST (`ExprKind::ArrayLit`), parser, evaluator, and
  analyzer (`FieldType::Array`) all updated.
- **No positional access.** `arr[n]` and `arr[n] = v` are intentionally
  absent from the grammar. Arrays are addressed by identity (`find_by`,
  `foreach`) and mutated by "back / front" semantics (`append`,
  `prepend`). Numeric indexing drifts under insert / delete; identity
  addressing survives. See
  `docs/src/processing/user-defined.md#arrays` for the rationale.

### Fixed — security hardening from the v0.5.0 audit

- **OTLP output: header values no longer logged on validation failure.**
  The configured `headers { ... }` block typically holds bearer tokens
  / API keys. Previously, a malformed value would produce a
  `tracing::warn!` containing both key and value verbatim — leaking
  the credential into the log stream on misconfiguration. Now logs
  the key only, with explicit `value redacted`.
- **OTLP output: graceful-shutdown buffer warning.** `OtlpOutput`
  gained the `Drop` impl that `HttpOutput` already had: aborts the
  pending deferred-flush task and warns operators about events still
  in the buffer at shutdown. The events are not actually lost (the
  queue layer re-delivers from spool), but the count is now visible.
- **OTLP/HTTP: bounded decode-error log line.** `serde_json` /
  `prost` error wording is capped at 256 characters in the warn log
  to remove a pathological-payload log-amplification primitive.
- **OTLP gRPC input: panic-free peer fallback.** The `remote_addr()`
  fallback for non-TCP transports now constructs the unspecified
  `SocketAddr` directly instead of parsing a constant — removes a
  panic seed that any future refactor of the literal could revive.
- **OTLP output retry: saturating doubling.** `wait * 2` under
  exponential backoff is `saturating_mul(2)`. The realistic reach of
  `Duration` overflow is "never" (~584 years) but the explicit bound
  removes another panic seed.
- **`hostname()` panic-safe.** The `gethostname` 0.5.x crate panics
  on `gethostname(2)` syscall failure (chroot / namespace edge
  cases — vanishingly rare in practice). The primitive now wraps
  the call in `catch_unwind` and degrades to `Value::Null` on
  unwind, so a tokio task can't take the daemon down.
- **`to_int(Float)` rejects non-finite values.** `NaN` and `±∞`
  used to slip through `as i64` (NaN → 0, ∞ → `i64::MIN`/`i64::MAX`),
  both of which violate Principle 1. Finite-but-out-of-range floats
  still saturate (matching the documented `as`-cast semantics);
  non-finite values fall through to the same partial-data `Null`
  path as unparseable strings.

### Refactored — TLS helper centralization

`crate::tls` now owns the `tls { cert key ca }` block parser
(`TlsConfig::from_properties_block`) and the rustls `CryptoProvider`
installer (`install_default_crypto_provider`), both of which were
duplicated across `syslog_tls`, `otlp_grpc` (input), and `otlp`
(output) after the OTLP push. Consolidation keeps error wording
uniform across modules and removes the only direct duplication
flagged by the v0.5.0 abstraction review.

### Known limitations

- **`otlp_http` server-side TLS** is not implemented; front the input
  with a TLS-terminating proxy (envoy / nginx / traefik) or use
  `otlp_grpc` for native TLS. Native HTTPS support is queued for
  v0.5.x.
- **Selective re-send of OTLP `partial_success.rejected_log_records`**
  is logged as a warning only; the dedicated retry-just-the-rejects
  path is queued for v0.5.x. Transport-level retry shipped in this
  release covers hard failures (connection refused, 5xx, …).

## [0.4.0] - 2026-04-24

Testability release. Builds the static analyzer and observability
tooling on top of the DSL finalised in v0.3.0. No DSL breaking changes
— `limpid --check` does more, pipelines behave the same.

### Added — `limpid --check` static analyzer

- Full type-aware analyzer lives in `crates/limpid/src/check/` and
  runs whenever `limpid --check <config>` is invoked. It replaces the
  former "syntax OK" pass with real dataflow and type checking.
- Phase 2 type checking: `FieldType` + `Bindings` thread structural
  types through pipelines; function argument / return type signatures
  (`FunctionSig`), assignment type conflicts, operator type checks, and
  parser-function return shapes are all verified.
- Parser functions (`parse_json`, `parse_kv`, `syslog.parse`,
  `cef.parse`, `regex_parse`) declare the workspace keys they produce
  via `ParserInfo`; downstream references to those keys are verified.
- Phase 3 UX: diagnostics are rendered rustc-style with source snippet
  + caret, "did you mean" Levenshtein suggestions for unknown
  identifiers / functions, and clear summary + footer lines.
- Expr-level span: diagnostics carry precise source spans from
  expression nodes (not just statements), so the caret points at the
  offending sub-expression (`lower(workspace.count)` → carets the arg).
- `include "<glob>";` in configs is expanded by the analyzer with a
  cycle-safe source map, and summary counts (input / output / process /
  pipeline) are emitted per check.
- Footer: clean configs end with
  `<path>: Configuration OK (N pipeline(s), M process(es); dataflow check passed)`;
  configs with warnings include the warning count; configs with errors
  exit 1 with `error: N error(s) found`.

### Added — CLI flags

- `--strict-warnings`: promotes warning count to exit-2 (diagnostic
  level stays warning). CI-friendly switch for "warnings are failures."
- `--ultra-strict`: promotes **unknown-identifier** warnings to errors
  (exit 1). Distinct axis from `--strict-warnings` — this one changes
  the diagnostic level, not just the exit code. The two flags compose:
  unknown idents become errors, other warnings can still trigger
  exit-2. Category is tagged via `DiagKind`; `UnknownIdent` is the
  currently promoted class.
- `--graph[=<format>]`: emits a structural view of every pipeline to
  stdout. Formats: `mermaid` (default, GitHub-renderable),
  `dot` (Graphviz), `ascii` (terminal-only tree). Analyzer output stays
  on stderr so `--graph | pbcopy` etc. works cleanly.

### Added — documentation

- `docs/src/operations/schema-validation.md` — operations guide for
  schema validation. Covers the design decision to not ship an in-tree
  validator, the `limpidctl tap --json | <validator>` recipe (OCSF /
  ECS / custom JSON Schema), and the alternatives that were rejected
  (in-tree validator, DSL schema annotations, runtime per-event
  checking). Cross-linked from `operations/tap.md`.

### Changed — internals

- `Module::schema()` removed. Input / output modules no longer declare
  a data contract: they are I/O-pure (bytes in / bytes out) and have
  nothing to advertise. Schema information is carried by
  `FunctionSig` / `ParserInfo` on the function registry, which is where
  the analyzer looks. `modules/schema.rs` now only exports the
  `FieldType` / `FieldSpec` vocabulary.
- AST `Expr` became a wrapper struct (`Expr { kind: ExprKind, span }`)
  to carry per-expression spans without rewriting every pattern match.
- Unused `name_span` / `key_span` fields on def / property AST nodes
  (left as `#[allow(dead_code)]` placeholders) were removed; they can
  come back if a future analyzer phase needs them.
- Diagnostic category is routed via `DiagKind` enum (`UnknownIdent` /
  `TypeMismatch` / `Dataflow` / `Other`) instead of message-string
  heuristics, so category rendering and `--ultra-strict` promotion
  share the same source of truth.

### Security / hardening

- Snippet renderer sanitises ASCII control bytes (0x00–0x1F minus `\t`,
  and 0x7F) to `?` before writing the source line to stderr. Prevents
  ANSI OSC/CSI injection through config contents displayed in a
  reviewer's terminal.
- `include "<glob>";` is now confined to the config's root directory.
  Absolute paths and `..` traversal outside that root are rejected with
  a clear error. Prevents an include line from silently pulling in
  arbitrary files (`/etc/passwd`, `~/.ssh/*` etc.) or from leaking the
  first bytes of such files via a pest parse error.

### Documentation fixes

- `limpidctl check` references in operations / pipelines / processing
  docs corrected to `limpid --check` (check lives in the daemon binary,
  not the CLI tool — this was the Block 1 decision during v0.3.0
  restructure, but the docs had drifted).

## [0.3.0] - 2026-04-24

DSL stabilization release. This is a broad pre-1.0 breaking change that
settles the Event model, function namespaces, and core shape so that
future work (analyzer polish, snippet library, transport expansion) can
build on a final-form DSL without further surface-level churn.

### Breaking — Event model renamed

- `Event.raw` → `Event.ingress` (immutable bytes received on this hop)
- `Event.message` → `Event.egress` (bytes written on the wire by the output)
- `Event.fields` → `Event.workspace` (pipeline-local scratch namespace)
- `tap --json` / `inject --json` key names follow the rename; existing
  dumped replay files need `sed` (see `docs/src/operations/upgrade-0.3.md`)

### Breaking — Event core is now schema-agnostic

- `Event.facility` / `Event.severity` removed. These were syslog-specific
  metadata masquerading as pipeline-wide state; in a world where OTLP /
  OCSF / vendor JSON are first-class citizens, they do not belong in the
  Event core.
- DSL assignments `facility = N` / `severity = N` are now "unknown
  assignment target" errors. The PRI byte is constructed explicitly via
  the new `syslog.set_pri(egress, facility, severity)` function.
- `syslog.extract_pri(bytes)` returns the numeric PRI for reading.

### Breaking — Native process layer removed

- `modules/process/` is gone in its entirety. Pipeline statements like
  `process parse_syslog` no longer resolve to built-ins — schema-specific
  parsers are DSL functions (`syslog.parse(ingress)` etc.) invoked as
  statements inside an inline `process { ... }` block, and format
  primitives (`parse_json`, `parse_kv`, `regex_replace`) are flat DSL
  functions.
- `prepend_source` / `prepend_timestamp` have no direct replacement; the
  upgrade guide shows the `+` / `strftime` rewrite.

### Added — dot-namespaced function call syntax

- `<namespace>.<fn>(args)` grammar. Schema-specific functions declare their
  identity in the name. `parse_syslog(raw)` / `parse_cef(raw)` /
  `strip_pri(msg)` become `syslog.parse(ingress)` / `cef.parse(ingress)` /
  `syslog.strip_pri(egress)`. Flat primitives (JSON/KV/regex/hash/table)
  keep the bare-name form.
- New functions: `syslog.set_pri`, `syslog.extract_pri`, `regex_parse`,
  `hostname()`.

### Added — `regex_parse(target, pattern)`

- Named-capture extraction with dotted capture names producing nested
  objects: `(?P<date.month>\\w{3})` merges into `workspace.date.month`.
  Returns `Object` (bare-statement merges into `workspace`) or `null`.
- `regex_extract` remains as the single-value extractor.

### Added — `let` bindings

- `let x = <expr>` inside a `def process { ... }` body. Process-local
  scratch that keeps `workspace` clean of intermediate values. Bare-ident
  resolution is `LocalScope → Event metadata → error`.

### Added — pipeline fan-in

- `input a, b, c;` accepts multiple comma-separated inputs feeding the
  same pipeline body. Motivation: HA syslog (two redundant feeds running
  the same dedup / transform pipeline) no longer requires copy-pasting
  the pipeline twice.

### Added — `${expr}` template interpolation + string `+`

- `"prefix-${workspace.foo}-suffix"` interpolates any DSL expression.
  Old `%{name}` shorthand in `format()` has been removed; placeholders
  must be either reserved event names (`ingress`, `egress`, `source`,
  `timestamp`, `severity`, `facility`) or explicit `workspace.xxx` /
  `let`-bound names.
- `+` operator concatenates strings (falls back to arithmetic for
  numeric operands).

### Added — `strftime`, `hostname`

- `strftime(timestamp, format, tz?)` formats an RFC 3339 timestamp.
- `hostname()` returns the daemon's system hostname; portable configs
  can use `"${hostname()}"` in templates instead of hardcoding.

### Added — `output file` path templates via DSL evaluator

- `output file { path "/var/log/${source}/${strftime(timestamp, \"%Y-%m-%d\")}.log" }`
  evaluates the DSL expression per event instead of going through the
  legacy string template.

### Added — Design Principles page

- `docs/src/design-principles.md` publishes the five principles that
  govern limpid's scope (zero hidden behavior, I/O purity, domain
  knowledge as DSL snippets, only `egress` crosses hops, schema
  identity via namespaces).

### Added — developer / example docs

- `docs/src/processing/design-guide.md` — process design guide for
  contributors writing snippet library entries.
- `docs/src/pipelines/multi-host.md` — end-to-end worked example of a
  edge-host → relay → AMA multi-host pipeline, highlighting how
  the `tap` / `inject` primitives and the RFC 5424 hop contract turn a
  distributed pipeline into something you can reason about from one
  config.

### Changed — function code organization

- `crates/limpid/src/functions/` is now a tree of one-file-per-function
  modules: `primitives/` (flat), `syslog/` (dot namespace), `cef/`
  (dot namespace). The old `mod.rs` megafile is gone.
- Module trait introduced (`crates/limpid/src/modules/mod.rs`):
  `Module: Sized { fn schema() -> ModuleSchema; fn from_properties(...) }`.
  Replaces the former `FromProperties`. `schema()` is unused in-tree
  today but reserved for the upcoming analyzer (v0.4.0).

### Changed — hardening

- `limpid` and `limpidctl` restore `SIG_DFL` for SIGPIPE, so piped
  output (`limpidctl stats | head`) exits cleanly instead of panicking.
- `output http`: emits a `WARN` log when `verify false` disables TLS
  certificate validation, and the setting is documented as
  debugging-only.
- Control socket (`/var/run/limpid/control.sock`): max 8 concurrent
  connections, max 16 MiB per inject stream, max 4 KiB per command line.
- `syslog_tls` certificate and key loading moved off the async runtime
  via `spawn_blocking` to avoid stalling the reactor at startup.
- `fmt: cargo fmt --all` applied once across the tree so subsequent
  diffs are free of cosmetic noise.

### Internal refactors

- `<PRI>` header parsing consolidated into a single `parse_leading_pri`
  helper (was duplicated across `strip_pri`, `extract_pri`, `set_pri`).
- `values_equal` merged into `values_match` as the single equality
  routine for both `==`/`!=` and `switch` arms.
- TCP and Unix-socket outputs share a `PersistentConn` trait encoding
  the common "connect on first write, reconnect on broken pipe" pattern.
- `tls::build_client_config` (speculative dead code) removed; TLS client
  support will be reintroduced when an output needs it.

### Removed

- `modules/process/` (entire directory) and the `ModuleRegistry`
  process API (`register_process` / `call_process` / `process_names` /
  `ProcessFn`).
- `%{name}` shorthand in `format()` templates.
- `FromProperties` trait (absorbed into `Module`).

### Migration

See `docs/src/operations/upgrade-0.3.md` for end-to-end migration
recipes including `sed` snippets for the Event model rename, the
function rename table, and worked examples of replacing every removed
native process with its DSL function equivalent.

## [0.2.2] - 2026-04-24

### Added

- `limpidctl inject --replay-timing[=<factor>]` — replays events at their
  original timing using each event's top-level `timestamp` field. Accepts
  `realtime` (= `1x`) or a factor like `10x` / `0.2x`. Defaults to `1x`
  when given without a value. Requires `--json`.

### Documentation

- `docs/src/operations/tap.md` — cadence-faithful replay section with
  examples (default / 10x / 0.2x / realtime), `--json` requirement, and
  the explicit failure cases (missing or unparseable timestamp, invalid
  factor, backwards timestamp, wall-clock catch-up) so there is no
  hidden behaviour.
- `docs/src/operations/cli.md` — `--replay-timing` entry in the CLI
  quick reference.

## [0.2.1] - 2026-04-18

### Fixed

- `--test-pipeline` now loads `table { ... }` global blocks from the
  configuration. Previously it constructed an empty `TableStore`, which
  caused pipelines using `table_lookup` / `table_upsert` / `table_delete`
  to emit "unknown table" warnings in test mode only.

## [0.2.0] - 2026-04-17

### Added

- `limpidctl inject <input|output> <name>` — pushes raw lines into a
  named input's event channel, or directly into an output's queue
  (bypassing pipelines entirely). Symmetric with `limpidctl tap`.
- `inject --json` — pushes full Event JSON (as emitted by `tap --json`),
  enabling `tap → inject` roundtrip for replay use cases.
- Control protocol: `inject <kind> <name> [json]`, EOF-terminated.
- Per-inject metrics: `events_injected` (for inputs and outputs) and
  `events_received` (for outputs).
- Prometheus exporter: three new counters (input injected, output
  injected, output received).

### Changed

- `limpidctl stats` output restructured to **Pipelines → Inputs →
  Outputs** ordering with updated counter set.

### Fixed

- `.gitignore` patterns to exclude common secrets layouts.
- `fold_by_precedence`: guard against empty operator lists.
- `tap.rs`: best-effort comment / error-path fixes surfaced by the
  v0.2.0 audit pass.

## [0.1.0] - 2026-04-17

Initial public release. Rust + tokio log pipeline daemon replacing
rsyslog / syslog-ng / fluentd with a single readable DSL (`def input`,
`def process`, `def output`, `def pipeline`). Includes syslog (UDP/TCP/
TLS) / tail / journal / unix socket inputs; file / HTTP / Kafka / TCP /
UDP / unix socket / stdout outputs; in-DSL expression language with
parsers (JSON / KV / CEF / syslog), regex, string templates, tables
with TTL, GeoIP; control socket (`limpidctl tap`, `stats`, `health`);
hot reload via `SIGHUP` with automatic rollback; per-output disk-backed
queues.

[Unreleased]: https://github.com/naoto256/limpid/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/naoto256/limpid/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/naoto256/limpid/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/naoto256/limpid/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/naoto256/limpid/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/naoto256/limpid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naoto256/limpid/releases/tag/v0.1.0

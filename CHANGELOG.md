# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and
runtime shape converge. After 1.0, changes will follow semver strictly.

## [Unreleased]

## [0.5.0] - 2026-04-26

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

### Added — `syslog.parse` exposes header timestamp

`syslog.parse` now writes the parsed RFC 5424 / RFC 3164 timestamp from
the wire header into `workspace.syslog_timestamp` (previously dropped
silently). Snippets that need the source-claimed event time, e.g. for
the OCSF `time` field or the OTLP `time_unix_nano`, can read it
directly. Behaviour is purely additive — existing configs continue to
work.

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

### Added — `syslog.parse` exposes header timestamp

`syslog.parse` now writes the parsed RFC 5424 / RFC 3164 timestamp from
the wire header into `workspace.syslog_timestamp` (previously dropped
silently). Snippets that need the source-claimed event time, e.g. for
the OCSF `time` field or the OTLP `time_unix_nano`, can read it
directly. Behaviour is purely additive — existing configs continue to
work.

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

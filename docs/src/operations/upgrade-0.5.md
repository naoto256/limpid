# Upgrading to 0.5

This release ships first-class OpenTelemetry Protocol (OTLP) support for
both ingest and emit, adds `Value::Bytes` to the DSL runtime, and
introduces a single small breaking rename in the Event model. Configs
written for 0.4 will load on 0.5 after one mechanical `sed`; nothing
else in the DSL surface changed.

## Breaking changes

### `Event.timestamp` renamed to `Event.received_at`

The Event struct field, the reserved DSL identifier, the `format()`
template placeholder, and the `tap --json` serialisation key are all
renamed:

| Old | New |
|-----|-----|
| `Event.timestamp` (Rust struct) | `Event.received_at` |
| `${timestamp}` (string template) | `${received_at}` |
| `%{timestamp}` (legacy template) | `%{received_at}` |
| `timestamp` (bare DSL identifier) | `received_at` |
| `tap --json` top-level key `timestamp` | `received_at` |

**Why.** The old name was generic enough to be misread as "the event's
timestamp" in the source-claimed sense, when limpid has always
populated it with the wall-clock moment the *current hop* received the
bytes. The two are not the same thing — a syslog line that crossed a
WAN reaches a forwarder seconds after the originating device generated
it, and the OTLP world distinguishes `time_unix_nano` (source-claimed)
from `observed_time_unix_nano` (receiver-observed) for exactly this
reason. The rename makes the limpid field's meaning unambiguous: this
is the local hop's observation time, full stop.

The source-claimed time, when extractable from the wire, lives in
workspace fields populated by parser snippets:

- `syslog.parse` writes `workspace.syslog_timestamp`
- `cef.parse` writes `workspace.cef_rt`
- vendor parsers (Palo Alto, FortiGate, …) write to whatever field
  the snippet author chose

A composer snippet that needs a "real" event time picks the workspace
field with a fallback to `received_at`:

```
let event_ns = workspace.syslog_timestamp_ns
            ?? workspace.cef_rt
            ?? received_at_to_ns(received_at)
```

There is **no deprecation alias** — `${timestamp}` and bare
`timestamp` are hard errors on 0.5. Pre-1.0 breaking changes are
expected; this is the right window.

**Migrating `*.limpid` configs.** A single `sed` covers most call
sites:

```bash
# Run from your config directory (e.g. /etc/limpid/). Review with
# `git diff` or a dry-run grep first.
find . -name '*.limpid' -exec sed -i \
    -e 's/\${timestamp}/\${received_at}/g' \
    -e 's/%{timestamp}/%{received_at}/g' \
    -e 's/strftime(timestamp,/strftime(received_at,/g' \
    {} +
```

Bare `timestamp` references inside DSL bodies (`if timestamp > ...`,
`workspace.captured = timestamp`, etc.) need a careful pass — a blind
`s/\btimestamp\b/received_at/g` will rewrite comments and string
literals you did not mean to touch. After the `sed` above, grep for
remaining bare hits:

```bash
grep -nE '\btimestamp\b' *.limpid
```

and rewrite each DSL-concept hit by hand. Leave comments and
string-literal text alone unless they describe the DSL.

**Migrating captured `tap --json` files.** `tap --json` on 0.5 emits
`received_at` directly. Captures made on 0.4 use the old `timestamp`
key and will not round-trip through `inject --json` on 0.5 as-is.
Convert in place:

```bash
jq -c '.received_at = .timestamp | del(.timestamp)' \
    old-capture.jsonl > new-capture.jsonl
```

Fresh captures on 0.5 emit the new key directly — no conversion
needed going forward.

## New features

### OTLP — first-class transport across ingest and emit

Three transports, both directions, full proto3 wire:

- **Inputs**: [`otlp_http`](../inputs/otlp-http.md) (`POST /v1/logs`,
  protobuf and JSON) and [`otlp_grpc`](../inputs/otlp-grpc.md)
  (`LogsService.Export`, with optional server-side TLS / mTLS).
- **Output**: [`otlp`](../outputs/otlp.md) — speaks
  `http_json`, `http_protobuf`, or `grpc` per the `protocol`
  property; supports per-batch retry with exponential backoff and
  three batch-merging modes (`none` / `resource` / `scope`).
- **Primitives**: `otlp.encode_resourcelog_protobuf`,
  `otlp.decode_resourcelog_protobuf`, `otlp.encode_resourcelog_json`,
  `otlp.decode_resourcelog_json` — the proto3 ↔ HashLit bridge.
  Composers and semantic mappings live in DSL snippets.

The design decisions and the spec-reading they came from are in
[OTLP — design rationale](../otlp.md). Reading that page before
opening an issue will save everyone time.

### OTLP/HTTP throughput controls

Four orthogonal defense layers on the input side, all opt-in:

| Property | What it bounds |
|---|---|
| `body_limit` *(default `16MB`)* | Bytes per request (HTTP 413 on overflow) |
| `max_concurrent_requests` | In-flight request count → worst-case decode memory = `permits × body_limit` |
| `request_rate_limit` | Sustained req/sec (token bucket) |
| `rate_limit` | Emitted events/sec (per-LogRecord, post-decode — same as `syslog_*`) |

For an exposed-ingress preset, see the example block on the
[`otlp_http`](../inputs/otlp-http.md) reference page.

### `Value::Bytes` in the DSL

The DSL runtime gains a `Bytes(bytes::Bytes)` value variant,
replacing the prior `serde_json::Value`-based representation that
silently corrupted non-UTF-8 byte streams via lossy conversion.
User-facing surface is preserved — `ingress` / `egress` reads still
return `Value::String` for UTF-8-clean data; only non-UTF-8 content
(which used to be mangled) now surfaces as `Value::Bytes` and can be
hashed (`md5`/`sha1`/`sha256`), measured (`len`), or converted
explicitly via the new `to_bytes(s, encoding)` / `to_string(b,
encoding, strict)` primitives.

Text-only primitives (`upper`, `lower`, `regex_*`, `format`,
template interpolation, …) error on `Bytes` rather than silently
coercing — the "気を利かせない" rule. See the
[functions reference](../processing/functions.md) for the full
cross-primitive table.

### DSL primitives

Five new flat primitives:

- **`csv_parse(text, field_names)`** — RFC 4180 single-row CSV parser.
- **`find_by(array, key, value)`** — locate the first object in an
  array whose `key` field equals `value`. Identity-based array
  access; pairs with the new array literals (`[a, b, c]`).
- **`len(x)`** — cardinality for arrays, strings (Unicode characters),
  or objects (top-level keys).
- **`append(arr, v)` / `prepend(arr, v)`** — return a new array with
  `v` added at the back / front. Arrays are positionless; this is
  the supported mutation idiom.
- **`to_int(x)`** — coerce to `i64` (strings, floats, bools, nulls).

### DSL arrays

Array literals (`[a, b, c]`, `[]`, mixed types, nesting) are now
first-class expressions. Arrays are addressed by *identity*
(`find_by`, `foreach`) and mutated by back/front semantics
(`append`, `prepend`); positional access (`arr[n]`) is intentionally
absent, because numeric indices drift under insert / delete and
identity addressing survives. See
[User-defined Processes — Arrays](../processing/user-defined.md#arrays)
for the rationale.

### `syslog.parse` exposes header timestamp

The parsed RFC 5424 / RFC 3164 timestamp from the syslog header now
surfaces in `workspace.syslog_timestamp`. Snippets that need the
source-claimed event time can read it directly. Behaviour is purely
additive — existing configs continue to work.

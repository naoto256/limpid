# Built-in Functions

Built-in functions are primitives implemented in Rust and shipped with the daemon. They cover the parts that have to live close to the runtime — parsers, encoders, regex, hashing, GeoIP, table operations, OS helpers — and adding a new one means a Rust commit and a daemon rebuild.

```
def process parse_cef_line {
    workspace.syslog = syslog.parse(ingress)
    workspace.cef    = cef.parse(workspace.syslog.msg)
    workspace.payload = parse_json(workspace.cef.msg)
}
```

Two call surfaces:

- **Flat primitive** — `name(args...)`. Schema-agnostic helpers that don't depend on any particular log format. JSON / KV parsing, regex, hashing, timestamp formatting, table operations, GeoIP, and OS-level helpers all live here.
- **Dot namespace** — `<schema>.<name>(args...)`. Schema-specific helpers declare the schema they bind to in their name: `syslog.parse(ingress)`, `cef.parse(ingress)`, `syslog.set_pri(egress, 16, 6)`. See the [*Schema-specific functions live under a schema namespace*](../design-principles.md#schema-specific-functions-live-under-a-schema-namespace) operating rule for the rationale.

The judgement rule for whether a function is namespaced is a single question: does its behavior follow a specific schema specification (RFC 3164/5424, ArcSight CEF, OCSF, …)? If yes, the schema's name is part of the function's name. If no, it is a flat primitive.

This page is the reference for every built-in. Operators can also declare their own pure DSL helpers — see [User-defined Functions](./user-defined.md) for `def function`; both surfaces register into the same dispatch table, so the call-site syntax is identical.

## Call syntax

### Bare statements vs assignments

A parser function returns a `Value::Object` whose keys are taken from the parse — `hostname`, `appname`, `msg` for `syslog.parse`; `version`, `name`, `severity`, plus CEF extension keys (`src`, `dst`, `act`, …) for `cef.parse`; whatever the source JSON contains for `parse_json`. There are two ways to consume that object inside a process body.

**Bare statement** — the returned object's top-level keys are merged directly into `workspace`:

```
def process parse_fw_flat {
    syslog.parse(ingress)        // workspace.hostname, workspace.msg, ...
}
```

**Capture into a path** — the object lands as a single value at the assigned path:

```
def process parse_fw_namespaced {
    workspace.syslog = syslog.parse(ingress)   // workspace.syslog.hostname, workspace.syslog.msg, ...
    workspace.cef    = cef.parse(ingress)      // workspace.cef.version, workspace.cef.src, ...
}
```

> **Recommendation: always capture into a namespace.** The bare form is supported, but two parsers that emit overlapping key names (e.g. `syslog.parse` and `cef.parse` both producing a `version` field, or `parse_json` of an OCSF event clobbering `syslog.parse` output) will silently overwrite each other when merged flat into `workspace`. Capturing into `workspace.syslog` / `workspace.cef` / `workspace.ocsf` keeps each parser's output isolated and makes downstream references (`workspace.syslog.msg`, not `workspace.msg`) self-describing. Bare is fine for one-off scripts and tests; production processes should namespace.

When a function returns `null` (e.g. `table_upsert(...)`), the bare statement is silently accepted. Any other return type as a bare statement is an error — it almost always means a value was discarded by mistake.

Other assignment targets:

```
let pri = syslog.extract_pri(egress)    // process-local scratch (scalar only)
egress  = syslog.strip_pri(egress)      // overwrite egress
egress  = syslog.set_pri(egress, 16, 6) // rewrite the PRI byte
```

## Reference

The remaining sections describe each function — its signature, what it returns, and how to use it. Schema parsers (`syslog.parse`, `cef.parse`) and format primitives (`parse_json`, `parse_kv`) all return un-prefixed keys; namespacing is the operator's job, via `workspace.<name> = parser(...)`.

## syslog.* — RFC 3164 / RFC 5424

### syslog.parse(text[, defaults])

Parses an RFC 3164 (BSD) or RFC 5424 (versioned) syslog header. Auto-detects the version by looking for a single digit followed by a space after `<PRI>`. Errors when no valid `<PRI>` header is present (1–3 ASCII digits, value 0–191, framed by `<` and `>` at the start of the input).

```
workspace.syslog = syslog.parse(ingress)   // recommended: capture under a namespace
syslog.parse(ingress)                       // bare merge into workspace top-level (collision-prone)
```

Returns a `Value::Object`:

| key         | type   | meaning                                       |
|-------------|--------|-----------------------------------------------|
| `pri`       | Int    | raw `<PRI>` value (0..=191)                   |
| `facility`  | Int    | `pri / 8` (0..=23)                            |
| `severity`  | Int    | `pri % 8` (0..=7)                             |
| `timestamp` | String | source-claimed event time (5424 / 3164 token) |
| `hostname`  | String | originating host                              |
| `appname`   | String | app-name (5424) / tag (3164)                  |
| `procid`    | String | process id (when present)                     |
| `msgid`     | String | message id (5424 only)                        |
| `msg`       | String | body after header                             |

`pri` / `facility` / `severity` are always present. String fields appear only when the wire format provides a non-empty, non-`-` value.

The optional `defaults` argument is a hash literal whose keys fill in any field missing from the parse — handy for asserting an expected shape inline.

This function does **not** rewrite `egress` — it only populates the workspace. The wire payload is whatever the next hop expects to receive, which is almost always still a syslog line; rewrites to `egress` are usually surgical (e.g. `syslog.set_pri(egress, 16, 6)` to renormalise the PRI byte), not wholesale replacements.

If you only need the PRI value (e.g. to route on severity without tokenising the rest of the header), reach for the lighter [`syslog.extract_pri`](#syslog-extract_pri-text) instead.

### syslog.strip_pri(text)

Removes a leading `<PRI>` header. Returns the input unchanged if there is no syntactically valid `<N>` header (1-3 digits, value 0-191).

```
egress = syslog.strip_pri(egress)
```

### syslog.set_pri(text, facility, severity)

Writes or rewrites the leading `<PRI>` header. `facility` must be 0-23, `severity` 0-7. If the input already has a valid `<PRI>`, it is replaced; otherwise the new header is prepended.

```
egress = syslog.set_pri(egress, 16, 6)   // local0.info
```

### syslog.extract_pri(text)

Returns the leading `<PRI>` value as a number (0-191), or `null` when no valid PRI is present.

```
let pri = syslog.extract_pri(ingress)
if pri != null and pri < 8 {
    output alert        // emergencies and alerts
}
```

To recover the constituent facility / severity:

```
let pri      = syslog.extract_pri(ingress)
let facility = pri / 8
let severity = pri % 8
```

## cef.* — ArcSight Common Event Format

### cef.parse(text[, defaults])

Parses CEF. The input must start with `CEF:` — syslog wrapper handling is the caller's responsibility, not the CEF parser's. The canonical pattern for CEF over syslog:

```
workspace.syslog = syslog.parse(ingress)
workspace.cef    = cef.parse(workspace.syslog.msg)
```

When CEF arrives on transports without a syslog wrapper (HTTP body, file, …), call directly:

```
workspace.cef = cef.parse(ingress)
```

Returns a `Value::Object` with these header keys:

| key              | type           | meaning                  |
|------------------|----------------|--------------------------|
| `version`        | String         | CEF version (usually `0`) |
| `device_vendor`  | String         | device vendor            |
| `device_product` | String         | device product           |
| `device_version` | String         | device version           |
| `signature_id`   | String         | vendor-specific event id |
| `name`           | String         | human-readable event name |
| `severity`       | Int \| String  | vendor severity (0–10), or the raw string when the producer sent garbage |

Extension `key=value` pairs from the CEF tail (e.g. `src=10.0.0.1 dst=192.168.1.1 act=block`) are emitted alongside the header keys, under the names defined by the CEF spec (`src`, `dst`, `act`, …) — those names are part of CEF, not a limpid convention.

The optional `defaults` argument behaves the same as in `syslog.parse`.

## otlp.* — OpenTelemetry Protocol (logs signal)

Mechanical wire-format encode / decode for the OTLP logs signal,
operating on a singleton `ResourceLogs` (1 Resource + 1 Scope + 1
LogRecord) — limpid's hop contract for OTLP. Composers and
semantic mappings live in DSL snippets, not in Rust (per the *Domain
knowledge in DSL* operating rule);
these primitives are just the proto3 ↔ HashLit bridge.

> The reasoning behind the singleton-ResourceLogs contract, the
> bytes-on-the-hop choice, and the SeverityNumber convention is
> in [OTLP — design rationale](../otlp.md). This section is the
> reference for the four primitives only.

HashLit shape mirrors the proto3 message tree with snake_case keys
so authors write directly against the OTLP spec. The JSON form
applies the canonical OTLP/JSON conventions (camelCase, u64-as-string,
bytes-as-hex) at the wire boundary.

```
workspace.otlp = {
    resource: {
        attributes: [
            { key: "service.name", value: { string_value: "limpid" } },
            { key: "host.name",    value: { string_value: hostname() } }
        ]
    },
    scope_logs: [{
        scope: { name: "limpid", version: "0.5.0" },
        log_records: [{
            time_unix_nano: workspace.event_time_ns,
            severity_number: 9,                   // 9=INFO, 13=WARN, 17=ERROR, 21=FATAL
            severity_text: "INFO",
            body: { string_value: workspace.message },
            attributes: [
                { key: "source.address", value: { string_value: source } }
            ]
        }]
    }]
}
egress = otlp.encode_resourcelog_protobuf(workspace.otlp)
```

`AnyValue` is tagged with the variant name: exactly one of
`string_value`, `bool_value`, `int_value`, `double_value`,
`array_value`, `kvlist_value`, `bytes_value`. Multiple variants on
the same `AnyValue` are an error.

### otlp.encode_resourcelog_protobuf(hashlit) → Bytes

Encode the HashLit as a `ResourceLogs` proto3 message and return
the raw wire bytes. Pair with the [`otlp` output](../outputs/otlp.md)'s
`http_protobuf` or `grpc` protocol.

### otlp.decode_resourcelog_protobuf(bytes) → Object

Inverse of `encode_resourcelog_protobuf`. Used by snippets that
need to inspect / transform an inbound OTLP record:

```
def process redact_pii {
    workspace.otlp = otlp.decode_resourcelog_protobuf(ingress)
    // ... edit workspace.otlp ...
    egress = otlp.encode_resourcelog_protobuf(workspace.otlp)
}
```

### otlp.encode_resourcelog_json(hashlit) → String

Encode as canonical OTLP/JSON (camelCase, u64-as-string, bytes-as-hex).
For the [`otlp` output](../outputs/otlp.md) `http_json` protocol.

### otlp.decode_resourcelog_json(s) → Object

Decode an OTLP/JSON-encoded `ResourceLogs` string (or UTF-8 bytes)
back into the snake_case HashLit form.

### SeverityNumber convention

OTLP defines a 1..24 range, with the canonical level values used in
practice:

| Level | Number |
|-------|--------|
| TRACE | 1 |
| DEBUG | 5 |
| INFO  | 9 |
| WARN  | 13 |
| ERROR | 17 |
| FATAL | 21 |

The four slots within each level (`*2/*3/*4`) are for bridging
finer-grained external systems (Windows Event etc.). limpid snippets
typically only emit the canonical level value.

## String functions

### contains(haystack, needle)

Returns `true` if `haystack` contains `needle` anywhere.

```
if contains(workspace.syslog.msg, "Failed password") {
    output alerts
}
```

### starts_with(haystack, needle) / ends_with(haystack, needle)

Returns `true` if `haystack` starts (resp. ends) with `needle`. Use these when the position matters — for example, dispatching to the right parser based on the leading bytes:

```
workspace.syslog = syslog.parse(ingress)
if starts_with(workspace.syslog.msg, "CEF:") {
    workspace.cef = cef.parse(workspace.syslog.msg)
}
```

### lower(str) / upper(str)

Returns the string in lowercase or uppercase.

```
workspace.syslog.hostname = lower(workspace.syslog.hostname)
```

### regex_match(str, pattern)

Returns `true` if `str` matches the regex pattern.

```
if regex_match(egress, "^\\d{4}-\\d{2}-\\d{2}") {
    workspace.has_date = true
}
```

### regex_extract(str, pattern)

Returns the first capture group (or full match if no groups). Returns `null` if no match.

```
workspace.ip = regex_extract(egress, "(\\d+\\.\\d+\\.\\d+\\.\\d+)")
```

### regex_parse(str, pattern)

Runs `pattern` against `str` and returns a `Value::Object` with one key per **named capture** group (`(?P<name>...)` or `(?<name>...)`). Positional groups are ignored. Use this when one regex extracts several fields at once; `regex_extract` remains the right tool for a single scalar.

Capture names containing `.` build a nested object, so `(?P<date.month>...)` populates `{ date: { month: "..." } }` and sibling dotted names merge under the same parent. Used as a bare statement, the returned object merges into `workspace` exactly like `parse_json` / `parse_kv` / `syslog.parse`.

```
// Bare-statement merge: parse a FortiGate-style header into workspace.
regex_parse(ingress, "^(?P<date>\\S+) (?P<time>\\S+) (?P<host>\\S+) (?P<prog>\\w+):")

// Dotted names in the regex build nested objects on a bare merge.
regex_parse(ingress, "(?P<date.month>\\w{3}) (?P<date.day>\\d+)")
// → workspace.date.month, workspace.date.day

// Same result via capture (recommended — namespacing is explicit).
workspace.date = regex_parse(ingress, "(?P<month>\\w{3}) (?P<day>\\d+)")
// → workspace.date.month, workspace.date.day
```

Returns:

| Outcome | Result |
|---------|--------|
| Pattern matches | Object of named captures (positional groups dropped) |
| Pattern compiles but does not match | `null` |
| Pattern has zero named captures | Empty object (so a bare statement is a safe no-op) |
| Pattern is invalid | Error |

> **Implementation note.** `.` is not a legal capture-name character in the Rust regex engines, so the engine sees the name with `.` rewritten to the internal marker `__DOT__`. Capture names that literally contain `__DOT__` will be misinterpreted — avoid that token in your own names.

### regex_replace(str, pattern, replacement)

Returns the string with all matches replaced. Supports capture group references (`$1`, `$2`).

```
egress = regex_replace(egress, "\\d{16}", "REDACTED")
```

Regex patterns are cached per thread for performance.

## Format parsers

JSON and KV are *formats*, not schemas — they describe how bytes are arranged, not what fields mean. They live in the flat namespace.

### parse_json(text[, defaults])

Parses `text` as JSON and returns the top-level object. Non-object JSON (arrays, scalars) is wrapped under the `_json` key so the return is always an object.

```
workspace.body = parse_json(egress)      // recommended: capture under a namespace
parse_json(egress)                       // bare merge top-level keys into workspace (collision-prone)
```

The returned object's keys are whatever the source JSON contains at the top level — limpid does not transform or filter them.

### parse_kv(text[, separator][, defaults])

Parses `key=value` pairs (handling quoted values). Tokens without `=` are skipped.

```
workspace.kv = parse_kv(egress)          // recommended: capture under a namespace
parse_kv(egress)                          // bare merge into workspace
// egress = `date=2026-04-15 srcip=10.0.0.1 action=deny msg="login failed"`
// → workspace.kv.date, workspace.kv.srcip, workspace.kv.action, workspace.kv.msg
```

`separator` is a single ASCII byte (default `' '`). Comma-separated payloads (Cisco ASA, Microsoft Defender, OEM telemetry) pass an explicit separator:

```
workspace.kv = parse_kv(workspace.syslog.msg, ",")
// "a=1,b=2,c=\"three,four\"" → {a: "1", b: "2", c: "three,four"}
```

The optional `defaults` hash literal fills missing keys (same shape as `parse_json` defaults). It can be the second argument when separator is the default space, or the third after an explicit separator.

The returned object's keys come from the parsed input as-is. Useful for FortiGate, Palo Alto, and similar firewall log formats.

### csv_parse(text, field_names)

Parses a single CSV row into an object keyed by the supplied field names. `field_names` is a JSON array of strings; positional columns that line up with empty names (`""`) are skipped. Useful for vendor exports that ship long flat rows with no header, most notably Palo Alto Networks syslog logs (100+ positional fields per THREAT / TRAFFIC record).

```
csv_parse(egress, ["future1", "receive_time", "serial", "log_type",
                   "threat_type", "", "generated_time", "src_ip", "dst_ip", ...])
// → workspace.future1 = "1", workspace.receive_time = "2026/04/25 10:00:00", ...
```

| Behaviour | Result |
|-----------|--------|
| Empty cell | `null` |
| Extra columns beyond `field_names` | Dropped silently |
| Fewer columns than `field_names` | Trailing names become `null` |
| Quoted cells (`"a,b,c"`, `"he said ""hi"""`) | RFC 4180 unquoting |
| Non-string `text` or non-array `field_names` | `null` |

Used as a bare statement, the returned object merges into `workspace` like other format parsers.

## Timestamps

Timestamps are a first-class DSL value type (`Value::Timestamp`). `received_at`, `timestamp()`, and `strptime` all return one; `strftime` accepts one. String coercion (e.g. `${received_at}`) renders RFC3339; `to_int(timestamp)` returns unix nanoseconds (matching OTLP `time_unix_nano`); `tap --json` serialises timestamps as integer unix nanoseconds.

Timestamps and strings are distinct types. `contains(received_at, "2026")` is a type error — to inspect the wire form, format it explicitly: `contains(strftime(received_at, "%Y", "UTC"), "2026")`.

### strftime(timestamp, format[, timezone])

Formats a `Value::Timestamp` (such as `received_at`) according to a [`chrono` strftime](https://docs.rs/chrono/latest/chrono/format/strftime/) format string.

```
strftime(received_at, "%Y-%m-%d")          // 2026-04-19
strftime(received_at, "%b %e %H:%M:%S")    // Apr 19 10:30:45
strftime(received_at, "%Y-%m-%d", "local") // convert to local time first
strftime(received_at, "%H:%M", "UTC")      // force UTC
strftime(received_at, "%H:%M", "+09:00")   // fixed offset
```

| Argument | Description |
|----------|-------------|
| `timestamp` | a `Value::Timestamp` (from `received_at`, `timestamp()`, or `strptime`). Passing a string is a type error. |
| `format` | `chrono` strftime format. |
| `timezone` *(optional)* | `"local"`, `"UTC"` (case-insensitive), or `±HH:MM` / `±HHMM`. If omitted, the timestamp's own offset is used. |

An invalid timezone specifier is a loud error — `strftime` never silently returns an empty string.

### strptime(value, format[, timezone])

Inverse of `strftime`. Parses an arbitrary timestamp string with a `strftime`-style format and returns a `Value::Timestamp`.

```
workspace.event_time = strptime(workspace.kv.date, "%Y-%m-%d %H:%M:%S", "UTC")
strptime("2026-04-15T10:30:00+09:00", "%Y-%m-%dT%H:%M:%S%:z")  // tz in format → no third arg
strptime("2026-04-15 10:30:00", "%Y-%m-%d %H:%M:%S", "local")  // naive + local
```

| Argument | Description |
|----------|-------------|
| `value` | timestamp string |
| `format` | `chrono` strftime format |
| `timezone` *(required when format produces a naive datetime)* | `"local"`, `"UTC"`, or `±HH:MM` / `±HHMM` |

If the format includes an offset specifier (`%z`, `%:z`, `%#z`), the third argument is rejected as conflicting. If the format produces a naive datetime, the third argument is required — limpid never silently assumes UTC.

### timestamp()

Returns the current wall-clock instant as a `Value::Timestamp` (UTC). Matches the type of `received_at` and the input shape `strftime` / `strptime` expect.

```
workspace.processed_at = timestamp()
egress = "${strftime(timestamp(), \"%Y-%m-%dT%H:%M:%S%:z\", \"local\")} ${egress}"
```

Resolved at every call (no caching) — successive calls within the same process body see successive instants.

## Object / Array shaping

### coalesce(a, b, c, ...)

Return the leftmost argument that is not `null`; if every argument is
`null`, return `null`. Variadic: accepts ≥ 1 argument. Designed for the
"prefer parsed value, fall back to environment" pattern that recurs
throughout composers and parsers.

```
// Composer: prefer parsed event time, fall back to received_at
let event_time = coalesce(workspace.limpid.time, received_at)

// Parser: pick first source IP that is populated
workspace.limpid.src_endpoint.ip = coalesce(
    workspace.cef.src,
    workspace.cef.sourceTranslatedAddress,
    workspace.syslog.hostname
)
```

| Input | Output |
|-------|--------|
| `coalesce(null, 42, 99)` | `42` |
| `coalesce(1, 2, 3)` | `1` |
| `coalesce(null, null, null)` | `null` |
| `coalesce("", "fallback")` | `""` (empty string is a present value) |
| `coalesce(0, 99)` | `0` (zero is a present value) |
| `coalesce()` | rejected by analyzer / runtime (≥ 1 arg required) |

Only `null` is "passed over". Empty strings, zero, empty objects, and
empty arrays are real present-but-empty values and are returned as-is.
Callers who want "blank string is also absent" express that condition
explicitly (`switch true { x != "" { x } default { y } }`).

All arguments are evaluated (DSL has no short-circuit at call sites).
Since DSL identifiers and built-ins are pure, eager evaluation has no
observable difference from short-circuit at the user level.

### null_omit(value)

Recursively strip `null` **keys** from objects, recursing into the
remaining values (and into array elements). Arrays themselves are not
compacted — a `null` element survives, because that's often the
parser's placeholder ("this slot was unknown") and silently dropping
it would hide the signal.

```
workspace.payload = {
    src: workspace.cef.src,
    dst: workspace.cef.dst,
    user: workspace.cef.suser,        // may be null on machine-only events
    evidences: [
        { file: workspace.cef.fname, hash: workspace.cef.fhash }
    ]
}
egress = to_json(null_omit(workspace.payload))
//   → {"src":"...","dst":"...","evidences":[{"file":"..."}]}
//   (user dropped because it was null; evidences[0].hash dropped
//   because it was null; the array slot stayed)
```

| Input | Output |
|-------|--------|
| `{a: 1, b: null, c: 3}` | `{a: 1, c: 3}` |
| `{a: 1, b: {c: null, d: 2}}` | `{a: 1, b: {d: 2}}` |
| `{a: 1, b: [null, 2, null]}` | `{a: 1, b: [null, 2, null]}` (array unchanged) |
| `{list: [{x: null, y: 2}]}` | `{list: [{y: 2}]}` (recurse into Object elements) |
| `{a: 1, b: {}}` | `{a: 1, b: {}}` (empty container kept) |
| `null` | `null` |
| `42` | `42` (scalar pass-through) |

Designed for the OCSF-shape composer pattern (build a HashLit from
parser-populated workspace fields, then `to_json` for `egress`).
Without `null_omit`, every absent field renders as `"key": null` in
the output — not strictly invalid, but consumers that strictly
validate against OCSF schema (Microsoft Sentinel, Splunk DM) often
choke on it. Pipe through `null_omit` before `to_json` and the
output stays clean without the parser having to conditionally
populate every field.

Why arrays are not compacted: the function name advertises "omit
null *keys*", not "compact arrays". A `null` slot in an array is
often a parser's intentional placeholder, and dropping it would
change the array length silently — which can carry meaning even
though limpid arrays are [positionless](../processing/user-defined.md#arrays).
When array compaction is what you want, use a dedicated array
primitive instead.

## Hash functions

### md5(str) / sha1(str) / sha256(str)

Return the hex digest.

```
workspace.fingerprint = md5(egress)
workspace.hash = sha256(egress)

// Anonymize source IP
workspace.src_hash = sha256(workspace.src)
```

Useful for event deduplication, fingerprinting, or anonymisation.

## Type coercion

### to_bytes(s, encoding="utf8")

Convert a string to raw bytes. The DSL value system distinguishes
`String` (always valid UTF-8) from `Bytes` (raw byte buffer); this
primitive is the explicit text → binary boundary.

| Encoding | Behaviour |
|----------|-----------|
| `"utf8"` (default) | The string's UTF-8 byte representation. Lossless. |
| `"hex"` | Parse as hex (lowercase or upper, even length). `"deadBEEF"` → 4 bytes. |
| `"base64"` | Decode standard RFC 4648 base64 with padding. |

```
workspace.signature = to_bytes(workspace.sig_hex, "hex")
egress = to_bytes(workspace.payload_b64, "base64")
```

Errors on unknown encoding or malformed input (odd hex length,
invalid hex digit, malformed base64). The default `utf8` form is
lossless because Rust strings are always valid UTF-8.

### to_string(b, encoding="utf8", strict=true)

Convert raw bytes to a string. Counterpart of `to_bytes`.

| Encoding | `strict` | Behaviour |
|----------|----------|-----------|
| `"utf8"` (default) | `true` (default) | Invalid UTF-8 errors. |
| `"utf8"` | `false` | Invalid sequences become U+FFFD (lossy). |
| `"hex"` | (ignored) | Lowercase hex pair per byte. |
| `"base64"` | (ignored) | Standard RFC 4648 with padding. |

```
workspace.message = to_string(ingress)                       // strict UTF-8 — error on binary
workspace.message = to_string(ingress, "utf8", false)        // lossy fallback
workspace.signature_b64 = to_string(workspace.sig, "base64") // bytes → printable
```

Text-only primitives (`upper`, `regex_*`, `format`, `to_int`,
`contains`, etc.) reject `Bytes` to keep failure modes explicit;
`to_string` is the way to opt into a textual interpretation.

### to_int(value)

Coerces a value to a 64-bit signed integer. Returns `null` on unparseable input, matching the partial-data policy of `regex_extract` and `table_lookup`.

```
workspace.limpid.src_endpoint.port = to_int(workspace.cef.spt)  // CEF ext: "54321" → 54321
```

| Input | Result |
|-------|--------|
| `Int` | Pass-through |
| `Float` | Truncated toward zero |
| `String` | `str::parse::<i64>` after trimming whitespace; otherwise `null` |
| `Bool` | `1` or `0` |
| `Timestamp` | unix nanoseconds (matches OTLP `time_unix_nano`). `to_int(received_at)` is the natural epoch-ns cast. |
| `Null` | `Null` |
| Array / Object | `Null` |

Motivation: CEF extension values and CSV column values arrive as strings even when carrying numeric content. OCSF schemas commonly require `Integer` for those same fields (ports, session IDs, byte counts). `to_int` is the schema-agnostic cast used by SIEM parser snippets.

## Array helpers

Arrays in limpid are **positionless collections** — you construct them with `[a, b, c]` literals, but the DSL deliberately omits positional access (`arr[n]`) and positional writes (`arr[n] = v`). Element identity, not position, is the addressing model; see [User-defined Processes → Arrays](../processing/user-defined.md#arrays) for the rationale and `find_by` / `foreach` / `append` / `prepend` / `len` below.

### find_by(array, key, value)

Returns the first element of `array` that is an object whose `key` field equals `value`. Returns `null` when nothing matches, the input is not an array, or the key is not a string.

```
workspace.process = find_by(workspace.evidence, "entityType", "Process")
workspace.user    = find_by(workspace.evidence, "entityType", "User")
```

Equality is value-level with no coercion: `find_by(arr, "n", "2")` does not match `{"n": 2}`. Callers who need coercion should cast the value first (`to_int`, string interpolation, etc.). Non-object elements inside the array are skipped silently, so mixed arrays do not cause errors.

Designed for event schemas that carry arrays-of-objects (MDE evidence, OCSF observables, CEF ext lists) where the caller wants "pick the first item matching this type" as a scalar result rather than iterating with `foreach`.

### append(array, value) / prepend(array, value)

Return a new array with `value` added at the back (`append`) or the front (`prepend`). The input array is not mutated — callers re-bind:

```
workspace.limpid.observables = append(workspace.limpid.observables, new_obs)
workspace.high_prio_tags = prepend(workspace.high_prio_tags, "urgent")
```

| Input `array` | Result |
|---------------|--------|
| `Array` | New array with `value` added |
| `Null` | `Null` |
| Anything else (`String` / `Object` / scalar) | `Null` |

`value` may be any type, including `null` — if the caller wants to record "a slot with no value", that's a legitimate element.

These are the only mutation paths for arrays because they identify "where" by insertion-order semantics rather than a numeric index. Middle insertion / removal is out of scope for v0.5.0; use identity-based primitives (future `insert_after_by`, `remove_by`) when that need surfaces.

### len(value)

Cardinality primitive — works for every container-like type:

| Input | Result |
|-------|--------|
| `Array` | Number of elements |
| `String` | Number of Unicode characters (not bytes) |
| `Object` | Number of top-level keys |
| `Null` | `Null` |
| Scalars (`Int` / `Float` / `Bool`) | `Null` |

```
workspace.n_observables = len(workspace.limpid.observables)
workspace.msg_len = len(workspace.syslog.msg)
```

Returning `null` on scalars (rather than `0` or an error) keeps the "not applicable" signal distinguishable from a legitimately empty collection.

## Serialization

### to_json(value)

Serializes a value to a JSON string. Errors if the value (or any nested value) contains `Value::Bytes` — convert explicitly via `to_string(b)` if you mean to embed bytes as text.

```
egress = to_json(workspace)               // common: ship workspace as JSON downstream
workspace.geo_json = to_json(geoip(workspace.src))
egress = to_json({                         // build any shape inline
    received_at: received_at,
    source: source,
    parsed: workspace.cef,
})
```

## Table functions

In-memory key-value tables with optional TTL and max entry limits. Tables are defined in the `table` global block and accessed via `table_lookup`, `table_upsert`, and `table_delete`.

### table_lookup(table, key)

Returns the value for a key, or `null` if not found or expired.

```
workspace.asset_name = table_lookup("asset", workspace.src)
```

### table_upsert(table, key, value, expire?)

Inserts or updates a key. `expire` is TTL in seconds (0 = no expiry, omitted = table default).

Can be used as an expression statement (no assignment needed):

```
table_upsert("seen", workspace._hash, "1", 300)
```

### table_delete(table, key)

Removes a key from the table.

```
table_delete("sessions", workspace.session_id)
```

### Use cases

**Asset enrichment** — look up metadata from a static table loaded at startup:

```
def process enrich {
    workspace.asset = table_lookup("assets", source)
    workspace.owner = table_lookup("owners", source)
}
```

**Event deduplication** — suppress repeated events from the same source within a time window:

```
def process dedup {
    workspace._key = sha256(regex_extract(ingress, "msg=(.+)"))
    if table_lookup("seen", workspace._key) != null {
        drop
    }
    table_upsert("seen", workspace._key, "1", 600)
}
```

**Rate limiting by source** — allow one event per source IP per interval, drop the rest:

```
def process rate_limit_alerts {
    if table_lookup("alert_rate", source) != null {
        drop
    }
    table_upsert("alert_rate", source, "1", 60)
}
```

**Session tracking** — track active sessions and clean up on disconnect:

```
def process track_session {
    if contains(ingress, "session opened") {
        workspace._sid = regex_extract(ingress, "session=(\\S+)")
        table_upsert("sessions", workspace._sid, source, 3600)
    }
    if contains(ingress, "session closed") {
        workspace._sid = regex_extract(ingress, "session=(\\S+)")
        table_delete("sessions", workspace._sid)
    }
}
```

See [Configuration](../configuration.md#table) for table definition options.

## OS / network helpers

### geoip(ip_str)

Returns a GeoIP lookup result as an object with `country`, `city`, `latitude`, and `longitude` fields.

```
workspace.geo = geoip(workspace.src)
// workspace.geo.country = "JP"
// workspace.geo.city = "Tokyo"
```

Requires the `geoip` global block. See [Configuration](../configuration.md#geoip).

Access nested properties with postfix property access:

```
workspace.country = geoip(workspace.src).country
```

### hostname()

Returns the hostname of the machine running the limpid daemon. Resolved at every call via `gethostname(2)`.

```
workspace.forwarded_by = hostname()
```

Useful for tagging events with the forwarder's identity (e.g. when several limpid hosts feed a central collector) or populating OTLP `host.name` resource attributes.

### version()

Returns the limpid daemon's version string, baked in at compile time (e.g. `"0.5.0"`).

```
workspace.processed_by_version = version()
```

Useful for provenance markers and OTLP `service.version` attributes.

## Operators

Expressions support the following operators:

| Operator | Description |
|----------|-------------|
| `==`, `!=` | Equality |
| `<`, `<=`, `>`, `>=` | Numeric comparison |
| `and`, `or` | Logical |
| `not` | Logical negation |
| `+` | Arithmetic addition **or** string concatenation (see below) |
| `-`, `*`, `/`, `%` | Arithmetic (numeric) |

### `+` overloading

If either operand is a string, `+` concatenates after stringifying the other side:

```
egress = "[" + workspace.syslog.hostname + "] " + egress
egress = source.ip + " " + egress
```

If both operands are numeric, `+` is ordinary addition. Mixing with `null`, arrays, or objects is an error — stringify explicitly with `to_json()` first if that is what you want.

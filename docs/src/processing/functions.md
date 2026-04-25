# Expression Functions

Expression functions return values. They appear in conditions, on the right-hand side of assignments, inside `${...}` interpolations, and as bare statements inside a `process` body (where the returned object is merged into `workspace`).

## Call syntax

Functions come in two forms:

- **Flat primitive** — `name(args...)`. Schema-agnostic helpers that don't depend on any particular log format. JSON / KV format parsing, regex, hashing, timestamp formatting, table operations, GeoIP, and OS-level helpers all live here.
- **Dot namespace** — `<schema>.<name>(args...)`. Schema-specific helpers declare the schema they bind to in their name: `syslog.parse(ingress)`, `cef.parse(ingress)`, `syslog.set_pri(egress, 16, 6)`. See [Design Principle 5](../design-principles.md#principle-5--schema-identity-is-declared-by-namespace) for the rationale.

The judgement rule for whether a function is namespaced is a single question: does its behavior follow a specific schema specification (RFC 3164/5424, ArcSight CEF, OCSF, …)? If yes, the schema's name is part of the function's name. If no, it is a flat primitive.

### Bare statements vs assignments

A function call can appear as a bare statement inside a process body:

```
def process parse_fw {
    syslog.parse(ingress)        // bare statement: object return → merged into workspace
    cef.parse(ingress)           // same: extension keys flow into workspace
}
```

When the function returns an object, the object's top-level keys are inserted into `workspace`. When it returns `null` (e.g. `table_upsert(...)`), the bare statement is silently accepted. Any other return type as a bare statement is an error — it almost always means a value was discarded by mistake.

To capture a returned value into a specific location, assign it explicitly:

```
workspace.parsed = syslog.parse(ingress)         // single object under workspace.parsed
let pri          = syslog.extract_pri(egress)    // process-local scratch
egress           = syslog.strip_pri(egress)      // overwrite egress
egress           = syslog.set_pri(egress, 16, 6) // rewrite the PRI byte
```

### Workspace key naming convention

Schema parsers emit fields under a `<schema>_` prefix so a workspace dump stays self-describing even when several schemas have populated the same event:

| Function | Workspace keys produced |
|----------|-------------------------|
| `syslog.parse` | `syslog_hostname`, `syslog_appname`, `syslog_procid`, `syslog_msgid`, `syslog_msg` |
| `cef.parse` | `cef_version`, `cef_device_vendor`, `cef_device_product`, `cef_device_version`, `cef_signature_id`, `cef_name`, `cef_severity` (plus CEF extension keys like `src`, `dst`, `act` copied verbatim — those names are part of the CEF spec) |

Format primitives (`parse_json`, `parse_kv`) emit the keys exactly as they appear in the source — JSON / KV is a format, not a schema, so there is no convention name to prefix with.

## syslog.* — RFC 3164 / RFC 5424

### syslog.parse(text[, defaults])

Parses an RFC 3164 (BSD) or RFC 5424 (versioned) syslog header. Auto-detects the version by looking for a single digit followed by a space after `<PRI>`.

```
syslog.parse(ingress)              // bare: merge fields into workspace
workspace.s = syslog.parse(ingress) // capture object under workspace.s
```

Returns a `Value::Object` with `syslog_hostname`, `syslog_appname`, `syslog_procid`, `syslog_msgid`, `syslog_msg` (each present only when the wire format provides a non-empty, non-`-` value).

The optional `defaults` argument is a hash literal whose keys fill in any field missing from the parse — handy for asserting an expected shape inline.

This function does **not** rewrite `egress`. To replace the wire payload with the parsed MSG body, do it explicitly:

```
syslog.parse(ingress)
egress = workspace.syslog_msg
```

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

This function is the explicit replacement for the old `facility = N` / `severity = N` magic assignments. With facility / severity removed from the Event core, PRI is now a pure byte operation against `egress`.

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

## otlp.* — OpenTelemetry Protocol (logs signal)

Mechanical wire-format encode / decode for the OTLP logs signal,
operating on a singleton `ResourceLogs` (1 Resource + 1 Scope + 1
LogRecord) — the v0.5.0 hop contract for OTLP. Composers and
semantic mappings live in DSL snippets, not in Rust (Principle 3);
these primitives are just the proto3 ↔ HashLit bridge.

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

## cef.* — ArcSight Common Event Format

### cef.parse(text[, defaults])

Parses CEF. The header is located by searching for `CEF:` anywhere in the input, so an optional syslog wrapper is tolerated.

```
cef.parse(ingress)                 // bare: merge cef_* and extension keys into workspace
```

Header keys: `cef_version`, `cef_device_vendor`, `cef_device_product`, `cef_device_version`, `cef_signature_id`, `cef_name`, `cef_severity`. Extension `key=value` pairs (e.g. `src=10.0.0.1 dst=192.168.1.1 act=block`) are emitted under the keys named by the CEF spec — they are not prefixed because CEF defines those names.

The optional `defaults` argument behaves the same as in `syslog.parse`.

## String functions

### contains(haystack, needle)

Returns `true` if `haystack` contains `needle`.

```
if contains(ingress, "CEF:") {
    cef.parse(ingress)
}
```

### lower(str) / upper(str)

Returns the string in lowercase or uppercase.

```
workspace.syslog_hostname = lower(workspace.syslog_hostname)
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

// Dotted names build nested objects.
regex_parse(ingress, "(?P<date.month>\\w{3}) (?P<date.day>\\d+)")
// → workspace.date.month, workspace.date.day

// Or capture explicitly.
workspace.parts = regex_parse(egress, "(?P<head>\\S+)\\s+(?P<tail>.*)")
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

### format(template)

Expands `%{...}` placeholders against the current event. Kept for backward compatibility and for callers who want an event-wide template in one argument; new code should prefer the [`${expr}` interpolation](./templates.md) that any string literal supports.

```
egress = format("%{workspace.syslog_hostname} %{workspace.syslog_appname}[%{workspace.syslog_procid}]: %{workspace.syslog_msg}")
```

Available placeholders:

| Placeholder | Source |
|-------------|--------|
| `%{source}`, `%{received_at}` | Event metadata |
| `%{egress}`, `%{ingress}` | Event byte buffers |
| `%{workspace.xxx}` | Named workspace value (nested: `%{workspace.geo.country}`) |

> **Note:** Workspace values must be referenced with the explicit `%{workspace.xxx}` form. A bare `%{xxx}` that isn't one of the event-level names above is an error — this avoids typos silently rendering as empty strings, and keeps the `%{}` resolution rules independent of any in-scope [`let`](./user-defined.md) bindings.

## Format parsers

JSON and KV are *formats*, not schemas — they describe how bytes are arranged, not what fields mean. They live in the flat namespace.

### parse_json(text[, defaults])

Parses `text` as JSON and returns the top-level object. Non-object JSON (arrays, scalars) is wrapped under the `_json` key so the return is always an object.

```
parse_json(egress)                       // bare: merge top-level keys into workspace
workspace.body = parse_json(egress)      // or capture under one workspace key
```

### parse_kv(text)

Parses `key=value` pairs (handling quoted values). Tokens without `=` are skipped.

```
parse_kv(egress)
// egress = `date=2026-04-15 srcip=10.0.0.1 action=deny msg="login failed"`
// → workspace.date, workspace.srcip, workspace.action, workspace.msg
```

Useful for FortiGate, Palo Alto, and similar firewall log formats.

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

## Timestamp formatting

### strftime(value, format[, timezone])

Formats an RFC 3339 timestamp (such as the event's `timestamp` field) according to a [`chrono` strftime](https://docs.rs/chrono/latest/chrono/format/strftime/) format string.

```
strftime(received_at, "%Y-%m-%d")          // 2026-04-19
strftime(received_at, "%b %e %H:%M:%S")    // Apr 19 10:30:45
strftime(received_at, "%Y-%m-%d", "local") // convert to local time first
strftime(received_at, "%H:%M", "UTC")      // force UTC
strftime(received_at, "%H:%M", "+09:00")   // fixed offset
```

| Argument | Description |
|----------|-------------|
| `value` | RFC 3339 string (e.g. `"2026-04-19T10:30:45+00:00"`). `received_at` always parses. |
| `format` | `chrono` strftime format. |
| `timezone` *(optional)* | `"local"`, `"UTC"` (case-insensitive), or `±HH:MM` / `±HHMM`. If omitted, `value`'s own offset is used. |

Both an invalid RFC 3339 input and an invalid timezone specifier are errors — `strftime` never silently returns an empty string.

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
workspace.src_endpoint.port = to_int(workspace.spt)  // CEF ext: "54321" → 54321
```

| Input | Result |
|-------|--------|
| `Int` | Pass-through |
| `Float` | Truncated toward zero |
| `String` | `str::parse::<i64>` after trimming whitespace; otherwise `null` |
| `Bool` | `1` or `0` |
| `Null` | `Null` |
| Array / Object | `Null` |

Motivation: CEF extension values and CSV column values arrive as strings even when carrying numeric content. OCSF schemas commonly require `Integer` for those same fields (ports, session IDs, byte counts). `to_int` is the schema-agnostic cast used by SIEM parser snippets.

## Array helpers

Arrays in limpid are **positionless collections** — you construct them with `[a, b, c]` literals, but the DSL deliberately omits positional access (`arr[n]`) and positional writes (`arr[n] = v`). Element identity, not position, is the addressing model; see [User-defined Processes → Arrays](./user-defined.md#arrays) for the rationale and `find_by` / `foreach` / `append` / `prepend` / `len` below.

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
workspace.observables = append(workspace.observables, new_obs)
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
workspace.n_observables = len(workspace.observables)
workspace.msg_len = len(workspace.syslog_msg)
```

Returning `null` on scalars (rather than `0` or an error) keeps the "not applicable" signal distinguishable from a legitimately empty collection.

## Serialization

### to_json() / to_json(value)

Without arguments, serializes the entire event as JSON. With one argument, serializes that value.

```
// Serialize entire event
egress = to_json()

// Serialize a single value
workspace.geo_json = to_json(geoip(workspace.src))
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
egress = "[" + workspace.syslog_hostname + "] " + egress
egress = source + " " + egress
```

If both operands are numeric, `+` is ordinary addition. Mixing with `null`, arrays, or objects is an error — stringify explicitly with `to_json()` first if that is what you want.

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
| `%{source}`, `%{timestamp}` | Event metadata |
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

## Timestamp formatting

### strftime(value, format[, timezone])

Formats an RFC 3339 timestamp (such as the event's `timestamp` field) according to a [`chrono` strftime](https://docs.rs/chrono/latest/chrono/format/strftime/) format string.

```
strftime(timestamp, "%Y-%m-%d")          // 2026-04-19
strftime(timestamp, "%b %e %H:%M:%S")    // Apr 19 10:30:45
strftime(timestamp, "%Y-%m-%d", "local") // convert to local time first
strftime(timestamp, "%H:%M", "UTC")      // force UTC
strftime(timestamp, "%H:%M", "+09:00")   // fixed offset
```

| Argument | Description |
|----------|-------------|
| `value` | RFC 3339 string (e.g. `"2026-04-19T10:30:45+00:00"`). `timestamp` always parses. |
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

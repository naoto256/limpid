# Expression Functions

Expression functions return values and can be used in conditions, assignments, and inline process blocks.

## String functions

### contains(haystack, needle)

Returns `true` if `haystack` contains `needle`.

```
if contains(ingress, "CEF:") {
    process parse_cef
}
```

### lower(str) / upper(str)

Returns the string in lowercase or uppercase.

```
workspace.hostname = lower(workspace.hostname)
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
workspace.clean = regex_replace(workspace.msg, "\\d+", "N")
```

> **Note:** `regex_replace` also exists as a [process module](./builtin-processes.md#regex_replace) that operates directly on `egress`. The function version works on any string value.

### format(template)

Expands `%{...}` placeholders against the current event. Kept for backward compatibility and for callers who want an event-wide template in one argument; new code should prefer the [`${expr}` interpolation](./templates.md) that any string literal supports.

```
egress = format("%{workspace.hostname} %{workspace.appname}[%{workspace.procid}]: %{workspace.syslog_msg}")
```

Available placeholders:

| Placeholder | Source |
|-------------|--------|
| `%{source}`, `%{facility}`, `%{severity}`, `%{timestamp}` | Event metadata |
| `%{egress}`, `%{ingress}` | Event byte buffers |
| `%{workspace.xxx}` | Named workspace value (nested: `%{workspace.geo.country}`) |

> **Note:** Workspace values must be referenced with the explicit `%{workspace.xxx}` form. A bare `%{xxx}` that isn't one of the event-level names above is an error — this avoids typos silently rendering as empty strings, and keeps the `%{}` resolution rules independent of any in-scope [`let`](./user-defined.md) bindings.

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
egress = "[" + severity + "] " + egress
egress = source + " " + egress
```

If both operands are numeric, `+` is ordinary addition. Mixing with `null`, arrays, or objects is an error — stringify explicitly with `to_json()` first if that is what you want.

# Expression Functions

Expression functions return values and can be used in conditions, assignments, and inline process blocks.

## String functions

### contains(haystack, needle)

Returns `true` if `haystack` contains `needle`.

```
if contains(raw, "CEF:") {
    process parse_cef
}
```

### lower(str) / upper(str)

Returns the string in lowercase or uppercase.

```
fields.hostname = lower(fields.hostname)
```

### regex_match(str, pattern)

Returns `true` if `str` matches the regex pattern.

```
if regex_match(message, "^\\d{4}-\\d{2}-\\d{2}") {
    fields.has_date = true
}
```

### regex_extract(str, pattern)

Returns the first capture group (or full match if no groups). Returns `null` if no match.

```
fields.ip = regex_extract(message, "(\\d+\\.\\d+\\.\\d+\\.\\d+)")
```

### regex_replace(str, pattern, replacement)

Returns the string with all matches replaced. Supports capture group references (`$1`, `$2`).

```
fields.clean = regex_replace(fields.msg, "\\d+", "N")
```

> **Note:** `regex_replace` also exists as a [process module](./builtin-processes.md#regex_replace) that operates directly on the message. The function version works on any string value.

### format(template)

Expands `%{...}` placeholders against the current event. Kept for backward compatibility and for callers who want an event-wide template in one argument; new code should prefer the [`${expr}` interpolation](./templates.md) that any string literal supports.

```
message = format("%{hostname} %{appname}[%{procid}]: %{syslog_msg}")
```

Available placeholders:

| Placeholder | Source |
|-------------|--------|
| `%{source}`, `%{facility}`, `%{severity}`, `%{timestamp}` | Event metadata |
| `%{message}`, `%{raw}` | Event body |
| `%{fields.xxx}` | Named field (nested: `%{fields.geo.country}`) |
| `%{xxx}` | Shorthand for `%{fields.xxx}` |

> **Note:** The shorthand `%{xxx}` checks fields first but is shadowed by reserved names (`source`, `facility`, `severity`, `timestamp`, `message`, `raw`). Use `%{fields.xxx}` to avoid ambiguity.

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
fields.fingerprint = md5(message)
fields.hash = sha256(message)

// Anonymize source IP
fields.src_hash = sha256(fields.src)
```

Useful for event deduplication, fingerprinting, or anonymisation.

## Serialization

### to_json() / to_json(value)

Without arguments, serializes the entire event as JSON. With one argument, serializes that value.

```
// Serialize entire event
message = to_json()

// Serialize a single value
fields.geo_json = to_json(geoip(fields.src))
```

## Table functions

In-memory key-value tables with optional TTL and max entry limits. Tables are defined in the `table` global block and accessed via `table_lookup`, `table_upsert`, and `table_delete`.

### table_lookup(table, key)

Returns the value for a key, or `null` if not found or expired.

```
fields.asset_name = table_lookup("asset", fields.src)
```

### table_upsert(table, key, value, expire?)

Inserts or updates a key. `expire` is TTL in seconds (0 = no expiry, omitted = table default).

Can be used as an expression statement (no assignment needed):

```
table_upsert("seen", fields._hash, "1", 300)
```

### table_delete(table, key)

Removes a key from the table.

```
table_delete("sessions", fields.session_id)
```

### Use cases

**Asset enrichment** — look up metadata from a static table loaded at startup:

```
def process enrich {
    fields.asset = table_lookup("assets", source)
    fields.owner = table_lookup("owners", source)
}
```

**Event deduplication** — suppress repeated events from the same source within a time window:

```
def process dedup {
    fields._key = sha256(regex_extract(raw, "msg=(.+)"))
    if table_lookup("seen", fields._key) != null {
        drop
    }
    table_upsert("seen", fields._key, "1", 600)
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
    if contains(raw, "session opened") {
        fields._sid = regex_extract(raw, "session=(\\S+)")
        table_upsert("sessions", fields._sid, source, 3600)
    }
    if contains(raw, "session closed") {
        fields._sid = regex_extract(raw, "session=(\\S+)")
        table_delete("sessions", fields._sid)
    }
}
```

See [Configuration](../configuration.md#table) for table definition options.

### geoip(ip_str)

Returns a GeoIP lookup result as an object with `country`, `city`, `latitude`, and `longitude` fields.

```
fields.geo = geoip(fields.src)
// fields.geo.country = "JP"
// fields.geo.city = "Tokyo"
```

Requires the `geoip` global block. See [Configuration](../configuration.md#geoip).

Access nested fields with postfix property access:

```
fields.country = geoip(fields.src).country
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
message = "[" + severity + "] " + message
message = source + " " + message
```

If both operands are numeric, `+` is ordinary addition. Mixing with `null`, arrays, or objects is an error — stringify explicitly with `to_json()` first if that is what you want.

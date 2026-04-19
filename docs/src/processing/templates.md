# String Templates

Any string literal in a `.limpid` file can contain `${...}` interpolations. Each `${expr}` is an ordinary DSL expression: it is parsed when the config loads and evaluated per event when the string is used.

```
def output archive {
    type file
    path "/var/log/limpid/${source}/${strftime(timestamp, "%Y-%m-%d", "local")}.log"
}

def process tag {
    message = "[${severity}] ${fields.hostname}: ${message}"
}
```

## Syntax

- `${expr}` — evaluate `expr` and splice the result (stringified) into the surrounding string.
- `\${` — a literal `${`. The backslash escape only takes effect inside interpolated strings; it is harmless elsewhere.
- `expr` can be anything that's valid in a DSL expression: identifiers, field paths (`fields.geo.country`), function calls (`lower(fields.host)`, `strftime(timestamp, "%Y")`), string concatenation with `+`, even nested string literals.

## Available names

Inside `${...}` you have access to the full event:

| Name | Meaning |
|------|---------|
| `source`, `facility`, `severity`, `timestamp` | Event metadata |
| `message`, `raw` | Event body |
| `fields.xxx`, `fields.xxx.yyy` | Named fields (nested lookup is supported) |

All [expression functions](./functions.md) — `strftime`, `lower`, `regex_extract`, `to_json`, `geoip`, and the parsers — are callable from inside `${...}`.

## Stringification

Evaluated values are coerced to strings:

| Value | String form |
|-------|-------------|
| String | as-is |
| Integer / Float | decimal representation |
| Bool | `true` / `false` |
| Null | empty string |
| Object / Array | JSON |

For full control over structured values, wrap them in `to_json(...)` yourself.

## Sanitisation in file paths

The `file` output's `path` property applies one extra rule on top of normal evaluation: interpolations that dereference `fields.*` directly (e.g. `${fields.hostname}`) have `/`, `\`, and `..` replaced with `_`. This prevents event-supplied field values from escaping the configured directory.

```
def output per_host {
    type file
    // fields.hostname is sanitised; ${source} and ${strftime(...)} are not
    path "/var/log/limpid/${fields.hostname}/${strftime(timestamp, "%Y-%m-%d")}.log"
}
```

Expressions that *compute* a value from fields (e.g. `${lower(fields.hostname)}`) are **not** auto-sanitised — the rule is deliberately conservative. If you transform a field and still want the guardrail, apply it explicitly (for example with `regex_replace`).

## Relationship to `format()`

`format("%{...}")` is still a function and still works. The differences:

| | `${expr}` (template literal) | `format("%{name}")` |
|---|------------------------------|----------------------|
| Parsed as | DSL expression (AST) | Runtime string scan |
| Works in | Any string literal anywhere | Anywhere you can call a function |
| Inside `${}` | Full DSL expression | Limited placeholders |
| Escape | `\${` | none needed (no special `${`) |

Prefer the template literal form for new code; `format()` remains for existing configs and for the rare case where the template itself comes from an expression (e.g. looked up from a table).

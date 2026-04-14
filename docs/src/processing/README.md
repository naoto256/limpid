# Processing

Processing in limpid has two layers:

- **Process modules** — transform the entire event (parse, filter, rewrite). Called via `process <name>` in pipelines.
- **Expression functions** — return values for use in conditions and assignments. Called via `name(args)` in expressions.

## Process vs Function

| | Process | Function |
|---|---------|----------|
| Syntax | `process parse_cef` | `geoip(fields.src)` |
| Operates on | Entire event (message, fields, metadata) | Returns a value |
| Can drop events | Yes (`drop` inside process) | No |
| Where used | Pipeline statements, process chains | Conditions, assignments, any expression |

Some names exist in both:

- `regex_replace("pat", "repl")` as a **process** — replaces in message
- `regex_replace(str, "pat", "repl")` as a **function** — returns replaced string

## Process chains

In a pipeline, processes can be chained with `|`:

```
process strip_pri | parse_cef | {
    fields.geo = geoip(fields.src)
}
```

This runs `strip_pri`, then `parse_cef`, then an inline process block — in sequence.

If any process in the chain drops the event, the remaining processes are skipped.

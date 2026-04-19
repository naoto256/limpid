# Upgrading to 0.3

This release replaces most built-in processes with DSL functions, adds first-class string interpolation, and introduces `limpidctl check`. Configuration written for 0.2 will not load unchanged — all of the breaking items below are simple mechanical rewrites.

## Breaking changes

### `file` output: no more hardcoded path placeholders

Previous releases recognised fixed tokens (`${date}`, `${year}`, `${month}`, `${day}`) inside `path`. These are gone — `${...}` now runs the full DSL evaluator, so you use `strftime` explicitly:

| Old | New |
|-----|-----|
| `path "/log/${date}.log"` | `path "/log/${strftime(timestamp, "%Y-%m-%d", "local")}.log"` |
| `path "/log/${year}/${month}/${day}.log"` | `path "/log/${strftime(timestamp, "%Y/%m/%d", "local")}.log"` |

`${source}`, `${severity}`, `${fields.xxx}` continue to work because they are valid DSL expressions. `fields.*` interpolations are still auto-sanitised.

## New features

- **String interpolation** (`${expr}`) in any string literal, with the full DSL available inside. See [String Templates](../processing/templates.md).
- **`strftime(value, format[, tz])`** for timestamp formatting. See [Timestamp formatting](../processing/functions.md#timestamp-formatting).
- **`+` concatenates strings** when either operand is a string; stays as numeric addition otherwise.

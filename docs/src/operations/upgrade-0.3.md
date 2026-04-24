# Upgrading to 0.3

This release replaces most built-in processes with DSL functions, adds first-class string interpolation, and introduces `limpidctl check`. Configuration written for 0.2 will not load unchanged — all of the breaking items below are simple mechanical rewrites.

## Breaking changes

### `file` output: no more hardcoded path placeholders

Previous releases recognised fixed tokens (`${date}`, `${year}`, `${month}`, `${day}`) inside `path`. These are gone — `${...}` now runs the full DSL evaluator, so you use `strftime` explicitly:

| Old | New |
|-----|-----|
| `path "/log/${date}.log"` | `path "/log/${strftime(timestamp, "%Y-%m-%d", "local")}.log"` |
| `path "/log/${year}/${month}/${day}.log"` | `path "/log/${strftime(timestamp, "%Y/%m/%d", "local")}.log"` |

`${source}`, `${severity}`, `${workspace.xxx}` continue to work because they are valid DSL expressions. `workspace.*` interpolations are still auto-sanitised.

### Event model: `raw`/`message`/`fields` renamed to `ingress`/`egress`/`workspace`

The three Event-level names have been renamed to match the hop-contract naming used elsewhere in limpid:

| Old | New | Meaning |
|-----|-----|---------|
| `raw` | `ingress` | Bytes as received from the input (immutable by convention) |
| `message` | `egress` | Bytes the output will write to the wire |
| `fields` | `workspace` | Pipeline-local scratch namespace for parsed/enriched values |

The rationale: `ingress`/`egress` is the industry-standard directional pair and is free of the overload between DSL `message` and syslog `MSG` that recurred in configs touching syslog payloads. `workspace` reflects that the namespace is a mutable working area owned by the in-flight event, not a persistent field set. These are the last user-visible name changes to the Event model before 1.0.

All three names are now unrecognised identifiers — the parser will reject them via the existing `unknown identifier` path. There is no automatic alias and no deprecation warning.

**Migrating `*.limpid` configs.** The renames are purely mechanical; a single `sed` pass over your config tree covers the vast majority of call sites:

```bash
# Run from your config directory (e.g. /etc/limpid/). Review with `git diff` or a dry-run grep first.
sed -i \
  -e 's/\bfields\./workspace./g' \
  -e 's/\bevent\.raw\b/event.ingress/g' \
  -e 's/\bevent\.message\b/event.egress/g' \
  -e 's/%{fields\./%{workspace./g' \
  -e 's/%{raw}/%{ingress}/g' \
  -e 's/%{message}/%{egress}/g' \
  -e 's/\${fields\./${workspace./g' \
  -e 's/\${raw}/${ingress}/g' \
  -e 's/\${message}/${egress}/g' \
  *.limpid
```

Bare `raw` and `message` identifiers (as in `contains(raw, ...)` or `message = format(...)`) also need to change. The safe pattern is to rewrite them by hand — a blind `sed s/\braw\b/ingress/g` will touch comments, string literals, and variable names you did not mean to change. After running the `sed` above, grep for the remaining bare occurrences:

```bash
grep -nE '\b(raw|message)\b' *.limpid
```

and rewrite each DSL-concept hit (e.g. `contains(raw, ...)` → `contains(ingress, ...)`, `message = ...` → `egress = ...`). Leave comments and string-literal text alone unless they are actually describing the DSL.

**Migrating captured `tap --json` files.** `tap --json` emits the Event as JSON with the new key names. Captures made with limpid 0.2 use the old keys (`raw`, `message`, `fields`) and will not round-trip through `inject --json` on 0.3 as-is. Convert in place:

```bash
jq -c '{
  timestamp, source, facility, severity,
  ingress:   .raw,
  egress:    .message,
  workspace: .fields
}' old-capture.jsonl > new-capture.jsonl
```

Fresh captures on 0.3 emit the new keys directly — no conversion needed going forward.

## New features

- **String interpolation** (`${expr}`) in any string literal, with the full DSL available inside. See [String Templates](../processing/templates.md).
- **`strftime(value, format[, tz])`** for timestamp formatting. See [Timestamp formatting](../processing/functions.md#timestamp-formatting).
- **`+` concatenates strings** when either operand is a string; stays as numeric addition otherwise.

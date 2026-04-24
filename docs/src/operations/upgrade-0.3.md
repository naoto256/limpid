# Upgrading to 0.3

This release replaces most built-in processes with DSL functions, adds first-class string interpolation, and introduces `limpid --check`. Configuration written for 0.2 will not load unchanged — all of the breaking items below are simple mechanical rewrites.

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

**Migrating captured `tap --json` files.** `tap --json` emits the Event as JSON with the new key names, and 0.3 also drops the top-level `facility` / `severity` fields (see below). Captures made with limpid 0.2 use the old keys (`raw`, `message`, `fields`) and will not round-trip through `inject --json` on 0.3 as-is. Convert in place:

```bash
jq -c '{
  timestamp, source,
  ingress:   .raw,
  egress:    .message,
  workspace: .fields
}' old-capture.jsonl > new-capture.jsonl
```

Fresh captures on 0.3 emit the new keys directly — no conversion needed going forward.

### Schema-specific functions moved to dot namespaces

Schema-specific helpers that used to be flat-named primitives now live under their schema's dot namespace, per [Design Principle 5](../design-principles.md#principle-5--schema-identity-is-declared-by-namespace). The rename is mechanical:

| Old | New |
|-----|-----|
| `parse_syslog(x)` | `syslog.parse(x)` |
| `parse_cef(x)` | `cef.parse(x)` |
| `strip_pri(x)` | `syslog.strip_pri(x)` |

Two new namespaced helpers replace the old `facility = N` / `severity = N` magic assignments (see the next section):

| New | Purpose |
|-----|---------|
| `syslog.set_pri(text, facility, severity)` | Write or rewrite the leading `<PRI>` header. |
| `syslog.extract_pri(text)` | Return the leading `<PRI>` value as a number, or `null`. |

Schema-agnostic primitives — `parse_json`, `parse_kv`, `regex_*`, `strftime`, `md5` / `sha1` / `sha256`, `to_json`, `contains`, `lower`, `upper`, `format`, `table_*`, `geoip` — keep their flat names. JSON / KV are *formats*, not schemas.

### Workspace key naming convention

Schema parsers now emit fields under a `<schema>_` prefix so a workspace dump stays self-describing when several schemas have populated the same event:

| Function | Old workspace keys (0.2) | New workspace keys (0.3) |
|----------|---------------------------|---------------------------|
| `syslog.parse` | `hostname`, `appname`, `procid`, `msgid`, `syslog_msg` | `syslog_hostname`, `syslog_appname`, `syslog_procid`, `syslog_msgid`, `syslog_msg` |
| `cef.parse` (header) | `device_vendor`, `device_product`, `device_version`, `signature_id`, `name`, `severity`, `version` | `cef_device_vendor`, `cef_device_product`, `cef_device_version`, `cef_signature_id`, `cef_name`, `cef_severity`, `cef_version` |
| `cef.parse` (extensions) | `src`, `dst`, `act`, … | unchanged (CEF defines those names) |

Update `format(...)` templates, `${...}` interpolations, and any `if workspace.X ...` conditions accordingly.

### Built-in process layer removed

The native `process <name>` modules are gone. Every transformation now happens in DSL — either inside a named `def process` body or an inline `process { ... }` block.

| Old (0.2 process) | New (0.3 DSL) |
|-------------------|---------------|
| `process parse_syslog` | `process { syslog.parse(ingress) }` |
| `process parse_cef` | `process { cef.parse(ingress) }` |
| `process parse_json` | `process { parse_json(egress) }` |
| `process parse_kv` | `process { parse_kv(egress) }` |
| `process strip_pri` | `process { egress = syslog.strip_pri(egress) }` |
| `process regex_replace("pat", "repl")` | `process { egress = regex_replace(egress, "pat", "repl") }` |
| `process prepend_source` | `process { egress = source + " " + egress }` |
| `process prepend_timestamp` | `process { egress = strftime(timestamp, "%b %e %H:%M:%S") + " " + egress }` |

Bare-statement function calls (`syslog.parse(ingress)`, `parse_kv(egress)`, `cef.parse(ingress)`) merge their returned object into `workspace` automatically — that is the same semantic the old parser processes had, now spelled in pure DSL. See [Expression Functions: Bare statements vs assignments](../processing/functions.md#bare-statements-vs-assignments) for the rule.

### Event core: `facility` / `severity` removed

The Event struct no longer carries facility / severity. They are bytes inside the `<PRI>` header, and pipelines that need their numeric value extract them from `egress` (or `ingress`) on demand.

- `event.facility` / `event.severity` (or bare `facility` / `severity`) → **`unknown identifier` error.**
- `facility = N` / `severity = N` assignments → **`unknown assignment target` error.**
- `tap --json` no longer emits top-level `facility` / `severity` keys; the `--input` JSON for `--test-pipeline` no longer accepts them either.
- Routing on severity now reads the byte explicitly:

  ```
  let pri = syslog.extract_pri(ingress)
  if pri != null and pri % 8 <= 3 {
      output alert
  }
  ```

- Rewriting the PRI is now an explicit byte operation:

  ```
  egress = syslog.set_pri(egress, 16, 6)   // local0.info
  ```

  This replaces the old side-effecting `facility = 16  severity = 6` form.

This change makes `egress` the single hop contract — there is no parallel sidecar of "metadata that also travels". See [Design Principle 4](../design-principles.md#principle-4--only-egress-crosses-hop-boundaries).

## New features

- **String interpolation** (`${expr}`) in any string literal, with the full DSL available inside. See [String Templates](../processing/templates.md).
- **`strftime(value, format[, tz])`** for timestamp formatting. See [Timestamp formatting](../processing/functions.md#timestamp-formatting).
- **`+` concatenates strings** when either operand is a string; stays as numeric addition otherwise.
- **`let name = expr`** for process-local scratch bindings, scoped to the process body. See [User-defined Processes](../processing/user-defined.md#assignments).
- **Dot-namespace function calls** (`syslog.parse`, `cef.parse`, `syslog.set_pri`, `syslog.extract_pri`, `syslog.strip_pri`).

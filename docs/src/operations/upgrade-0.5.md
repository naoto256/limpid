# Upgrading to 0.5

> **Coming from 0.2 or earlier?** The DSL was rewritten substantially
> between 0.2 and 0.3 — native process modules (`process parse_syslog`,
> `process parse_cef`, `process strip_pri`, …) were removed in favour
> of DSL function calls inside a `process` body, and several other
> shapes shifted. There is no per-version upgrade recipe; the simplest
> path is to read the [Tutorial](../tutorial.md) and rewrite. Apologies
> for the breakage — this is what pre-1.0 means.

This page lists everything that breaks when moving a 0.4.x (or earlier)
config to 0.5.0. It is the upgrade-time reference, not a release-notes
overview — for the additive surface (OTLP transport, new DSL primitives,
array literals, `Value::Bytes`, …) see the [CHANGELOG](../../CHANGELOG.md).

Most entries below come with a one-line `sed` or `grep` recipe.

## Breaking changes

### Typed timestamps (`Value::Timestamp`)

The DSL now has a first-class `Value::Timestamp` arm. `received_at`,
the new `timestamp()` primitive, and `strptime` all return a
`Value::Timestamp`; `strftime` accepts one as its first argument.
Type-correct configs from 0.4 keep working byte-for-byte:

```limpid
egress = "${strftime(received_at, \"%Y-%m-%d\", \"local\")} ${egress}"
```

What changes:

- **`now()` is gone** — rename call sites to `timestamp()`. The new
  name reads consistently with `received_at` and matches the value
  type it returns.
- **String operations on `received_at` no longer compile**. Code like
  `contains(received_at, "2026")` or `regex_match(received_at, ...)`
  errors at the analyzer (`expected string, got timestamp`). These
  were always meaningless on a timestamp; the analyzer just used to
  miss them. To inspect the wire form of a timestamp, format it
  explicitly: `contains(strftime(received_at, "%Y", "UTC"), "2026")`.
- **`to_int(received_at)`** returns unix nanoseconds (`i64`) — matches
  OTLP `time_unix_nano`. So `workspace.observed_unix_nano =
  to_int(received_at)` is the natural way to get an epoch number.
- **`tap --json`** emits `received_at` as `i64` unix nanoseconds
  rather than an RFC3339 string. See *`tap --json` `received_at` is
  now unix nanoseconds* below for capture-file migration.

### `Event.timestamp` renamed to `Event.received_at`

The Event struct field, the reserved DSL identifier, the `format()`
template placeholder, and the `tap --json` serialisation key are all
renamed:

| Old | New |
|-----|-----|
| `Event.timestamp` (Rust struct) | `Event.received_at` |
| `${timestamp}` (string template) | `${received_at}` |
| `%{timestamp}` (legacy template) | `%{received_at}` |
| `timestamp` (bare DSL identifier) | `received_at` |
| `tap --json` top-level key `timestamp` | `received_at` |

**Why.** The old name was generic enough to be misread as "the event's
timestamp" in the source-claimed sense, when limpid has always
populated it with the wall-clock moment the *current hop* received the
bytes. The two are not the same thing — a syslog line that crossed a
WAN reaches a forwarder seconds after the originating device generated
it, and the OTLP world distinguishes `time_unix_nano` (source-claimed)
from `observed_time_unix_nano` (receiver-observed) for exactly this
reason. The rename makes the limpid field's meaning unambiguous: this
is the local hop's observation time, full stop.

The source-claimed time, when extractable from the wire, lives in
workspace fields populated by parser snippets — typically captured
under a per-schema namespace, e.g. `workspace.syslog.timestamp` after
`workspace.syslog = syslog.parse(ingress)`.

There is **no deprecation alias** — `${timestamp}` and bare
`timestamp` are hard errors on 0.5. Pre-1.0 breaking changes are
expected; this is the right window.

**Migrating `*.limpid` configs.** A single `sed` covers most call
sites:

```bash
# Run from your config directory (e.g. /etc/limpid/). Review with
# `git diff` or a dry-run grep first.
find . -name '*.limpid' -exec sed -i \
    -e 's/\${timestamp}/\${received_at}/g' \
    -e 's/%{timestamp}/%{received_at}/g' \
    -e 's/strftime(timestamp,/strftime(received_at,/g' \
    {} +
```

Bare `timestamp` references inside DSL bodies (`if timestamp > ...`,
`workspace.captured = timestamp`, etc.) need a careful pass — a blind
`s/\btimestamp\b/received_at/g` will rewrite comments and string
literals you did not mean to touch. After the `sed` above, grep for
remaining bare hits:

```bash
grep -nE '\btimestamp\b' *.limpid
```

and rewrite each DSL-concept hit by hand. Leave comments and
string-literal text alone unless they describe the DSL.

**Migrating captured `tap --json` files.** `tap --json` on 0.5 emits
`received_at` directly. Captures made on 0.4 use the old `timestamp`
key and will not round-trip through `inject --json` on 0.5 as-is.
Convert in place:

```bash
jq -c '.received_at = .timestamp | del(.timestamp)' \
    old-capture.jsonl > new-capture.jsonl
```

Fresh captures on 0.5 emit the new key directly — no conversion
needed going forward.

### `tap --json` `received_at` is now unix nanoseconds

Captured `*.jsonl` files from 0.4 hold `received_at` as RFC3339
strings; 0.5 emits them as integers (unix nanoseconds, OTLP-shape).
`inject --json` only accepts the new wire form. If you need to
replay a 0.4 capture, convert it first:

```bash
# Lossy conversion (drops sub-second precision); fine for most
# replay use cases. For exact precision, write a small Python /
# Rust script.
jq -c '.received_at = ((.received_at | sub("\\.\\d+"; "")
                                 | strptime("%Y-%m-%dT%H:%M:%S%z")
                                 | mktime) * 1000000000)' \
    old-capture.jsonl > new-capture.jsonl
```

The new wire form aligns with OTLP's `time_unix_nano` and removes
timezone string ambiguity from the on-the-wire interchange.

### `syslog.parse` returns more, and PRI parsing is strict

Two changes ship together.

**Returned object grew.** Beyond the structural fields, `syslog.parse`
now emits `pri` (Int 0–191), `facility` (Int 0–23), `severity` (Int
0–7), and `timestamp` (the source-claimed wire timestamp from the
header — previously dropped silently). Combined with the un-prefixed
keys (see *Schema parsers no longer prefix workspace keys* below), the
recommended pattern is:

```limpid
workspace.syslog = syslog.parse(ingress)
// → workspace.syslog.pri / .facility / .severity / .timestamp
//   .hostname / .appname / .procid / .msgid / .msg
```

The lighter `syslog.extract_pri` is unchanged and remains useful for
callers that want the PRI byte without tokenising the rest of the
header.

**PRI parsing is now strict.** `syslog.parse` validates the leading
`<PRI>` exactly as RFC 5424 §6.2.1 specifies: 1–3 ASCII digits, value
0–191, framed by `<` and `>` at the start of the input. Inputs the
previous lax parser tolerated silently — `<malformed text>...`
(non-digit content), `<999>...` (out-of-range), `<>...` (empty PRI) —
now error with `syslog.parse(): no PRI header`. Sibling primitives
(`syslog.strip_pri` / `syslog.set_pri` / `syslog.extract_pri`) already
used the strict scanner; this aligns the family.

If you have a flow that depended on the old lax behaviour to ingest
non-syslog payloads via `syslog.parse`, switch to a different parser
(`parse_kv`, `regex_parse`, or a vendor-specific snippet) — calling
`syslog.parse` on something that isn't syslog has no defined output
anyway.

### `cef.parse` is strict: requires `CEF:` at position 0

Previously `cef.parse` searched for `CEF:` anywhere in the input,
silently skipping any leading `<PRI>` syslog wrapper. The parser now
requires the input to start with `CEF:` and reports `cef.parse():
input does not start with \`CEF:\`` otherwise. Syslog-wrapped CEF
should be handled by composing the two parsers:

```limpid
workspace.syslog = syslog.parse(ingress)
workspace.cef    = cef.parse(workspace.syslog.msg)
```

CEF that arrives on non-syslog transports (HTTP body, file tail, …)
is unaffected.

### Schema parsers no longer prefix workspace keys

`syslog.parse` and `cef.parse` previously emitted keys with a
`<schema>_` prefix (`syslog_hostname`, `cef_name`, …) on the rationale
that bare invocation kept workspace dumps self-describing. In practice
the prefix collided with the *capture* idiom — `workspace.s =
syslog.parse(ingress)` produced `workspace.s.syslog_hostname`,
double-prefixed — and made schema parsers behave inconsistently with
format primitives (`parse_json`, `parse_kv`) which always emit raw
keys.

Both schema parsers now return un-prefixed keys (`hostname`,
`appname`, `version`, `name`, `severity`, …). Namespacing is the
operator's job and is the recommended pattern:

```limpid
workspace.syslog = syslog.parse(ingress)   // workspace.syslog.hostname, ...
workspace.cef    = cef.parse(ingress)      // workspace.cef.version, workspace.cef.src, ...
```

Bare invocation still works (keys merge flat into `workspace`) but is
collision-prone and discouraged. CEF extension keys (`src`, `dst`,
`act`, …) were never prefixed — those names are part of the CEF spec
and continue verbatim.

**Migration**:

```bash
# 1. capture once at the top of each process body that calls a schema parser:
#      workspace.syslog = syslog.parse(ingress)
#      workspace.cef    = cef.parse(ingress)
# 2. rewrite the references:
sed -i 's/workspace\.syslog_/workspace.syslog./g; s/workspace\.cef_/workspace.cef./g' \
    /etc/limpid/**/*.limpid
```

### `to_json()` requires an argument

`to_json()` (no argument) used to serialise the entire `Event` — a hidden default that almost no one wanted. Pass an explicit value now; `to_json(workspace)` is the typical replacement when shipping enriched events downstream.

```limpid
// before
egress = to_json()

// after
egress = to_json(workspace)
```

For the rare case where you actually want the whole event, build it explicitly: `to_json({received_at: received_at, source: source, ingress: ingress, egress: egress, workspace: workspace})`.

### `output file` path templates are stricter

The `path` template renderer in the `file` output gained three guards
that reject configs the previous lax renderer accepted silently. Each
fires before any byte hits disk.

- **Per-interpolation slash strip.** Every `${...}` result has
  forward and back slashes replaced with `_`, so an interpolation
  cannot smuggle a path separator into the rendered path.
- **`..` rejected anywhere in the rendered path.** After all
  interpolations resolve, the path is split on `/` and any component
  exactly equal to `..` causes the write to error rather than being
  silently rewritten.
- **Empty interpolation rejected.** An interpolation that evaluates
  to the empty string errors instead of producing surprise paths
  (`/foo//bar`, `/foo/.log`).
- **Trailing-slash rejected.** A rendered path that ends in `/` (no
  filename component) errors rather than turning into a spurious
  `mkdir`.

Configs that depended on any of these silent rewrites should sanitise
the inputs upstream (`regex_replace`, explicit fallbacks in a
`process` block) and reference the cleaned workspace key from the
template. The full rationale and worked examples are in the
[`output file`](../outputs/file.md) reference.

### `format()` primitive removed

`format(template)` and the `%{...}` placeholder syntax are gone. The DSL has `${expr}` interpolation in any string literal, which is strictly more capable (any expression, not just a fixed set of placeholders) and parse-time checked. Rewrite call sites mechanically:

```limpid
// before
egress = format("[%{source}] %{workspace.cef_name}: %{egress}")

// after
egress = "[${source}] ${workspace.cef.name}: ${egress}"
```

A grep for `format(` and `%{` over your config tree should surface every site to migrate.

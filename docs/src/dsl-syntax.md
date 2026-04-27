# DSL Syntax Basics

Surface-level rules of the limpid DSL — keywords, literal forms, block structure. The actual *meaning* of definitions (what an input is, how a process runs, how a pipeline routes) is documented under each module's reference page; this page collects the syntactic conventions that apply across all `.limpid` files.

## Definitions

The `def` keyword introduces a top-level definition. The five kinds are:

```
def input <name> { ... }              // see Inputs
def output <name> { ... }             // see Outputs
def process <name> { ... }            // see Processing → User-defined Processes
def function <name>(<args>) { expr }  // see Processing → User-defined Functions
def pipeline <name> { ... }           // see Pipelines
```

A name is an identifier (`[A-Za-z_][A-Za-z0-9_]*`). Definitions can appear in any order and any file; cross-references between them are resolved at config-load time, not at parse time.

## Comments

```
// Line comment to end of line.
```

Block comments are not supported.

## Statement separators

Newlines separate statements. Semicolons are **optional** and only useful when you want multiple statements on one line:

```
def output fw01 {
    type file
    path "/var/log/fw/fw01.log"
}

// Equivalent one-liner — semicolons for readability:
def output fw01 { type file; path "/var/log/fw/fw01.log" }
```

## Literals

| Form | Examples |
|------|----------|
| String | `"hello"`, `"path with spaces"` |
| Integer | `42`, `-1`, `0` |
| Float | `3.14`, `-0.5` |
| Bool | `true`, `false` |
| Null | `null` |
| Array | `[a, b, c]`, `[]`, mixed types allowed |
| Object (hash literal) | `{ key: value, other: 42 }` |

Strings are double-quoted only — no single-quote form. Strings support `${expr}` interpolation, where `expr` is any DSL expression (see [String interpolation](#string-interpolation) below).

## Blocks

`{ ... }` introduces a nested block. Block contents depend on context:

| Block in | Contains |
|----------|----------|
| `def input` / `def output` | property assignments (`type syslog_tcp`, `bind "..."`, …) |
| `def process` | function calls, assignments to `egress` / `workspace` / `let`, `if` / `switch` / `drop` |
| `def pipeline` | `input` / `output` references, `process` invocations, `if` / `switch` / `drop` / `finish` |
| `geoip {}`, `control {}`, `table {}` | global block properties (see [Main Configuration](./configuration.md#global-blocks)) |

## Identifier paths

Dotted identifiers reach into nested objects:

```
workspace.host                  // workspace -> "host" key
workspace.geo.country           // nested
workspace.cef.src_endpoint.ip   // arbitrarily deep
```

The leading segment is one of the event-level names (`ingress`, `egress`, `received_at`, `source`, `error`, `workspace`) or a `let` binding in scope. Bare identifiers that match none of these are an error at analyzer time.

## Property assignment in process bodies

Inside a `def process { ... }` body, the `=` operator assigns to an identifier path. The left side must be a path under `egress`, `workspace`, or `let`:

```
def process tag {
    workspace.host_safe = lower(workspace.syslog.hostname)
    egress = "${workspace.host_safe}: ${workspace.syslog.msg}"
    let pri = syslog.extract_pri(ingress)
}
```

`let <name> = expr` introduces a process-local scratch binding (scalar only, not a path). See [User-defined Processes](./processing/user-defined.md) for the full statement set.

## String interpolation

Any string literal can contain `${...}` interpolations. Each `${expr}` is an ordinary DSL expression: parsed when the config loads, evaluated per event when the string is used.

```
def output archive {
    type file
    path "/var/log/limpid/${source}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def process tag {
    egress = "[${workspace.syslog.appname}] ${workspace.syslog.hostname}: ${egress}"
}
```

`${expr}` accepts any expression valid in the DSL: identifiers, workspace paths (`workspace.geo.country`), function calls (`lower(workspace.host)`, `strftime(received_at, "%Y")`), string concatenation with `+`, even nested string literals (`"${"${a}${b}"}"`). To embed a literal `${`, escape with `\${`.

Inside `${...}` you have access to the full event:

| Name | Meaning |
|------|---------|
| `source`, `received_at` | Event metadata |
| `egress`, `ingress` | Event byte buffers |
| `workspace.xxx`, `workspace.xxx.yyy` | Named workspace values (nested lookup is supported) |

All [built-in functions](./functions/expression-functions.md) — `strftime`, `lower`, `regex_extract`, `to_json`, `geoip`, and the parsers — are callable from inside `${...}`.

Evaluated values are coerced to strings:

| Value | String form |
|-------|-------------|
| String | as-is |
| Integer / Float | decimal representation |
| Bool | `true` / `false` |
| Null | empty string |
| Timestamp | RFC3339 (`2026-04-19T10:30:45+00:00`) |
| Object / Array | JSON |

For full control over structured values, wrap them in `to_json(...)` yourself.

Some outputs apply extra safety rules on top of the generic interpolation above. The notable case is the [`file` output's `path`](./outputs/file.md#sanitisation), which sanitises slashes per interpolation, rejects `..` traversal in the assembled path, and rejects empty / trailing-slash results.

## Control flow

The DSL has six control-flow constructs. The summary table maps each one to where it can appear:

| Construct | Form | Process body | Pipeline body |
|-----------|------|--------------|---------------|
| **if / else** | `if expr { ... } else if expr { ... } else { ... }` | yes | yes |
| **switch** | `switch expr { value1 { ... } value2 { ... } default { ... } }` | yes | yes |
| **foreach** | `foreach <array-path> { ... }` (current element exposed as `workspace._item`) | yes | — |
| **try / catch** | `try { ... } catch { ... }` (error message exposed as `error`) | yes | — |
| **drop** | `drop` | yes (concession — see note) | yes (terminates routing for this event) |
| **finish** | `finish` | — | yes (completes pipeline early without dropping) |

> **Note on `drop` inside a process body.** `drop` is fundamentally a routing decision (where the event goes — namely, nowhere) rather than a transformation, so in principle it belongs in a pipeline. The DSL allows it inside a process body anyway because in practice you sometimes recognise mid-transformation that the event isn't worth keeping (e.g., a parser snippet finds a malformed payload). Use it sparingly there; if a `drop` rule is reusable or its condition is independent of the surrounding transform, prefer expressing it at the pipeline level. See [Processing → process vs routing](./processing/README.md#process-vs-routing) for the full doctrine.

`if/else` and `switch` are the two constructs that work in both bodies, so the full treatment lives here. The other constructs are tied to one side and are documented on the page that owns them — pointers in *Where to use which* at the end of this section.

### if / else if / else

```
if expr { ... }
if expr { ... } else { ... }
if expr { ... } else if expr { ... } else { ... }
```

`expr` is any DSL expression. The branch runs when the value is *truthy*; everything else is falsy. Truthiness rules:

| Type | Truthy when | Falsy when |
|------|-------------|------------|
| `Bool` | `true` | `false` |
| `Int` / `Float` | non-zero | `0`, `0.0`, `NaN` |
| `String` | non-empty | `""` |
| `Bytes` | non-empty | length 0 |
| `Array` / `Object` | non-empty | empty `[]` / `{}` |
| `Null` | (never truthy) | always |
| `Timestamp` | always | (never falsy) |

Arms are statements valid in the surrounding body — pipeline statements at pipeline level (`output`, `process`, nested `if` / `switch`, `drop`, `finish`), process statements inside a `process` body (function calls, assignments, nested control flow). An empty arm (`if cond { }`) is allowed but rare.

```
// pipeline body
if workspace.cef.severity >= 8 {
    output alert
} else if workspace.cef.severity >= 5 {
    output siem
} else {
    output archive
}

// process body
if workspace.kv.action == "deny" {
    workspace.outcome = "blocked"
} else {
    workspace.outcome = "allowed"
}
```

`else if` is left-associative sugar for nested `if`/`else` and reads top-to-bottom; the first matching arm runs and the rest are skipped.

### switch

```
switch expr {
    value1 { ... }
    value2 { ... }
    default { ... }    // optional
}
```

The discriminator after `switch` is any DSL expression, evaluated once. Each arm's literal is matched against it with `==` semantics — types must agree (`switch workspace.severity { 5 { ... } }` matches `Int(5)` but not `String("5")`). The first matching arm runs. If none match, `default` runs; if `default` is absent, the `switch` is a no-op.

```
// pipeline body — route by source IP
switch source {
    "192.0.2.1" { output fw01 }
    "192.0.2.2" { output fw02 }
    default     { output archive }
}

// process body — dispatch parser by detected vendor
switch workspace.cef.device_vendor {
    "Fortinet"   { process parse_fortigate }
    "CheckPoint" { process parse_checkpoint }
    default      { process parse_generic }
}
```

Arm bodies are statements valid in the surrounding body, same rule as `if`. There is no fall-through and no need for an explicit `break`.

There is also an **expression form** of `switch` — each arm body is one expression rather than a statement list, and the matching arm's value is the value of the whole `switch`. Used inside `def function` bodies and anywhere a value is expected:

```
def function normalize_proto(num) {
    switch num {
        6  { "tcp" }
        17 { "udp" }
        1  { "icmp" }
        default { null }    // optional; absent → null on no match
    }
}
```

The expression form has no side effects (no `workspace.x = …`, no `process foo`, no routing keywords). The statement form is what process and pipeline bodies use; the expression form is what `def function` bodies and HashLit values use.

### Where to use which

The constructs not detailed above live on the page they semantically belong to:

- **`foreach` / `try-catch`** — process-body only, transformations over per-event data. See [User-defined Processes → Control flow](./processing/user-defined.md#control-flow) for the syntax and the per-context details (`workspace._item` binding, the `error` name inside `catch`).
- **`drop` / `finish`** — pipeline routing. `drop` terminates the event; `finish` ends the pipeline early without dropping. `drop` is also allowed inside a process body as a concession (the note above); `finish` is pipeline-only. See [Pipelines → Routing](./pipelines/routing.md) for the routing semantics (`events_dropped` vs `events_finished`, the deep-copy boundary at outputs).

## Reserved identifiers

The following names are reserved and cannot be used as user identifiers:

- Event metadata: `ingress`, `egress`, `received_at`, `source`, `error`, `workspace`
- Keywords: `def`, `input`, `output`, `process`, `pipeline`, `if`, `else`, `switch`, `default`, `drop`, `finish`, `let`, `include`
- Literal markers: `true`, `false`, `null`

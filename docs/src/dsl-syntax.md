# DSL Syntax Basics

Surface-level rules of the limpid DSL — keywords, literal forms, block structure. The actual *meaning* of definitions (what an input is, how a process runs, how a pipeline routes) is documented under each module's reference page; this page collects the syntactic conventions that apply across all `.limpid` files.

## Definitions

The `def` keyword introduces a top-level definition. The five kinds are:

```limpid
def input <name> { ... }              // see Inputs
def output <name> { ... }             // see Outputs
def process <name> { ... }            // see Processing → User-defined Processes
def function <name>(<args>) { expr }  // see Processing → User-defined Functions
def pipeline <name> { ... }           // see Pipelines
```

A name is an identifier (`[A-Za-z_][A-Za-z0-9_]*`). Definitions can appear in any order and any file; cross-references between them are resolved at config-load time, not at parse time.

## Comments

```limpid
// Line comment to end of line.
```

Block comments are not supported.

## Statement separators

Newlines separate statements. Semicolons are **optional** and only useful when you want multiple statements on one line:

```limpid
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
| `def process` | function calls, assignments to `egress` / `workspace` / `let`, `if` / `switch` / `drop` / `error` |
| `def pipeline` | `input` / `output` references, `process` invocations, `if` / `switch` / `drop` / `finish` / `error` |
| `geoip {}`, `control {}`, `table {}` | global block properties (see [Main Configuration](./configuration.md#global-blocks)) |

## Identifier paths

Dotted identifiers reach into nested objects:

```limpid
workspace.host                  // workspace -> "host" key
workspace.geo.country           // nested
workspace.cef.src_endpoint.ip   // arbitrarily deep
```

The leading segment is one of the event-level names (`ingress`, `egress`, `received_at`, `source`, `error`, `workspace`) or a `let` binding in scope. Bare identifiers that match none of these are an error at analyzer time.

## Property assignment in process bodies

Inside a `def process { ... }` body, the `=` operator assigns to an identifier path. The left side must be a path under `egress`, `workspace`, or `let`:

```limpid
def process tag {
    workspace.host_safe = lower(workspace.syslog.hostname)
    egress = "${workspace.host_safe}: ${workspace.syslog.msg}"
    let pri = syslog.extract_pri(ingress)
}
```

`let <name> = expr` introduces a process-local scratch binding. The
binding can hold any value type (scalar, Object, or Array): if the
expression returns an Object, dot-access reads through the binding
the same way `workspace.x.y` does — for example
`let f = regex_parse(...)` followed by `f.user`. The binding name
itself, however, is not a path target on the left of `=`: writes go
to `egress` or `workspace.*` (`let f.x = ...` is rejected). See
[User-defined Processes](./processing/user-defined.md) for the full
statement set.

## String interpolation

Any string literal can contain `${...}` interpolations. Each `${expr}` is an ordinary DSL expression: parsed when the config loads, evaluated per event when the string is used.

```limpid
def output archive {
    type file
    path "/var/log/limpid/${source.ip}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def process tag {
    egress = "[${workspace.syslog.appname}] ${workspace.syslog.hostname}: ${egress}"
}
```

`${expr}` accepts any expression valid in the DSL: identifiers, workspace paths (`workspace.geo.country`), function calls (`lower(workspace.host)`, `strftime(received_at, "%Y")`), string concatenation with `+`, even nested string literals (`"${"${a}${b}"}"`). To embed a literal `${`, escape with `\${`.

**Visibility differs by surface.** Inside a process body or pipeline expression (the example above), the full event is in scope. Inside an output config (`path` on `output file`, `key` on `output kafka`, etc.) the analyzer + daemon hard-reject `workspace`, `egress`, and `error` references — output config templates may only reference event-intrinsic fields (`source`, `received_at`, `ingress`). Routing decisions that depend on pipeline-mutable state belong in the pipeline body, not the output config; see [outputs/file → Dynamic path templates](./outputs/file.md#dynamic-path-templates) for the pipeline-body routing pattern.

Inside `${...}` the identifiers available are:

| Name | Type | Meaning | Available in output config? |
|------|------|---------|------------------------------|
| `received_at` | Timestamp | Wall-clock at which the event was received | Yes |
| `source` | Object `{ ip: String, port: Int }` | Peer address. Use `source.ip` for the IP string and `source.port` for the integer port; bare `source` returns the whole object | Yes |
| `ingress` | String / Bytes | Raw bytes as received from the input | Yes |
| `egress` | String / Bytes | Wire bytes assembled by the pipeline | **No** (pipeline-mutable; rejected by analyzer) |
| `error` | String | Error message inside a `catch` body (otherwise null) | **No** (pipeline-mutable; rejected by analyzer) |
| `workspace.xxx`, `workspace.xxx.yyy` | (varies) | Named workspace values (nested lookup is supported) | **No** (pipeline-mutable; rejected by analyzer) |

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
| **try / catch** | `try { ... } catch { ... }` (error message exposed as `error`) | yes | — |
| **drop** | `drop` | yes (concession — see note) | yes (terminates routing for this event) |
| **finish** | `finish` | — | yes (completes pipeline early without dropping) |
| **error** | `error` or `error <expr>` | yes | yes (routes event to DLQ with optional message) |

> **Note on `drop` inside a process body.** `drop` is fundamentally a routing decision (where the event goes — namely, nowhere) rather than a transformation, so in principle it belongs in a pipeline. The DSL allows it inside a process body anyway because in practice you sometimes recognise mid-transformation that the event isn't worth keeping (e.g., a parser snippet finds a malformed payload). Use it sparingly there; if a `drop` rule is reusable or its condition is independent of the surrounding transform, prefer expressing it at the pipeline level. See [Processing → process vs routing](./processing/README.md#process-vs-routing) for the full doctrine.

`if/else` and `switch` are the two constructs that work in both bodies, so the full treatment lives here. The other constructs are tied to one side and are documented on the page that owns them — pointers in *Where to use which* at the end of this section.

### if / else if / else

```limpid
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

Arms are statements valid in the surrounding body — pipeline statements at pipeline level (`output`, `process`, nested `if` / `switch`, `drop`, `finish`, `error`), process statements inside a `process` body (function calls, assignments, nested control flow, `drop`, `error`). An empty arm (`if cond { }`) is allowed but rare.

```limpid
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

```limpid
switch expr {
    value1 { ... }
    value2 { ... }
    default { ... }    // optional
}
```

The discriminator after `switch` is any DSL expression, evaluated once. Each arm's literal is matched against it with `==` semantics — types must agree (`switch workspace.severity { 5 { ... } }` matches `Int(5)` but not `String("5")`). The first matching arm runs. If none match, `default` runs; if `default` is absent, the `switch` is a no-op.

```limpid
// pipeline body — route by source IP
switch source.ip {
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

Position rule: `default` must be the last arm and may appear at most once; `--check` rejects otherwise (since 0.7.8). For example, the following is rejected:

```limpid
// NG — `default` is not last
switch source.ip {
    default     { output archive }
    "192.0.2.1" { output fw01 }
}
```

There is also an **expression form** of `switch` — each arm body is one expression rather than a statement list, and the matching arm's value is the value of the whole `switch`. Used inside `def function` bodies and anywhere a value is expected:

```limpid
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

- **`try-catch`** — process-body only. See [User-defined Processes → Control flow](./processing/user-defined.md#control-flow) for the syntax and the `error` name binding inside `catch`. Iteration over arrays is *not* a control-flow construct in limpid: use the block-arg primitives (`map`, `filter`, `find`, `reduce`) instead — see [Arrays](./processing/user-defined.md#arrays).
- **`drop` / `finish` / `error`** — pipeline routing. `drop` terminates the event silently (intended discard, counted as `events_dropped`); `finish` ends the pipeline early without dropping (counted as `events_finished`); `error <expr?>` routes the event to the [error log](./operations/error-log.md) with an operator-readable reason (counted as `events_errored`, same as a runtime process failure). `drop` and `error` are also allowed inside a process body; `finish` is pipeline-only. See [Pipelines → drop, finish, and error](./pipelines/drop-finish-error.md) for when to choose which.

## Pipe operator

The pipe operator chains expression-shaped transforms: `lhs |> f(...)` is parse-time sugar for `f(lhs, ...)` — the left-hand value is inserted as the first positional argument of the function call on the right. The transformation is purely syntactic; the AST contains only ordinary `FuncCall` nodes.

```limpid
// Without pipe:
workspace.users = distinct(map(filter(workspace.events) { |e| e.type == "auth" }) { |e| e.user })

// With pipe — read top-to-bottom:
workspace.users =
    workspace.events
    |> filter { |e| e.type == "auth" }
    |> map { |e| e.user }
    |> distinct
```

Pipe is universal — it works with any function (`foo |> to_int`, `evidence |> first |> path("file", "hash")`). Precedence is the lowest of any operator, so `a + 1 |> f(2)` parses as `f(a + 1, 2)`.

The right-hand side can be:

- A function call with explicit parens: `arr |> map(arr_extra) { |x| ... }`, `text |> regex_extract(pat)`.
- A bare identifier (zero-arg form): `arr |> first` ≡ `arr |> first()`.
- A bare identifier with a trailing block argument: `arr |> map { |x| x.id }` ≡ `arr |> map() { |x| x.id }`.

All three forms produce identical FuncCall AST after the parser splices the LHS as the first argument. The bare form is purely ergonomic — useful when the pipe is the only argument source and the function has no other positional inputs (`first` / `last` / `distinct` / `sum`) or only takes a block (`map` / `filter` / `find` over the pipe-fed array).

## Block argument

The block-arg primitives — `map`, `filter`, `find`, `reduce` — accept a trailing block argument that binds one (or, for `reduce`, two) identifier per element and runs the body against it:

```limpid
let evens = filter(workspace.nums) { |n| n % 2 == 0 }
let doubled = map(workspace.nums) { |n| n * 2 }
let user_alert = find(workspace.alerts) { |a| a.user == "alice" }
let total = reduce(workspace.amounts, 0) { |acc, x| acc + x }
```

The body has the same shape as a `def function` body: zero or more `let` bindings followed by a required trailing return expression. Locals introduced inside the body do not leak back to the caller — each iteration starts with a fresh child of the caller's scope.

Only `map` / `filter` / `find` / `reduce` accept block-args today; attaching one to any other function is a clear error. See [Built-in Functions → Array operations](./functions/expression-functions.md) for the per-primitive details and edge cases.

## Reserved identifiers

The following names are reserved and cannot be used as user identifiers:

- Event metadata: `ingress`, `egress`, `received_at`, `source`, `error`, `workspace`
- Keywords: `def`, `input`, `output`, `process`, `pipeline`, `function`, `if`, `else`, `switch`, `default`, `drop`, `finish`, `error`, `let`, `include`
- Literal markers: `true`, `false`, `null`

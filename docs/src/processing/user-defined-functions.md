# User-defined Functions

`def function` declares a **pure expression function** — given its arguments, it returns a value. No side effects, no Event access, no routing.

```
def function normalize_proto(num) {
    switch num {
        6  { "tcp" }
        17 { "udp" }
        1  { "icmp" }
        default { null }
    }
}
```

Use one anywhere an expression goes:

```
def process parse_fortigate_cef_traffic {
    workspace.limpid = {
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        // ... other canonical fields ...
    }
}
```

User-defined functions register into the same [`FunctionRegistry`](./functions.md) as built-in primitives — call sites dispatch through the same `(namespace, name)` lookup, the analyzer arity-checks them the same way, and a typo in the name surfaces the same way (`unknown function`, near-match suggestion).

## When to reach for it

`def function` is the right tool when you have a small, vendor-agnostic mapping or computation that:

- takes a few arguments,
- returns one value,
- doesn't need to read from `workspace.*` or other Event state directly, and
- is reused across multiple parsers / composers / processes.

Typical use cases:

| Need | Sketch |
|------|--------|
| Protocol number → name | `def function normalize_proto(num) { switch num { ... } }` |
| Severity string → OCSF `severity_id` | `def function normalize_severity(s) { switch lower(s) { ... } }` |
| Vendor action → OCSF `activity_id` | `def function fortigate_action_to_activity_id(a) { switch a { ... } }` |
| Numeric clamp / range check | `def function clamp(x, lo, hi) { switch true { x < lo { lo } x > hi { hi } default { x } } }` |
| String formatting helper | `def function host_label(h, p) { "${h}:${p}" }` |

For anything with side effects (writing to `workspace.*`, mutating `egress`, calling `process foo`, dropping the event), use [`def process`](./user-defined.md) instead.

## Body shape

The body is **a single expression**. To branch / map, use the expression-form `switch` ([DSL Syntax → switch](../dsl-syntax.md#switch)). Each arm body is one expression; the matching arm's value is the function's return value.

```
def function severity_id_from_label(s) {
    switch lower(s) {
        "critical" { 5 }
        "high"     { 4 }
        "medium"   { 3 }
        "low"      { 2 }
        "info"     { 1 }
        default    { 1 }
    }
}
```

Anything an expression can do (binary ops, primitive calls, hash literals, array literals, nested function calls) the body can do:

```
def function endpoint_label(host, port) {
    switch port {
        443 { "https://${host}" }
        80  { "http://${host}" }
        default { "${host}:${port}" }
    }
}

def function normalize(s) {
    lower(regex_replace(s, "\\s+", " "))
}
```

## Restrictions (enforced at `--check` time)

The body **may not**:

- **read from the Event** — `ingress`, `egress`, `source`, `received_at`, `error`, and any `workspace.*` path are rejected. Functions are pure transformations of their arguments; coupling them to the surrounding pipeline context defeats the point.
- **call user-defined `def process`** — process bodies have side effects (workspace writes, egress mutation, routing) that the function contract excludes.
- **recurse**, directly or mutually. The analyzer detects cycles in the function-to-function call graph and rejects them at config-load time. If you genuinely need recursion, write a `def process` instead.

```
// Rejected at --check time:
def function bad_event_ref() {
    workspace.foo + 1                          // ❌ reads workspace
}
def function bad_recursion(n) {
    bad_recursion(n - 1)                       // ❌ self-recursion
}
def function bad_routing(x) {
    drop                                        // ❌ routing keyword (parser rejects this)
}
def function bad_assignment(x) {
    workspace.cached = x                       // ❌ assignment (parser rejects this)
    x
}
```

The first one emits a warning (purity violation, `--ultra-strict` promotes to error). The cycle case is always an error. The last two are parser-level rejects — function body grammar doesn't allow `drop`, `finish`, `output`, `process`, or assignment statements.

## Calling other functions

Functions can call other functions (and any built-in primitive):

```
def function fortigate_severity_to_id(label) {
    severity_id_from_label(label)
}
```

The analyzer's cycle check catches mutual recursion across any chain length.

## Comparison with `def process`

| Aspect | `def function` | `def process` |
|--------|----------------|---------------|
| Body shape | one expression | sequence of statements |
| Returns | a value | nothing (mutates Event) |
| Reads `workspace.*` | ❌ rejected | ✅ allowed |
| Writes `workspace.x = …` | ❌ parser-rejected | ✅ allowed |
| Mutates `egress` | ❌ parser-rejected | ✅ allowed |
| `drop` / `finish` | ❌ parser-rejected | ✅ allowed (drop) |
| Calls another `def function` | ✅ | ✅ |
| Calls another `def process` | ❌ analyzer-rejected | ✅ |
| Recursion | ❌ analyzer-rejected | ✅ allowed (operator-responsible) |
| Composable in expressions / HashLit | ✅ | ❌ (statement only) |

Rule of thumb: **if the result is a single value the caller wants to embed somewhere**, write a function. **If the result is a side effect on the Event**, write a process.

## Example: vendor parser glue

A typical vendor parser uses several small functions to canonicalise vendor-specific values into OCSF-shape:

```
// _common/severity.limpid
def function normalize_severity(s) {
    switch lower(s) {
        "critical" { 5 }
        "high"     { 4 }
        "medium"   { 3 }
        "low"      { 2 }
        default    { 1 }
    }
}

// _common/proto.limpid
def function normalize_proto(num) {
    switch num {
        6 { "tcp" }
        17 { "udp" }
        1 { "icmp" }
        default { null }
    }
}

// parsers/fortigate.limpid
def process parse_fortigate_cef_traffic {
    workspace.limpid = {
        class_uid: 4001,
        severity_id: normalize_severity(workspace.cef.severity),
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        src_endpoint: { ip: workspace.cef.src, port: workspace.cef.spt },
        dst_endpoint: { ip: workspace.cef.dst, port: workspace.cef.dpt }
    }
}
```

Same `normalize_severity` and `normalize_proto` get reused by every other vendor's parser — no duplication, no Event coupling, no need for separate workspace scratch keys.

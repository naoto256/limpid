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

A call to `normalize_proto(x)` looks like any other function call — there's no marker at the call site that says "this is user-defined." The analyzer arity-checks it the same as a built-in, and a typo in the name surfaces the same way (`unknown function`, near-match suggestion).

The name must be a bare identifier. `def function normalize_proto { ... }` is allowed; `def function foo.bar { ... }` is **not** — the dot namespace is reserved for schema-bound built-ins (`syslog.parse`, `cef.parse`, `otlp.encode_resourcelog_protobuf`, …) where the prefix names a specific schema specification (RFC 5424, ArcSight CEF, OCSF, …). User-defined functions are vendor-agnostic by design, so they always live in the flat namespace. See the [*Schema-specific functions live under a schema namespace*](../design-principles.md#schema-specific-functions-live-under-a-schema-namespace) operating rule for the rationale.

## Where they can be called from

Anywhere an expression is evaluated — there's no callsite restriction:

- **Process bodies**: `workspace.limpid.severity_id = normalize_severity(workspace.cef.severity)`.
- **Pipeline-level conditions**: `if is_critical(workspace.limpid.severity_id) { output urgent }`.
- **`output` templates**: `path "/var/log/limpid/${normalize_proto(workspace.cef.proto)}/events.log"`.
- **HashLit values**: `workspace.limpid = { severity_id: normalize_severity(...), ... }`.
- **Function arguments**: `lower(normalize_proto(workspace.cef.proto))`.
- **Binary operands**: `if double_score(s) > threshold { ... }`.

The purity contract restricts the **body** of the function (no Event reads, no side effects). The call site is unrestricted: it operates in the surrounding expression's evaluation context, which can read the Event normally and pass concrete values into the function.

The mental model is the same as built-in primitives: `lower()` and `regex_match()` don't care where they're called from (pipeline `if` conditions, output `path` templates, process bodies — all valid). User-defined `normalize_proto()` is no different. Both are dispatched through `FunctionRegistry::call` with already-evaluated arguments. The only operator-visible difference is that `def function` lets you ship a vendor-agnostic mapping in the DSL itself, without touching Rust.

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

For anything with side effects (writing to `workspace.*`, mutating `egress`, calling `process foo`, dropping the event), use [`def process`](../processing/user-defined.md) instead.

## Body shape

The body is **zero or more `let` bindings followed by a required trailing expression** that becomes the return value:

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

For non-trivial computations, factor intermediate values into `let` bindings:

```
def function normalize(s) {
    let trimmed = trim(s)
    let lowered = lower(trimmed)
    regex_replace(lowered, "\\s+", " ")
}
```

`let` is the **assignment form** for local-scope variables in limpid — not a separate "declaration" step. Re-assigning the same name is just another `let` line:

```
def function f(x) {
    let v = x
    let v = v * 3              // reassigns v in the same scope
    v
}
```

For branching, use the expression-form `switch` ([DSL Syntax → switch](../dsl-syntax.md#switch)) — every `switch` arm is itself an expression, so it composes inside `let` RHS, function arguments, or as the trailing return:

```
def function endpoint_label(host, port) {
    let scheme = switch port {
        443 { "https" }
        80  { "http" }
        default { null }
    }
    switch scheme {
        null    { "${host}:${port}" }
        default { "${scheme}://${host}" }
    }
}
```

Anything an expression can do (binary ops, primitive calls, hash literals, array literals, nested function calls) is fair game inside `let` RHS or the trailing expression. What you cannot do is write a *statement* — no assignments to anything, no `drop` / `finish` / `process foo` / `output foo`, no statement-form `if` / `switch` / `foreach` / `try-catch`. Use the expression-form alternatives.

## Restrictions (enforced at `--check` time)

The body **may not**:

- **read from the Event** — `ingress`, `egress`, `source`, `received_at`, `error`, and any `workspace.*` path are rejected. Functions are pure transformations of their arguments; coupling them to the surrounding pipeline context defeats the point.
- **invoke any routing op** — `process foo`, `drop`, `finish`, `output` are all rejected. A function returns a value; routing decisions belong at pipeline level, and the side effects of a `def process` body don't fit the function contract.
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
    drop                                        // ❌ routing keyword
}
def function bad_assignment(x) {
    workspace.cached = x                       // ❌ assignment
    x
}
```

All four are hard errors at `--check` time — the config fails to load and the daemon won't start until they're fixed.

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
| Body shape | `let` bindings + trailing return expression | sequence of statements |
| Returns | a value | nothing (mutates Event) |
| Reads `workspace.*` / `ingress` / `egress` / … | ❌ | ✅ allowed |
| Any assignment (`x = …`) | ❌ | ✅ allowed |
| `drop` / `finish` / `output foo` / `process foo` | ❌ | ✅ allowed |
| Calls another `def function` | ✅ | ✅ |
| Recursion | ❌ | ✅ allowed (operator-responsible) |
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

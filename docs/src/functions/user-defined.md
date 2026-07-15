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
    workspace.lsis.parsed = {
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        // ... other canonical fields ...
    }
}
```

A call to `normalize_proto(x)` looks like any other function call — there's no marker at the call site that says "this is user-defined." The analyzer arity-checks it the same as a built-in, and a typo in the name surfaces the same way (`unknown function`, near-match suggestion).

The name must be a bare identifier. `def function normalize_proto() { ... }` is allowed; `def function foo.bar() { ... }` is **not** — the dot namespace is reserved for schema-bound built-ins (`syslog.parse`, `cef.parse`, `otlp.encode_resourcelog_protobuf`, …) where the prefix names a specific schema specification (RFC 5424, ArcSight CEF, OCSF, …). User-defined functions always live in the flat namespace; their names may still identify a vendor-specific mapping whose input domain is documented by that vendor. See the [*Schema-specific functions live under a schema namespace*](../design-principles.md#schema-specific-functions-live-under-a-schema-namespace) operating rule for the rationale.

## Where they can be called from

Anywhere an expression is evaluated — there's no callsite restriction on the function dispatch itself:

- **Process bodies**: `workspace.lsis.parsed.severity_number = severity_number_from_label(workspace.vendor.severity)`.
- **Pipeline-level conditions**: `if is_critical(workspace.lsis.parsed.severity_number) { output urgent }`.
- **`output` templates over event-intrinsic args**: `path "/var/log/limpid/${normalize_proto(source.port)}/events.log"` — the function call itself is fine; what *its arguments* may reference is restricted by the surrounding surface (output config rejects `workspace`, `egress`, `error`; see [DSL Syntax → String interpolation](../dsl-syntax.md#string-interpolation)). To route on a pipeline-mutable value, branch in the pipeline body and select between outputs whose own templates only reference event-intrinsic fields:
  ```limpid
  def output proto_tcp { type file path "/var/log/limpid/tcp/events.log" }
  def output proto_udp { type file path "/var/log/limpid/udp/events.log" }
  def pipeline split {
      input syslog_udp
      process parse_cef                                  // sets workspace.cef.proto
      switch normalize_proto(workspace.cef.proto) {
          "tcp" { output proto_tcp }
          "udp" { output proto_udp }
      }
  }
  ```
- **HashLit values**: `workspace.lsis.parsed = { severity_number: severity_number_from_label(...), ... }`.
- **Function arguments**: `lower(normalize_proto(workspace.cef.proto))`.
- **Binary operands**: `if double_score(s) > threshold { ... }`.

The purity contract restricts the **body** of the function (no Event reads, no side effects). The call site is dispatch-wise unrestricted: it operates in the surrounding expression's evaluation context, which can read whatever that surface allows (full Event in process bodies / pipeline expressions; event-intrinsic only in output config templates) and pass concrete values into the function.

The mental model is the same as built-in primitives: `lower()` and `regex_match()` don't care where they're called from. User-defined `normalize_proto()` is no different. Both are dispatched through `FunctionRegistry::call` with already-evaluated arguments. The only operator-visible difference is that `def function` lets you ship a vendor-agnostic mapping in the DSL itself, without touching Rust.

## When to reach for it

`def function` is the right tool when you have a small mapping or computation with an explicit input domain that:

- takes a few arguments,
- returns one value,
- doesn't need to read from `workspace.*` or other Event state directly, and
- is reused across multiple parsers / composers / processes.

Typical use cases:

| Need | Sketch |
|------|--------|
| Protocol number → name | `def function normalize_proto(num) { switch num { ... } }` |
| Source severity string → OTel `SeverityNumber` | `def function severity_number_from_label(s) { switch s { ... } }` |
| Vendor action → OCSF `activity_id` | `def function fortigate_action_to_activity_id(a) { switch a { ... } }` |
| Numeric clamp / range check | `def function clamp(x, lo, hi) { switch true { x < lo { lo } x > hi { hi } default { x } } }` |
| String formatting helper | `def function host_label(h, p) { "${h}:${p}" }` |

For anything with side effects (writing to `workspace.*`, mutating `egress`, calling `process foo`, dropping the event), use [`def process`](../processing/user-defined.md) instead.

## Body shape

The body is **zero or more `let` bindings followed by a required trailing expression** that becomes the return value:

```
def function severity_number_from_label(s) {
    switch s {
        "Critical"      { 21 }
        "High"          { 19 }
        "Medium"        { 17 }
        "Low"           { 13 }
        "Informational" { 9 }
        default         { null }
    }
}
```

For non-trivial computations, factor intermediate values into `let` bindings:

```
def function normalize(s) {
    let trimmed = regex_replace(s, "^\\s+|\\s+$", "")
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

Anything an expression can do (binary ops, primitive calls, hash literals, array literals, nested function calls) is fair game inside `let` RHS or the trailing expression. The block-arg primitives — `map`, `filter`, `find`, `reduce` — are pure expressions over arrays and compose freely inside a function body. What you cannot do is write a *statement* — no assignments to anything, no `drop` / `error` / `process foo` / `output foo`, no statement-form `if` / `switch` / `try-catch`. Use the expression-form alternatives.

## Restrictions (enforced at `--check` time)

The body **may not**:

- **read from the Event** — `ingress`, `egress`, `source`, `received_at`, `error`, and any `workspace.*` path are rejected. Functions are pure transformations of their arguments; coupling them to the surrounding pipeline context defeats the point.
- **invoke any routing op** — `process foo`, `drop`, [`error`](../processing/user-defined.md#error), `output` are all rejected. A function returns a value; routing decisions belong at pipeline level, and the side effects of a `def process` body don't fit the function contract. (`finish` is a pipeline-only statement and is not reachable from a function body even in principle — listed here for completeness.)
- **recurse**, directly or mutually. The analyzer detects cycles in the function-to-function call graph and rejects them at config-load time. If you genuinely need recursion, write a `def process` instead.
- **call an unknown function** — every function call inside the body must resolve to either a built-in primitive, a user-defined `def function`, or (if a block-arg primitive's block) the block parameters. Calls to names that don't exist (typos, references to removed primitives) are rejected, with a near-match hint when available.

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
def function bad_typo(x) {
    to_lower(x)                                 // ❌ unknown function (did you mean `lower`?)
}
```

All five are hard errors at `--check` time — the config fails to load and the daemon won't start until they're fixed.

## Calling other functions

Functions can call other functions (and any built-in primitive):

```
def function vendor_severity_number(label) {
    severity_number_from_label(label)
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
| `drop` / `error` / `output foo` / `process foo` | ❌ | ✅ allowed |
| Calls another `def function` | ✅ | ✅ |
| Recursion | ❌ | ✅ allowed (operator-responsible) |
| Composable in expressions / HashLit | ✅ | ❌ (statement only) |

Rule of thumb: **if the result is a single value the caller wants to embed somewhere**, write a function. **If the result is a side effect on the Event**, write a process.

## Example: vendor parser glue

A typical vendor parser uses several small functions to canonicalise vendor-specific values into canonical LSIS shape:

```
// functions/severity_number_from_label.limpid
def function severity_number_from_label(s) {
    switch s {
        "Critical"      { 21 }
        "High"          { 19 }
        "Medium"        { 17 }
        "Low"           { 13 }
        "Informational" { 9 }
        default         { null }
    }
}

// functions/normalize_proto.limpid
def function normalize_proto(num) {
    switch num {
        6 { "tcp" }
        17 { "udp" }
        1 { "icmp" }
        default { null }
    }
}

// parsers/parse_vendor_event.limpid
def process parse_vendor_event {
    let source_severity = workspace.vendor.severity
    let severity_number = severity_number_from_label(source_severity)
    if source_severity != null and severity_number == null {
        error "parse_vendor_event: invalid severity ${source_severity}"
    }
    workspace.lsis.parsed = {
        class_uid: 4001,
        severity_number: severity_number,
        severity: source_severity,
        connection_info: {
            protocol_num:  workspace.vendor.proto,
            protocol_name: normalize_proto(workspace.vendor.proto)
        },
        src_endpoint: { ip: workspace.vendor.src, port: workspace.vendor.spt },
        dst_endpoint: { ip: workspace.vendor.dst, port: workspace.vendor.dpt }
    }
}
```

The mapper returns `null` for values outside its documented source domain; the
process boundary distinguishes that invalid non-null input from a genuinely
missing severity and fails loudly. The exact source spelling is preserved in
`parsed.severity`, while `parsed.severity_number` carries the normalized OTel
value. OCSF `severity_id` is derived later by the OCSF composer.
Reuse a severity mapper only for sources that define the same exact vocabulary.

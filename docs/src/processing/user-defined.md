# User-defined Processes

limpid is preparing a [Snippet Library](../snippets/README.md) of pre-built processes for common parsing / mapping work (landing in v0.6.0). Even once it ships, sooner or later you'll hit a situation the library doesn't cover — a vendor format we haven't shipped, a one-off enrichment, a dedup or rate-limit shape that's specific to your environment — and want to write your own. This page covers how.

You define a process with `def process <name> { ... }`. Inside the body you call functions, assign to event slots and workspace, branch with `if` / `switch` / `try`, transform arrays with the block-arg primitives (`map`, `filter`, `find`, `reduce`), and call other processes by name.

## Defining a process

```
def process enrich_fortigate {
    workspace.cef = cef.parse(workspace.syslog.msg)   // capture parser output

    if workspace.cef.src != null {
        workspace.geo = geoip(workspace.cef.src)
    }

    egress = "${workspace.cef.device_product} ${workspace.cef.src} -> ${workspace.cef.dst} ${workspace.cef.act}"
}
```

The CEF parser call captures into `workspace.cef`; the `${...}` interpolation in the final string assembles the egress line. See [Built-in Functions](../functions/expression-functions.md) for the full function surface and [DSL Syntax Basics → String interpolation](../dsl-syntax.md#string-interpolation) for the template syntax.

## Assignments

| Target | Effect |
|--------|--------|
| `egress = expr` | Replace the bytes the output will write |
| `workspace.xxx = expr` | Set a workspace value (nested: `workspace.geo.country = "JP"`) |
| `let name = expr` | Bind a process-local scratch value (visible only inside this process body); the binding name is a single identifier — `let foo.bar = ...` is rejected |

Anything else on the left of `=` is rejected as an unknown assignment target.

A `let` binding can hold any value type — scalar, Object, or Array. When the bound value is an Object, dot-access reads through the binding the same way `workspace.x.y` does:

```
def process parse_xxx {
    let f = regex_parse(workspace.body, "(?P<user>\\S+) (?P<host>\\S+)")
    workspace.limpid = {
        user: f.user,                    // read from the let-bound Object
        host: f.host
    }
}
```

Read-side dot-access on a `let` binding is the same path-walk used for `workspace.*`: missing keys yield `null` (so `coalesce(f.maybe_missing, fallback)` works). The restriction is only on the write side: `let f = obj` followed by `let f.x = 1` is not legal — write into `workspace.*` if you need to mutate a structure.

### Important: what is and isn't reflected in output

**`egress`** is the byte buffer that output modules write to the wire. If you want to change what gets sent, you must change `egress`:

```
// This changes the output:
egress = "${workspace.syslog.hostname}: ${workspace.syslog.msg}"

// This does NOT change the output:
workspace.syslog.hostname = "new-host"
// ↑ sets a workspace value, but `egress` is unchanged
```

`workspace` is a pipeline-local scratch area for intermediate values — parsed data, enrichment results, routing decisions. Workspace values are **not** automatically serialised into `egress`. To include them in the output, explicitly rebuild `egress`:

```
workspace.kv = parse_kv(egress)               // capture into namespace
egress = to_json(workspace)                   // serialise workspace as JSON
// or
egress = "${workspace.kv.srcip} -> ${workspace.kv.dstip}"
```

### Rewriting the syslog PRI

```
def process ama_rewrite {
    if contains(ingress, "CEF:") {
        egress = syslog.set_pri(egress, 16, 6)   // local0.info for CEF
    } else {
        egress = syslog.set_pri(egress, 17, 6)   // local1.info for everything else
    }
}
```

## Control flow

`if` / `else` / `switch` are documented in full at [DSL Syntax Basics → Control flow](../dsl-syntax.md#control-flow); they work the same inside a process body, with the arms being process statements (function calls, assignments, nested control flow). The notes below cover what's *unique* to process-body control flow.

### try / catch

```
try {
    // body — may raise an error
} catch {
    // handler — runs only when the try body raised
}
```

Catches errors raised inside the `try` body — an unparseable `parse_json` input, a `to_int` overflow, a regex compile failure, or any other primitive that bails. The error message is bound to the reserved name `error` inside the `catch` block.

```
def process safe_parse_json {
    try {
        workspace.body = parse_json(egress)
    } catch {
        workspace.parse_failed = true
        workspace.parse_error = error
    }
}
```

`error` inside `catch` is a `Value::String` containing the bail message. It is scoped to the immediate `catch` body — not visible after the block ends, not visible inside a nested process call.

#### When to use `try` vs let the error propagate

`try` is for **expected** failures the operator knows how to handle inline — typically a mixed stream where some events take one shape and others take another, and the process needs to record which is which on workspace and continue.

When an error is **unexpected** (a bug in a wrap process, a parser hitting input it wasn't designed for, a typo'd workspace key), don't wrap it in `try`. Let it propagate: limpid sets the original event aside in the [error log](../operations/error-log.md) (DLQ) and increments `events_errored`. The operator audits the DLQ, fixes the cause, and replays via `jq | limpidctl inject --json`. Catching unexpected errors in `try` blocks would silently swallow the bug — the event would still flow downstream with whatever partial state the catch block put on workspace, and the failure signal would be lost.

Rule of thumb: if the catch block has nothing useful to do besides stamp `workspace.failed = true`, you don't want `try` — you want the DLQ.

| Situation | Use |
|-----------|-----|
| "Some events here are JSON, some are KV — record which is which" | `try { parse_json } catch { try { parse_kv } catch { ... } }` |
| "Optional enrichment — fall back to a default when GeoIP misses" | `try { ... } catch { workspace.geo = {country: "??"} }` |
| "An input I didn't anticipate broke my parser" | **No `try`** — let it land in the DLQ, fix the parser, replay. |
| "I want to know which events failed" | **No `try`** — `events_errored` + DLQ is exactly that signal. |

### drop

`drop` terminates the event immediately and counts it as `events_dropped`. It is fundamentally a routing decision and is documented in [Pipelines → Routing](../pipelines/routing.md); using it inside a process body is allowed as a concession (see [Processing → process vs routing](./README.md#process-vs-routing)).

## Arrays

limpid arrays are **ordered but index-hidden**. You construct them with `[a, b, c]` literals; reads order-preservingly through `map` / `filter` / `concat` / `first` / `last` / `distinct`; identity-based picks through `find`; whole-array reductions through `reduce` / `sum` / `max` / `min` / `len`; growth through `append` / `prepend`. What you cannot do is name an element by numeric index — `arr[0]`, `arr[-1]`, `arr[0] = v` are intentionally absent from the grammar, and `path(arr, 0)` is rejected too.

### Why no positional access

A numeric index is a human convenience that drifts the moment anything else mutates the collection. If "evidence of type Process" happened to land at `arr[0]` in one event and `arr[1]` in the next because an extra entity was prepended upstream, positional code silently reads the wrong thing. Use `find` to address by intrinsic identity, or `first` / `last` when "the only one" or "the most recent" is what you actually mean:

```
// WRONG (position is an accident of construction order)
workspace.process = workspace.evidence[0]

// RIGHT (identity survives insertion / deletion)
workspace.process = find(workspace.evidence) { |e| e.entityType == "Process" }
```

The library steers toward identity-based access so snippets stay correct under upstream evolution of vendor schemas. `first` / `last` are reserved for cases where the chronological / construction order is itself the contract (e.g. "the last login event in this session").

### What arrays can do

| Operation | Form |
|-----------|------|
| Construction | `[a, b, c]`, `[]`, mixed types and nesting OK |
| Per-element transform | `map(arr) { \|x\| <expr> }` — order-preserving |
| Predicate filter | `filter(arr) { \|x\| <bool> }` — order-preserving |
| Identity-keyed pick | `find(arr) { \|x\| <bool> }` — first match or `null` |
| Head / tail | `first(arr)`, `last(arr)` — element or `null` |
| Fold | `reduce(arr, init) { \|acc, x\| <step> }` |
| Numeric reductions | `sum(arr)`, `max(arr)`, `min(arr)` |
| Cardinality | `len(arr)` |
| Concatenate | `concat(a, b, ...)` |
| Dedupe | `distinct(arr)` (preserves first occurrence) |
| Name positional captures | `entitle(values, keys)` — `[v1, v2]` × `["a", "b"]` → `{a: v1, b: v2}` |
| Dynamic key access | `path(obj, "k1", "k2", ...)` (object only; integer keys rejected) |
| Add to back / front | `append(arr, v)`, `prepend(arr, v)` |
| Serialise to JSON | `to_json(arr)` |

Block-arg primitives (`map`, `filter`, `find`, `reduce`) and the pipe operator (`|>`) are documented in [DSL Syntax Basics → Block argument](../dsl-syntax.md#block-argument) and [Pipe operator](../dsl-syntax.md#pipe-operator).

### Example: building an OCSF multi-value field

```
def process compose_types {
    // Start with a fresh collection. Arrays preserve construction order
    // but consumers should not rely on the index of any one element.
    workspace.types = []

    if workspace.cef.name != null {
        workspace.types = append(workspace.types, workspace.cef.name)
    }
    if workspace.pan_threat_type != null {
        workspace.types = append(workspace.types, workspace.pan_threat_type)
    }
}
```

### Example: picking specific evidence from an MDE alert

```
def process parse_mde_alert {
    parse_json(ingress)
    workspace.process_ev = find(workspace.evidence) { |e| e.entityType == "Process" }
    workspace.user_ev    = find(workspace.evidence) { |e| e.entityType == "User" }
}
```

Neither lookup cares whether "the Process entity" appears first, last, or third in the evidence list. That independence is the point.

### Example: chained transforms with pipe

```
def process top_user_logins {
    // The pipe operator inserts the LHS as the first arg of the call on
    // the right; `a |> f(b)` ≡ `f(a, b)`. Reads top-to-bottom.
    workspace.distinct_users =
        workspace.events
        |> filter { |e| e.kind == "login" }
        |> map { |e| e.user }
        |> distinct
}
```

## process call

Inside a process body you can call another named process by name:

```
def process enrich_and_tag {
    process enrich_fortigate
    process my_custom_enrichment
    workspace.tagged_at = timestamp()
}
```

Each call runs the named process's body against the same event in sequence. Only the named single-call form works here — the `|`-chain shape (`process strip_headers | enrich`) and inline anonymous processes (`process { ... }`) are pipeline-side composition primitives, not process-body statements. See [Pipelines → Routing](../pipelines/routing.md) for those.

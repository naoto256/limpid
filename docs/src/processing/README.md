# Processing

A `process` is a unit of event transformation in the DSL. Inside a process body you call functions, assign to `egress` / `workspace` / `let` bindings, branch on conditions, and `drop` events you don't want.

There is no separate "built-in process" layer — every process is either:

- a **named user-defined process** (`def process name { ... }`), referenced from a pipeline as `process name`; or
- an **inline anonymous process** (`process { ... }`), defined where it's used.

## Process vs routing

A pipeline body can also contain `if` / `switch` / `drop` / `finish` directly — those are routing statements, not transformations, so they don't need a `process` wrapper. Function calls and assignments, on the other hand, are transformations and must live inside a `process` body (named or inline). The split is clean: **process** does what an event becomes, **pipeline** does where an event goes.

> **Recommendation.** When you write a control-flow construct (`if`, `switch`, `drop`, `finish`), pause for a moment and ask which side of that line it belongs on. A condition that decides whether the event continues toward an output is routing — write it at pipeline level. A condition that affects how the event is *transformed* (deciding which fields to set, which parser to call) is a transformation — write it inside a `process`. Mixing the two — for example, an `if` at pipeline level whose body mutates `workspace` via an inline `process { }` — usually means the named `process` for that decision hasn't been extracted yet.

## Where things live

| Concept | Where to look |
|---------|---------------|
| Available functions and their signatures | [Expression Functions](./functions.md) |
| `${...}` interpolation in any string literal | [DSL Syntax Basics → String interpolation](../dsl-syntax.md#string-interpolation) |
| Defining a process, control flow, `let`, `drop` | [User-defined Processes](./user-defined.md) |
| Defining a pure value-returning function (`def function`) | [User-defined Functions](./user-defined-functions.md) |
| Writing maintainable processes and snippet conventions | [Process Design Guide](./design-guide.md) |

## Process chains

In a pipeline, processes can be chained with `|`:

```
process strip_headers | enrich | {
    workspace.geo = geoip(workspace.src)
    egress = to_json(workspace)
}
```

Each stage runs in sequence on the same event. If any stage drops the event, the remaining stages are skipped.

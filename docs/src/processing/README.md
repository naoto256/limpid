# Processing

A `process` is a unit of event transformation in the DSL. Inside a process body you call functions, assign to `egress` / `workspace` / `let` bindings, branch on conditions, and `drop` events you don't want.

There is no separate "built-in process" layer — every process is either:

- a **named user-defined process** (`def process name { ... }`), referenced from a pipeline as `process name`; or
- an **inline anonymous process** (`process { ... }`), defined where it's used.

Earlier 0.x releases shipped a handful of native process modules (`process parse_syslog`, `process parse_cef`, `process strip_pri`, …). They have been removed in 0.3 — the same work is now done by DSL function calls inside a process body. See [Upgrading to 0.3](../operations/upgrade-0.3.md) for the rewrite recipes.

## Where things live

| Concept | Where to look |
|---------|---------------|
| Available functions and their signatures | [Expression Functions](./functions.md) |
| `${...}` interpolation in any string literal | [String Templates](./templates.md) |
| Defining a process, control flow, `let`, `drop` | [User-defined Processes](./user-defined.md) |

## Process chains

In a pipeline, processes can be chained with `|`:

```
process strip_headers | enrich | {
    workspace.geo = geoip(workspace.src)
    egress = to_json()
}
```

Each stage runs in sequence on the same event. If any stage drops the event, the remaining stages are skipped.

# Functions

Functions return values. They appear in conditions, on the right-hand side of assignments, inside `${...}` interpolations, in HashLit values, as `process` arguments, and as bare statements inside a `process` body (where the returned object is merged into `workspace`).

limpid distinguishes two forms by where the implementation lives, but the call surface is identical for both:

| Form | Where it's implemented | Page |
|------|------------------------|------|
| **Built-in primitive** (flat or namespaced) | Rust, ships with the daemon | [Built-in Functions](./expression-functions.md) |
| **User-defined** (`def function`) | DSL, declared by the operator | [User-defined Functions](./user-defined.md) |

Both forms register into the same [`FunctionRegistry`](./expression-functions.md) — a call site like `f(args)` doesn't care whether `f` is a Rust primitive or an operator-authored DSL function. The analyzer arity-checks them uniformly, typos surface the same way (`unknown function`, near-match suggestion), and they're all callable from any expression context (process body, pipeline `if` condition, `output` template, HashLit value, function argument).

Where they differ:

- **Built-in primitives** can be stateful or schema-aware (`syslog.parse`, `cef.parse`, `geoip`, `table_lookup`, `hostname`). Adding a new one means a Rust commit and a daemon rebuild.
- **User-defined functions** are pure — no Event reads, no side effects, no recursion — and live in the DSL. Adding one is a config edit, no daemon rebuild. They're the right place for vendor-agnostic mappings (protocol number → name, severity string → OCSF `severity_id`, action string → activity_id).

The functions vs processes choice — when to write `def function` vs `def process` — is documented under [Process Design Guide → Functions vs. processes](../processing/design-guide.md#functions-vs-processes).

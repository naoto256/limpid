# Functions

Functions return values. They appear in conditions, on the right-hand side of assignments, inside `${...}` interpolations, in HashLit values, and as bare statements inside a `process` body (where the returned object is merged into `workspace`).

limpid distinguishes two forms by where the implementation lives, but the call surface is identical for both:

| Form | Where it's implemented | Page |
|------|------------------------|------|
| **Built-in primitive** (flat or namespaced) | Rust, ships with the daemon | [Built-in Functions](./expression-functions.md) |
| **User-defined** (`def function`) | DSL, declared by the operator | [User-defined Functions](./user-defined.md) |

Both use the same [call syntax](./expression-functions.md), and the analyzer
checks arity for both forms. Unknown calls are diagnosed, but a near-match
hint is not guaranteed: suggestion candidates come from flat built-in
primitive names, not user-defined function names. Both forms can be called
from expression contexts such as process bodies, pipeline conditions,
output templates, object values, and function arguments.

Where they differ:

- **Built-in primitives** can be stateful or schema-aware (`syslog.parse`, `cef.parse`, `geoip`, `table_lookup`, `hostname`).
- **User-defined functions** are pure — no Event reads, no side effects, no recursion — and live in the DSL. Adding one is a config edit, no daemon rebuild. They're the right place for mappings with explicit input domains (protocol number → name, a documented source severity vocabulary → OTel `SeverityNumber`, action string → activity_id).

The functions vs processes choice — when to write `def function` vs `def process` — is documented under [Process Design Guide → Functions vs. processes](../processing/design-guide.md#functions-vs-processes).

//! `path(obj, key1, key2, ...)` — dynamic dotted-path access.
//!
//! Equivalent to `obj.key1.key2.…` but with the keys computed at run
//! time. Missing keys yield `null` (matches `workspace.x.y` path-walker
//! semantics). The first argument may be Object, Array, or Null:
//!
//! - Object → looked up by name.
//! - Null → propagates as null (no error).
//! - Anything else → null (consistent with `.field` access on scalars).
//!
//! **Integer keys are rejected** — positional access on arrays is
//! intentionally absent from the DSL; `path(arr, 0)` would re-introduce
//! it through the back door. Operators who need a specific element use
//! `find(arr) {{ |x| ... }}` instead.

use anyhow::bail;

use crate::dsl::value::Value;
use crate::functions::{Arity, FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    // Manual signature: first slot Any (object / null fallthrough),
    // additional slots are strings. We use Optional with a generous
    // upper bound — the central arity check enforces minimum 2; per-arg
    // type validation happens in the closure because the Optional shape
    // does not differentiate per-slot types past the declared length.
    let sig = FunctionSig {
        args: vec![FieldType::Any],
        arity: Arity::Variadic { min: 2 },
        ret: FieldType::Any,
    };
    reg.register_with_sig("path", sig, |_arena, args, _event| walk(args));
}

fn walk<'bump>(args: &[Value<'bump>]) -> anyhow::Result<Value<'bump>> {
    let mut current = args[0];
    for (i, key) in args[1..].iter().enumerate() {
        match key {
            Value::String(k) => {
                current = match current {
                    Value::Object(entries) => entries
                        .iter()
                        .find(|(ek, _)| *ek == *k)
                        .map(|(_, v)| *v)
                        .unwrap_or(Value::Null),
                    Value::Null => Value::Null,
                    _ => Value::Null,
                };
            }
            Value::Int(_) => {
                bail!(
                    "path() rejects integer keys (positional array access is intentionally absent — \
                     use find(arr) {{ |x| ... }} to pick by identity); arg #{} was an integer",
                    i + 2
                );
            }
            other => bail!(
                "path() keys must be strings, got {} at arg #{}",
                other.type_name(),
                i + 2
            ),
        }
    }
    Ok(current)
}

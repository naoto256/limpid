//! `entitle(values, keys)` — zip an Array of values with an Array of
//! string keys into an Object.
//!
//! Length mismatch is a loud failure (not a silent truncate) because
//! the typical use case is naming positional captures from a parser:
//! a count drift between the value source and the key list is almost
//! always a parser-shape bug the operator wants to hear about.
//!
//! Keys must be strings; non-string keys bail. Result Object preserves
//! the key order from `keys`.

use anyhow::bail;

use crate::dsl::arena::EventArena;
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::{ObjectBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "entitle",
        FunctionSig::fixed(&[FieldType::Array, FieldType::Array], FieldType::Object),
        |arena, args, _event| build(arena, &args[0], &args[1]),
    );
}

fn build<'bump>(
    arena: &'bump EventArena<'bump>,
    values: &Value<'bump>,
    keys: &Value<'bump>,
) -> anyhow::Result<Value<'bump>> {
    let values = match values {
        Value::Array(v) => *v,
        other => bail!(
            "entitle() expects Array for values, got {}",
            other.type_name()
        ),
    };
    let keys = match keys {
        Value::Array(k) => *k,
        other => bail!(
            "entitle() expects Array for keys, got {}",
            other.type_name()
        ),
    };
    if values.len() != keys.len() {
        bail!(
            "entitle() length mismatch: {} values vs {} keys",
            values.len(),
            keys.len()
        );
    }
    let mut out = ObjectBuilder::with_capacity(arena, keys.len());
    for (k, v) in keys.iter().zip(values.iter()) {
        let key_str = match k {
            Value::String(s) => *s,
            other => bail!("entitle() keys must be strings, got {}", other.type_name()),
        };
        out.push(key_str, *v);
    }
    Ok(out.finish())
}

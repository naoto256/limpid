//! `max(arr)` — maximum of a numeric Array, or `null` if empty.
//!
//! Same numeric-only rule as `sum`: mixed Int + Float compares through
//! f64, non-numeric elements bail. Empty Array → `Null`.

use anyhow::bail;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "max",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Any),
        |_arena, args, _event| pick(&args[0], /*want_max=*/ true),
    );
}

pub(super) fn pick<'bump>(v: &Value<'bump>, want_max: bool) -> anyhow::Result<Value<'bump>> {
    let items = match v {
        Value::Array(items) => *items,
        Value::Null => return Ok(Value::Null),
        other => bail!(
            "{}() expects an Array, got {}",
            if want_max { "max" } else { "min" },
            other.type_name()
        ),
    };
    let mut best: Option<Value<'bump>> = None;
    for item in items {
        let cur_f = match item {
            Value::Int(n) => *n as f64,
            Value::Float(n) => *n,
            other => bail!(
                "{}() expects numeric elements, got {}",
                if want_max { "max" } else { "min" },
                other.type_name()
            ),
        };
        let take = match best {
            None => true,
            Some(b) => {
                let bf = b.as_f64().expect("best is always numeric");
                if want_max { cur_f > bf } else { cur_f < bf }
            }
        };
        if take {
            best = Some(*item);
        }
    }
    Ok(best.unwrap_or(Value::Null))
}

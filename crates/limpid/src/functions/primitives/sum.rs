//! `sum(arr)` — sum a numeric Array.
//!
//! All-Int input stays Int; any Float participant promotes the result
//! to Float. Empty Array → `Int(0)`. Non-numeric elements bail rather
//! than silently coerce; mixed numeric / null arrays bail too because
//! "ignore the nulls" is a per-pipeline policy decision and limpid does
//! not pick one for the operator.

use anyhow::bail;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "sum",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Any),
        |_arena, args, _event| add(&args[0]),
    );
}

fn add<'bump>(v: &Value<'bump>) -> anyhow::Result<Value<'bump>> {
    let items = match v {
        Value::Array(items) => *items,
        other => bail!("sum() expects an Array, got {}", other.type_name()),
    };
    let mut int_acc: i64 = 0;
    let mut float_acc: f64 = 0.0;
    let mut saw_float = false;
    for item in items {
        match item {
            // `checked_add` so an i64 overflow surfaces as a typed
            // error instead of either silently wrapping (release
            // builds, with no explicit `[profile]` overflow-checks
            // pin in any Cargo.toml) or panicking on the
            // accumulator step (debug builds). Pipelines summing
            // big integer arrays would otherwise emit nonsense
            // negative totals in release, and the bug would only
            // appear in production.
            Value::Int(n) => {
                int_acc = int_acc.checked_add(*n).ok_or_else(|| {
                    anyhow::anyhow!(
                        "sum() overflowed i64 (accumulator {int_acc}, element {n})"
                    )
                })?;
            }
            Value::Float(n) => {
                saw_float = true;
                float_acc += *n;
            }
            other => bail!("sum() expects numeric elements, got {}", other.type_name()),
        }
    }
    if saw_float {
        Ok(Value::Float(int_acc as f64 + float_acc))
    } else {
        Ok(Value::Int(int_acc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;

    fn run<'b>(_bump: &'b Bump, input: Value<'b>) -> anyhow::Result<Value<'b>> {
        // The `_bump` parameter keeps the arena alive for the lifetime
        // of `Value<'b>`; `add` itself doesn't need to allocate.
        add(&input)
    }

    fn arr<'b>(bump: &'b Bump, items: &[Value<'b>]) -> Value<'b> {
        Value::Array(bump.alloc_slice_copy(items))
    }

    #[test]
    fn sums_integers() {
        let bump = Bump::new();
        let v = run(&bump, arr(&bump, &[Value::Int(1), Value::Int(2), Value::Int(3)])).unwrap();
        assert_eq!(v, Value::Int(6));
    }

    #[test]
    fn empty_array_returns_int_zero() {
        let bump = Bump::new();
        let v = run(&bump, arr(&bump, &[])).unwrap();
        assert_eq!(v, Value::Int(0));
    }

    #[test]
    fn mixed_int_float_promotes_to_float() {
        let bump = Bump::new();
        let v = run(&bump, arr(&bump, &[Value::Int(1), Value::Float(2.5)])).unwrap();
        assert_eq!(v, Value::Float(3.5));
    }

    #[test]
    fn rejects_non_array_input() {
        let bump = Bump::new();
        let err = run(&bump, Value::Int(42)).err().unwrap();
        assert!(err.to_string().contains("expects an Array"), "{err}");
    }

    #[test]
    fn rejects_null_input_with_array_error() {
        // Earlier shape silently treated Null as Int(0); the current
        // contract is "schema says Array, anything else is a type
        // error". Regression guard for that contract.
        let bump = Bump::new();
        let err = run(&bump, Value::Null).err().unwrap();
        assert!(err.to_string().contains("expects an Array"), "{err}");
    }

    #[test]
    fn rejects_non_numeric_element() {
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Int(1), Value::String("nope")]);
        let err = run(&bump, v).err().unwrap();
        assert!(err.to_string().contains("expects numeric elements"), "{err}");
    }

    #[test]
    fn integer_overflow_returns_typed_error() {
        // Regression guard: `int_acc += *n` would wrap silently in
        // release builds and panic in debug. `checked_add` produces
        // a typed error so pipelines surface the bug instead of
        // shipping bogus negative totals.
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Int(i64::MAX), Value::Int(1)]);
        let err = run(&bump, v).err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("overflowed"), "{msg}");
        assert!(msg.contains("i64"), "{msg}");
    }

    #[test]
    fn integer_overflow_works_at_min_too() {
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Int(i64::MIN), Value::Int(-1)]);
        let err = run(&bump, v).err().unwrap();
        assert!(err.to_string().contains("overflowed"), "{err}");
    }

    #[test]
    fn sum_at_i64_max_without_overflow_succeeds() {
        // Boundary case — adding 0 to MAX should not trip the check.
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Int(i64::MAX), Value::Int(0)]);
        let result = run(&bump, v).unwrap();
        assert_eq!(result, Value::Int(i64::MAX));
    }
}

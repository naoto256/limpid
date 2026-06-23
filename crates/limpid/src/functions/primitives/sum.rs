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
    // Decide the accumulator type up front by scanning for any Float.
    // Otherwise `[i64::MAX, 1, 0.5]` would overflow the i64 accumulator
    // on element 2 before discovering that the result type is float
    // anyway. The pre-scan is one extra pass over the slice (cheap;
    // no per-element work besides the variant tag check) and avoids
    // surprising the operator with a spurious overflow error when
    // their intent was clearly a float total.
    let has_float = items.iter().any(|i| matches!(i, Value::Float(_)));
    if has_float {
        let mut acc: f64 = 0.0;
        for item in items {
            match item {
                Value::Int(n) => acc += *n as f64,
                Value::Float(n) => acc += *n,
                other => bail!("sum() expects numeric elements, got {}", other.type_name()),
            }
        }
        Ok(Value::Float(acc))
    } else {
        // All-Int path: `checked_add` so overflow surfaces as a typed
        // error instead of either silently wrapping (release builds,
        // with no explicit `[profile]` overflow-checks pin in any
        // Cargo.toml) or panicking on the accumulator step (debug
        // builds). Pipelines summing big integer arrays would
        // otherwise emit nonsense negative totals in release, and
        // the bug would only appear in production.
        let mut acc: i64 = 0;
        for item in items {
            match item {
                Value::Int(n) => {
                    acc = acc.checked_add(*n).ok_or_else(|| {
                        anyhow::anyhow!(
                            "sum() overflowed i64 (accumulator {acc}, element {n}); \
                             if inputs may exceed i64::MAX, promote one element to \
                             float first (e.g. multiply by 1.0) so the accumulator \
                             becomes f64"
                        )
                    })?;
                }
                other => bail!("sum() expects numeric elements, got {}", other.type_name()),
            }
        }
        Ok(Value::Int(acc))
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

    #[test]
    fn float_only_sum() {
        let bump = Bump::new();
        let v = run(
            &bump,
            arr(&bump, &[Value::Float(1.5), Value::Float(2.25), Value::Float(0.25)]),
        )
        .unwrap();
        assert_eq!(v, Value::Float(4.0));
    }

    #[test]
    fn mixed_int_float_does_not_spuriously_overflow_i64() {
        // Regression guard: an earlier shape used a single i64
        // accumulator that fell back to float only after seeing a
        // Float, so `[i64::MAX, 1, 0.5]` errored with an i64
        // overflow before reaching the Float that should have
        // promoted the result. Pre-scanning the array for any Float
        // means the accumulator type is decided up front; this array
        // now returns a (lossy) f64 total instead of failing.
        let bump = Bump::new();
        let v = arr(
            &bump,
            &[Value::Int(i64::MAX), Value::Int(1), Value::Float(0.5)],
        );
        let result = run(&bump, v).unwrap();
        // f64 cannot represent i64::MAX + 1 exactly, but the call
        // succeeds and the magnitude is right (~9.22e18).
        match result {
            Value::Float(f) => assert!(f.is_finite() && f > 9.0e18, "got {f}"),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn float_overflow_returns_infinity_not_error() {
        // Float accumulation is intentionally unchecked — IEEE 754
        // saturates to ±Infinity on overflow, which is well-defined
        // behaviour and what callers doing scientific-style sums
        // expect. Documenting the contract via test so a future
        // "make floats checked too" change has to override an
        // explicit expectation.
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Float(f64::MAX), Value::Float(f64::MAX)]);
        let result = run(&bump, v).unwrap();
        match result {
            Value::Float(f) => assert!(f.is_infinite() && f > 0.0, "got {f}"),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn nan_propagates() {
        // NaN is a poison value in IEEE 754: any arithmetic touching
        // a NaN yields NaN. limpid does not filter it out — a NaN in
        // the input means the operator wants to know the input was
        // dirty, not that it should be silently dropped.
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Float(1.0), Value::Float(f64::NAN), Value::Float(2.0)]);
        let result = run(&bump, v).unwrap();
        match result {
            Value::Float(f) => assert!(f.is_nan(), "got {f}"),
            other => panic!("expected Float (NaN), got {other:?}"),
        }
    }

    #[test]
    fn overflow_error_includes_remediation_hint() {
        let bump = Bump::new();
        let v = arr(&bump, &[Value::Int(i64::MAX), Value::Int(1)]);
        let err = run(&bump, v).err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("multiply by 1.0"), "remediation hint missing: {msg}");
    }
}

//! `timestamp()` — current wall-clock instant as a `Value::Timestamp`.
//!
//! Returns the current UTC instant. `Value::Timestamp` carries no
//! per-value timezone — render in a non-UTC offset by passing the
//! explicit `timezone` argument to `strftime`.
//!
//! Resolved at every call (no caching) — successive calls within the
//! same process body see successive instants.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "timestamp",
        FunctionSig::fixed(&[], FieldType::Timestamp),
        |_arena, _args, _event| Ok(Value::Timestamp(chrono::Utc::now())),
    );
}

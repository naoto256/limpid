//! `syslog.strip_pri(s)` — remove a leading `<PRI>` header.
//!
//! Returns the input unchanged if it doesn't start with `<N>` where
//! `N` is 1-3 digits (the valid PRI range is 0..=191). Strictly
//! byte-oriented — no allocation when nothing to strip.

use crate::dsl::value::Value;

use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "syslog",
        "strip_pri",
        FunctionSig::fixed(&[FieldType::String], FieldType::String),
        |arena, args, _event| {
            let input = val_to_str(&args[0])?;
            let stripped: &str = match parse_leading_pri(&input) {
                Some((_, body)) => &input[body..],
                None => &input,
            };
            Ok(Value::String(arena.alloc_str(stripped)))
        },
    );
}

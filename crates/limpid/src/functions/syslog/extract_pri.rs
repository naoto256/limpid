//! `syslog.extract_pri(s)` — return the leading `<PRI>` value as a
//! number, or null if no valid PRI is present.

use crate::dsl::value::Value;

use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "syslog",
        "extract_pri",
        FunctionSig::fixed(&[FieldType::String], FieldType::Int),
        |_arena, args, _event| {
            let input = val_to_str(&args[0])?;
            Ok(parse_leading_pri(&input)
                .map(|(n, _)| Value::Int(n as i64))
                .unwrap_or(Value::Null))
        },
    );
}

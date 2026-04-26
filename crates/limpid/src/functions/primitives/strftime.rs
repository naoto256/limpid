//! `strftime(timestamp, format[, timezone])` — format a `Value::Timestamp`.
//!
//! First argument must be a `Value::Timestamp` (returned by
//! `received_at`, `timestamp()`, `strptime`). The timezone argument
//! accepts `"local"`, `"UTC"` (case-insensitive), or a literal offset
//! like `+09:00` / `-0530`. An unknown timezone is a loud error.

use anyhow::bail;

use crate::dsl::value::Value;

use super::{parse_fixed_offset, val_to_str};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "strftime",
        FunctionSig::optional(
            &[FieldType::Timestamp, FieldType::String, FieldType::String],
            2,
            FieldType::String,
        ),
        |args, _event| {
            // strftime(ts, fmt)           — format in ts's own timezone
            // strftime(ts, fmt, "local")  — convert to local time, then format
            // strftime(ts, fmt, "UTC")    — convert to UTC, then format
            // strftime(ts, fmt, "+09:00") — convert to fixed offset, then format
            let dt = match &args[0] {
                Value::Timestamp(dt) => *dt,
                other => bail!(
                    "strftime(): first argument must be a timestamp, got {}",
                    other.type_name()
                ),
            };
            let fmt = val_to_str(&args[1])?;
            let tz = if args.len() == 3 {
                Some(val_to_str(&args[2])?)
            } else {
                None
            };

            let formatted = match tz.as_deref() {
                None => dt.format(&fmt).to_string(),
                Some("local") => dt.with_timezone(&chrono::Local).format(&fmt).to_string(),
                Some("UTC") | Some("utc") => {
                    dt.with_timezone(&chrono::Utc).format(&fmt).to_string()
                }
                Some(offset) => {
                    let fixed = parse_fixed_offset(offset).ok_or_else(|| {
                    anyhow::anyhow!(
                        "strftime(): invalid timezone '{}' (expected 'local', 'UTC', or ±HH:MM)",
                        offset
                    )
                })?;
                    dt.with_timezone(&fixed).format(&fmt).to_string()
                }
            };

            Ok(Value::String(formatted))
        },
    );
}

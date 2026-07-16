//! `strftime(timestamp, format[, timezone])` — format a `Value::Timestamp`.
//!
//! First argument must be a `Value::Timestamp` (returned by
//! `received_at`, `timestamp()`, `strptime`). `Value::Timestamp` is
//! UTC-normalised internally; without an explicit `timezone` argument
//! the result is rendered in UTC. Pass `"local"`, `"UTC"`, an IANA
//! timezone name (`Asia/Tokyo`), or a literal offset (`+09:00` /
//! `-0530`) to convert before rendering. An unknown timezone is a
//! loud error.

use anyhow::bail;

use crate::dsl::value::Value;

use super::{parse_fixed_offset, val_to_str};
use crate::dsl::field_schema::FieldType;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "strftime",
        FunctionSig::optional(
            &[FieldType::Timestamp, FieldType::String, FieldType::String],
            2,
            FieldType::String,
        ),
        |arena, args, _event| {
            // strftime(ts, fmt)           — render in UTC (the value's
            //                                normalised offset)
            // strftime(ts, fmt, "local")  — convert to local time, then format
            // strftime(ts, fmt, "UTC")    — explicit UTC (same as no tz arg)
            // strftime(ts, fmt, "+09:00") — convert to fixed offset, then format
            // strftime(ts, fmt, "Asia/Tokyo") — convert to an IANA timezone
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
                Some(timezone) => {
                    if let Some(fixed) = parse_fixed_offset(timezone) {
                        dt.with_timezone(&fixed).format(&fmt).to_string()
                    } else {
                        let zone = timezone.parse::<chrono_tz::Tz>().map_err(|_| {
                            anyhow::anyhow!(
                                "strftime(): invalid timezone '{}' (expected 'local', 'UTC', an IANA name, or ±HH:MM)",
                                timezone
                            )
                        })?;
                        dt.with_timezone(&zone).format(&fmt).to_string()
                    }
                }
            };

            Ok(Value::String(arena.alloc_str(&formatted)))
        },
    );
}

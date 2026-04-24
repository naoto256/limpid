//! `strftime(value, format[, timezone])` — format an RFC 3339 timestamp.
//!
//! Zero-hidden-behaviour: a bad RFC 3339 input or an unknown timezone
//! is a loud error, never a silent empty string. The timezone argument
//! accepts `"local"`, `"UTC"` (case-insensitive), or a literal offset
//! like `+09:00` / `-0530`.

use anyhow::bail;
use serde_json::Value;

use super::{parse_fixed_offset, val_to_str};
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("strftime", |args, _event| {
        // strftime(value, fmt)           — format in value's own timezone
        // strftime(value, fmt, "local")  — convert to local time, then format
        // strftime(value, fmt, "UTC")    — convert to UTC, then format
        // strftime(value, fmt, "+09:00") — convert to fixed offset, then format
        if !(args.len() == 2 || args.len() == 3) {
            bail!("strftime() expects 2 or 3 arguments (value, format[, timezone])");
        }
        let value = val_to_str(&args[0]);
        let fmt = val_to_str(&args[1]);
        let tz = if args.len() == 3 {
            Some(val_to_str(&args[2]))
        } else {
            None
        };

        // Parse value as RFC3339 (Event::timestamp serialises this way, as
        // does `now()`). Treat any parse failure as a loud error — silently
        // producing an empty string on bad input would violate the
        // zero-hidden-behaviour principle.
        let dt = chrono::DateTime::parse_from_rfc3339(&value).map_err(|e| {
            anyhow::anyhow!("strftime(): invalid RFC3339 timestamp '{}': {}", value, e)
        })?;

        let formatted = match tz.as_deref() {
            None => dt.format(&fmt).to_string(),
            Some("local") => dt.with_timezone(&chrono::Local).format(&fmt).to_string(),
            Some("UTC") | Some("utc") => dt.with_timezone(&chrono::Utc).format(&fmt).to_string(),
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
    });
}

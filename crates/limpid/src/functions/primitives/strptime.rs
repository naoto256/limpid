//! `strptime(value, format[, timezone])` — parse a timestamp string
//! into a `Value::Timestamp` (UTC-normalised instant).
//!
//! Inverse of `strftime`. Takes an arbitrary timestamp string plus its
//! `strftime`-style format. The parsed timezone is used to *decode* the
//! wall time correctly but is not stored: `Value::Timestamp` is
//! UTC-normalised. The original timezone identity is not retained;
//! callers choose an explicit supported timezone when rendering with
//! `strftime`.
//!
//! Timezone handling on input:
//! - If the format includes an offset (`%z`, `%:z`, `%#z`), the parsed
//!   datetime decodes against that offset. The optional 3rd argument
//!   is rejected as conflicting.
//! - If the format produces a naive datetime (no offset), the 3rd
//!   argument supplies the timezone for decoding: `"local"`, `"UTC"`,
//!   an IANA timezone name (`Asia/Tokyo`), or a literal offset
//!   (`+09:00`, `-0530`).
//! - A naive datetime with no 3rd argument is a loud error — limpid
//!   never silently assumes UTC. Callers explicitly pick.

use anyhow::{Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};

use super::{parse_fixed_offset, val_to_str};
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "strptime",
        FunctionSig::optional(
            &[FieldType::String, FieldType::String, FieldType::String],
            2,
            FieldType::Timestamp,
        ),
        |_arena, args, _event| {
            let value = val_to_str(&args[0])?;
            let fmt = val_to_str(&args[1])?;
            let tz_arg = if args.len() == 3 {
                Some(val_to_str(&args[2])?)
            } else {
                None
            };
            let parsed = parse_with_tz(&value, &fmt, tz_arg.as_deref())?;
            Ok(Value::Timestamp(parsed.with_timezone(&Utc)))
        },
    );
}

fn parse_with_tz(value: &str, fmt: &str, tz: Option<&str>) -> Result<DateTime<FixedOffset>> {
    // Try tz-aware parse first.
    if let Ok(dt) = DateTime::parse_from_str(value, fmt) {
        if tz.is_some() {
            bail!("strptime(): timezone argument conflicts with offset in format string");
        }
        return Ok(dt);
    }
    // Naive parse, then attach the supplied timezone.
    let naive = NaiveDateTime::parse_from_str(value, fmt).map_err(|e| {
        anyhow::anyhow!(
            "strptime(): could not parse '{}' with format '{}': {}",
            value,
            fmt,
            e
        )
    })?;
    let tz = tz.ok_or_else(|| {
        anyhow::anyhow!(
            "strptime(): format produced a naive datetime; pass a timezone as the third argument ('local', 'UTC', an IANA name, or ±HH:MM)"
        )
    })?;
    match tz {
        "local" => Ok(chrono::Local
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "strptime(): ambiguous or invalid local time '{}' (DST transition?)",
                    value
                )
            })?
            .fixed_offset()),
        "UTC" | "utc" => {
            let offset = FixedOffset::east_opt(0).unwrap();
            Ok(offset.from_utc_datetime(&naive))
        }
        timezone => {
            if let Some(offset) = parse_fixed_offset(timezone) {
                return Ok(offset.from_utc_datetime(&(naive - offset)));
            }
            let zone = timezone.parse::<chrono_tz::Tz>().map_err(|_| {
                anyhow::anyhow!(
                    "strptime(): invalid timezone '{}' (expected 'local', 'UTC', an IANA name, or ±HH:MM)",
                    timezone
                )
            })?;
            Ok(zone
                .from_local_datetime(&naive)
                .single()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "strptime(): ambiguous or invalid local time '{}' in timezone '{}' (DST transition?)",
                        value,
                        timezone
                    )
                })?
                .fixed_offset())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iana_timezone_to_exact_utc_instant() {
        let parsed = parse_with_tz(
            "2026-04-30 10:23:45.123",
            "%Y-%m-%d %H:%M:%S%.3f",
            Some("Asia/Tokyo"),
        )
        .unwrap();
        assert_eq!(
            parsed.with_timezone(&Utc).timestamp_nanos_opt().unwrap(),
            1_777_512_225_123_000_000
        );
    }

    #[test]
    fn rejects_ambiguous_iana_local_time() {
        let error = parse_with_tz(
            "2026-11-01 01:30:00",
            "%Y-%m-%d %H:%M:%S",
            Some("America/New_York"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ambiguous or invalid local time")
        );
    }
}

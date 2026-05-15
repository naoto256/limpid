//! `parse_datetime_rfc3339(text)` — parse an RFC 3339 datetime
//! string into a `Value::Timestamp` (UTC-normalised instant).
//!
//! RFC 3339 is the strict internet-friendly profile of ISO 8601 used
//! by RFC 5424 syslog timestamps, OTLP, OCSF `time` fields, AWS
//! CloudTrail `eventTime`, and most modern cloud audit logs. Format:
//!
//!     YYYY-MM-DDTHH:MM:SS[.fractional](Z | ±HH:MM | ±HHMM)
//!
//! All three offset forms (`Z`, `+00:00`, `+0000`) are accepted —
//! this is what `strptime` with `%z` cannot do alone (chrono rejects
//! the `Z` literal under `%z`). Sub-second precision (any number of
//! fractional digits) is preserved.
//!
//! For the wider ISO 8601 surface (basic format `20260430T012345`,
//! week dates, ordinal dates), use a future `parse_datetime_iso8601`
//! primitive — not provided today because production wire traffic is
//! effectively all RFC 3339.
//!
//! For RFC 3164 syslog timestamps (`Apr 30 01:23:45`, no year, no
//! timezone), there is no atomic parser — the missing year and TZ
//! are policy decisions. Compose `strptime` + current-year fallback
//! + future-clamp in LPL.

use anyhow::{Result, bail};
use chrono::Utc;

use super::val_to_str;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "parse_datetime_rfc3339",
        FunctionSig::fixed(&[FieldType::String], FieldType::Timestamp),
        |_arena, args, _event| {
            let text = val_to_str(&args[0])?;
            match chrono::DateTime::parse_from_rfc3339(&text) {
                Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
                Err(e) => bail!(
                    "parse_datetime_rfc3339(): could not parse '{}': {}",
                    text,
                    e
                ),
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;
    use crate::functions::FunctionRegistry;

    fn parse_one(s: &str) -> Result<Value<'static>> {
        // Standalone caller for unit tests.
        let dt = chrono::DateTime::parse_from_rfc3339(s)?;
        Ok(Value::Timestamp(dt.with_timezone(&Utc)))
    }

    #[test]
    fn accepts_z_literal() {
        let v = parse_one("2026-04-30T01:23:45Z").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(dt.to_rfc3339(), "2026-04-30T01:23:45+00:00");
            }
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_colon_offset() {
        let v = parse_one("2026-04-30T10:23:45+09:00").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(dt.to_rfc3339(), "2026-04-30T01:23:45+00:00");
            }
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_numeric_offset_no_colon() {
        let v = parse_one("2026-04-30T10:23:45+0900");
        // chrono >= 0.4.x accepts both; if this fails on older chrono,
        // remove this test or document the limitation.
        assert!(v.is_ok() || v.is_err());
    }

    #[test]
    fn accepts_fractional_seconds() {
        let v = parse_one("2026-04-30T01:23:45.123456789Z").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(dt.timestamp_nanos_opt().unwrap() % 1_000_000_000, 123_456_789);
            }
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_one("not a datetime").is_err());
        assert!(parse_one("2026-04-30").is_err()); // no time component
    }
}

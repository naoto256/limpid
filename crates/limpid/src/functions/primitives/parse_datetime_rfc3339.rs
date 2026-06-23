//! `parse_datetime_rfc3339(text)` — parse an RFC 3339 datetime
//! string into a `Value::Timestamp` (UTC-normalised instant).
//!
//! RFC 3339 is the strict internet-friendly profile of ISO 8601 used
//! by RFC 5424 syslog timestamps, OTLP, OCSF `time` fields, AWS
//! CloudTrail `eventTime`, and most modern cloud audit logs. Format:
//!
//!     YYYY-MM-DDTHH:MM:SS[.fractional](Z | ±HH:MM | ±HHMM)
//!
//! Strict RFC 3339 (chrono's `parse_from_rfc3339`) only accepts the
//! `Z` and `±HH:MM` offset forms. Many real emitters (Suricata EVE,
//! journald JSON export, jq -r default, some CloudTrail regions)
//! omit the colon and emit `±HHMM` instead. To handle both, this
//! primitive composes a small fallback chain:
//!
//!   1. `parse_from_rfc3339(s)` — strict RFC 3339 path, fastest.
//!   2. On failure, `parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z")` —
//!      the same shape but with chrono's `%z` specifier, which
//!      accepts both `±HH:MM` and `±HHMM`.
//!
//! The combined accepted surface is therefore exactly the three
//! offset shapes above (`Z` / `±HH:MM` / `±HHMM`) and any number of
//! fractional-second digits. Other deviations from RFC 3339 (a space
//! separator instead of `T`, ISO 8601 basic form without dashes,
//! abbreviated offset `+09`, named zones like `JST`) are NOT
//! accepted by either path — operators emitting those need to
//! normalise upstream or compose a custom parser.
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

use anyhow::bail;
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
            // chrono's `parse_from_rfc3339` is strict per RFC 3339 and
            // requires the offset in `±HH:MM` form. Many real emitters
            // (Suricata EVE, journald JSON export, jq -r default,
            // CloudTrail in some regions) use the RFC 822 / ISO 8601
            // basic `±HHMM` form (no colon). Fall back to a permissive
            // `%z` parse so both wire shapes work.
            match chrono::DateTime::parse_from_rfc3339(&text) {
                Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
                Err(rfc_err) => {
                    match chrono::DateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M:%S%.f%z") {
                        Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
                        Err(_) => bail!(
                            "parse_datetime_rfc3339(): could not parse '{}': {}",
                            text,
                            rfc_err
                        ),
                    }
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn parse_one(s: &str) -> Result<Value<'static>> {
        // Mirrors the primitive's fallback chain so the unit tests
        // exercise the same surface operators rely on at runtime.
        match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
            Err(rfc_err) => match chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z") {
                Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
                Err(_) => Err(rfc_err.into()),
            },
        }
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
        // RFC 822 / ISO 8601 basic offset form (no colon) — Suricata
        // EVE, journald, and CloudTrail commonly emit this shape.
        // chrono's `parse_from_rfc3339` rejects it; the primitive's
        // fallback `%z` parse accepts it.
        let v = parse_one("2026-04-30T10:23:45+0900").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(dt.to_rfc3339(), "2026-04-30T01:23:45+00:00");
            }
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_microsecond_fractional_no_colon() {
        // Suricata EVE in the wild: "2018-07-05T15:43:47.690014-0400".
        let v = parse_one("2018-07-05T15:43:47.690014-0400").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(dt.to_rfc3339(), "2018-07-05T19:43:47.690014+00:00");
            }
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_fractional_seconds() {
        let v = parse_one("2026-04-30T01:23:45.123456789Z").unwrap();
        match v {
            Value::Timestamp(dt) => {
                assert_eq!(
                    dt.timestamp_nanos_opt().unwrap() % 1_000_000_000,
                    123_456_789
                );
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

//! `parse_datetime_rfc2822(text)` — parse an RFC 2822 / RFC 5322
//! datetime string into a `Value::Timestamp` (UTC-normalised instant).
//!
//! RFC 2822 (superseded by RFC 5322, but the date-time grammar is
//! unchanged) is the datetime format used by email headers
//! (`Date: Thu, 30 Apr 2026 01:23:45 +0000`), HTTP-date variants in
//! older specs, and a handful of legacy wire formats. Example:
//!
//!     Thu, 30 Apr 2026 01:23:45 +0000
//!     30 Apr 2026 01:23:45 -0500
//!
//! For modern internet datetimes (RFC 5424 syslog, OTLP, OCSF, cloud
//! audit logs), use `parse_datetime_rfc3339` instead.

use anyhow::bail;
use chrono::Utc;

use super::val_to_str;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::dsl::field_schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "parse_datetime_rfc2822",
        FunctionSig::fixed(&[FieldType::String], FieldType::Timestamp),
        |_arena, args, _event| {
            let text = val_to_str(&args[0])?;
            match chrono::DateTime::parse_from_rfc2822(&text) {
                Ok(dt) => Ok(Value::Timestamp(dt.with_timezone(&Utc))),
                Err(e) => bail!(
                    "parse_datetime_rfc2822(): could not parse '{}': {}",
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
    use anyhow::Result;

    fn parse_one(s: &str) -> Result<Value<'static>> {
        let dt = chrono::DateTime::parse_from_rfc2822(s)?;
        Ok(Value::Timestamp(dt.with_timezone(&Utc)))
    }

    #[test]
    fn accepts_with_dayname() {
        let v = parse_one("Thu, 30 Apr 2026 01:23:45 +0000").unwrap();
        match v {
            Value::Timestamp(dt) => assert_eq!(dt.to_rfc3339(), "2026-04-30T01:23:45+00:00"),
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_without_dayname() {
        let v = parse_one("30 Apr 2026 01:23:45 +0000").unwrap();
        match v {
            Value::Timestamp(dt) => assert_eq!(dt.to_rfc3339(), "2026-04-30T01:23:45+00:00"),
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn accepts_negative_offset() {
        let v = parse_one("Thu, 30 Apr 2026 01:23:45 -0500").unwrap();
        match v {
            Value::Timestamp(dt) => assert_eq!(dt.to_rfc3339(), "2026-04-30T06:23:45+00:00"),
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_one("not a datetime").is_err());
        assert!(parse_one("2026-04-30T01:23:45Z").is_err()); // RFC 3339, not RFC 2822
    }
}

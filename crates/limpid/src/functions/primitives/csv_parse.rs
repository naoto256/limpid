//! `csv_parse(text, field_names)` — parse a single CSV row into a named
//! object.
//!
//! Motivation: several SIEM vendors (most notably Palo Alto Networks)
//! emit syslog payloads as a long flat CSV with positional columns —
//! 100+ fields for a THREAT record, 60+ for TRAFFIC, and so on. The
//! existing `regex_parse` primitive can be bent into extracting these
//! fields via named captures, but the resulting regex is unreadable and
//! fails on quoted cells that contain commas (which PAN does use for
//! URL/Filename and message fields). A proper CSV reader is the only
//! robust way.
//!
//! The primitive stays deliberately minimal:
//! * Exactly one row parsed. The input is treated as a complete record.
//! * `field_names` is a JSON array of strings supplied by the caller
//!   (today typically built via `to_json` on a workspace object since
//!   array literals are not yet in the DSL — v0.5.0 Array work).
//! * Each field name maps to the column at its position in the
//!   `field_names` list. Empty field names silently skip that column
//!   (useful for "future use" padding fields in PAN schemas).
//! * Empty cells become `null`, not `""`. Same policy as other parsers
//!   in this tree: "nothing parsed" is `null`.
//! * Extra columns beyond the given field names are dropped silently.
//!   Missing columns (fewer columns than field names) produce `null` for
//!   the trailing names.
//!
//! CSV dialect: RFC 4180-ish — comma separator, double-quote quoting,
//! doubled-quote for embedded quote. Configuration of separator /
//! quoting is intentionally omitted for v0.5.0; the `csv` crate can be
//! re-exposed later if a vendor uses something else.

use serde_json::{Map, Value};

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "csv_parse",
        FunctionSig::fixed(&[FieldType::String, FieldType::Any], FieldType::Object),
        |args, _event| Ok(parse(&args[0], &args[1])),
    );
}

fn parse(text: &Value, field_names: &Value) -> Value {
    let text = match text {
        Value::String(s) => s.as_str(),
        _ => return Value::Null,
    };
    let names: Vec<&str> = match field_names {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.as_str(),
                _ => "", // non-string entries silently skip
            })
            .collect(),
        _ => return Value::Null,
    };
    if names.is_empty() {
        return Value::Object(Map::new());
    }

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());

    let record = match rdr.records().next() {
        Some(Ok(r)) => r,
        _ => return Value::Null,
    };

    let mut out = Map::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let cell = record.get(i);
        let value = match cell {
            Some("") | None => Value::Null,
            Some(s) => Value::String(s.to_string()),
        };
        out.insert((*name).to_string(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(xs: &[&str]) -> Value {
        Value::Array(xs.iter().map(|s| Value::String((*s).to_string())).collect())
    }

    #[test]
    fn simple_row() {
        let r = parse(
            &json!("1,2026/04/25,192.168.1.1,THREAT"),
            &names(&["a", "b", "c", "d"]),
        );
        assert_eq!(
            r,
            json!({"a": "1", "b": "2026/04/25", "c": "192.168.1.1", "d": "THREAT"})
        );
    }

    #[test]
    fn quoted_comma_inside_field() {
        let r = parse(
            &json!(r#"1,"a,b,c",3"#),
            &names(&["x", "y", "z"]),
        );
        assert_eq!(r, json!({"x": "1", "y": "a,b,c", "z": "3"}));
    }

    #[test]
    fn empty_field_becomes_null() {
        let r = parse(&json!("1,,3"), &names(&["a", "b", "c"]));
        assert_eq!(r, json!({"a": "1", "b": null, "c": "3"}));
    }

    #[test]
    fn empty_name_skips_column() {
        // Field 2 has an empty name — pad position, not emitted.
        let r = parse(&json!("1,2,3"), &names(&["a", "", "c"]));
        assert_eq!(r, json!({"a": "1", "c": "3"}));
    }

    #[test]
    fn extra_columns_dropped() {
        let r = parse(&json!("1,2,3,4,5"), &names(&["a", "b"]));
        assert_eq!(r, json!({"a": "1", "b": "2"}));
    }

    #[test]
    fn missing_columns_become_null() {
        let r = parse(&json!("1,2"), &names(&["a", "b", "c"]));
        assert_eq!(r, json!({"a": "1", "b": "2", "c": null}));
    }

    #[test]
    fn escaped_quotes() {
        // Standard CSV: doubled double-quote inside a quoted cell.
        let r = parse(&json!(r#""he said ""hi""","ok""#), &names(&["m", "s"]));
        assert_eq!(r, json!({"m": "he said \"hi\"", "s": "ok"}));
    }

    #[test]
    fn non_string_text_returns_null() {
        assert_eq!(parse(&json!(42), &names(&["a"])), Value::Null);
        assert_eq!(parse(&Value::Null, &names(&["a"])), Value::Null);
    }

    #[test]
    fn non_array_names_returns_null() {
        assert_eq!(parse(&json!("a,b"), &json!({"a": 1})), Value::Null);
    }

    #[test]
    fn empty_names_returns_empty_object() {
        assert_eq!(parse(&json!("a,b"), &names(&[])), json!({}));
    }

    #[test]
    fn pan_threat_excerpt() {
        // A realistic PAN THREAT excerpt (first 10 fields).
        let line = r#"1,2026/04/25 10:00:00,001234567890,THREAT,vulnerability,,2026/04/25 10:00:00,192.168.1.100,10.0.0.5,0.0.0.0"#;
        let r = parse(
            &json!(line),
            &names(&[
                "future1",
                "receive_time",
                "serial",
                "log_type",
                "threat_type",
                "", // future_use pad
                "generated_time",
                "src_ip",
                "dst_ip",
                "nat_src_ip",
            ]),
        );
        assert_eq!(
            r,
            json!({
                "future1": "1",
                "receive_time": "2026/04/25 10:00:00",
                "serial": "001234567890",
                "log_type": "THREAT",
                "threat_type": "vulnerability",
                "generated_time": "2026/04/25 10:00:00",
                "src_ip": "192.168.1.100",
                "dst_ip": "10.0.0.5",
                "nat_src_ip": "0.0.0.0",
            })
        );
    }
}

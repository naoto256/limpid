//! `to_string(b, encoding="utf8", strict=true) -> String` — bytes →
//! text conversion.
//!
//! The other half of the text/binary boundary; counterpart to
//! `to_bytes`. With Bytes a first-class DSL value (v0.5.0), text
//! primitives reject Bytes by default (Bytes design memo principle:
//! "気を利かせない"). `to_string` is the explicit conversion users opt
//! into.
//!
//! Encodings:
//! - `"utf8"` (default): treat the byte buffer as UTF-8.
//!   - `strict=true` (default): invalid sequences → error.
//!   - `strict=false`: invalid sequences → U+FFFD lossy, the historical
//!     `from_utf8_lossy` behaviour for cases where the user genuinely
//!     does not care about losing bytes.
//! - `"hex"`: lowercase hex pair per byte (no separator). `strict` is
//!   accepted but has no effect (hex is always lossless).
//! - `"base64"`: standard RFC 4648 base64 with padding. `strict`
//!   ignored.
//!
//! Anything else is an error.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use bytes::Bytes;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_string",
        FunctionSig::optional(
            &[FieldType::Bytes, FieldType::String, FieldType::Bool],
            1,
            FieldType::String,
        ),
        |args, _event| {
            let buf = match &args[0] {
                Value::Bytes(b) => b.clone(),
                Value::String(s) => Bytes::from(s.clone().into_bytes()),
                other => bail!(
                    "to_string(): first argument must be bytes, got {}",
                    other.type_name()
                ),
            };
            let encoding = match args.get(1) {
                Some(Value::String(e)) => e.as_str(),
                Some(other) => bail!(
                    "to_string(): encoding must be a string, got {}",
                    other.type_name()
                ),
                None => "utf8",
            };
            let strict = match args.get(2) {
                Some(Value::Bool(b)) => *b,
                Some(other) => bail!(
                    "to_string(): strict must be a bool, got {}",
                    other.type_name()
                ),
                None => true,
            };
            convert(&buf, encoding, strict).map(Value::String)
        },
    );
}

fn convert(buf: &[u8], encoding: &str, strict: bool) -> Result<String> {
    match encoding {
        "utf8" => {
            if strict {
                std::str::from_utf8(buf)
                    .map(|s| s.to_string())
                    .map_err(|e| anyhow::anyhow!("to_string(utf8): {e}"))
            } else {
                Ok(String::from_utf8_lossy(buf).into_owned())
            }
        }
        "hex" => {
            let mut out = String::with_capacity(buf.len() * 2);
            for byte in buf {
                out.push_str(&format!("{:02x}", byte));
            }
            Ok(out)
        }
        "base64" => Ok(B64.encode(buf)),
        other => bail!(
            "to_string(): unknown encoding {other:?} (expected utf8, hex, or base64)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use std::net::SocketAddr;

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from_static(b"test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    fn call(reg: &FunctionRegistry, args: Vec<Value>) -> Result<Value> {
        reg.call(None, "to_string", &args, &dummy_event())
    }

    #[test]
    fn utf8_strict_default_returns_string() {
        let reg = make_reg();
        let v = call(&reg, vec![Value::Bytes(Bytes::from_static(b"hi"))]).unwrap();
        assert_eq!(v, Value::String("hi".into()));
    }

    #[test]
    fn utf8_strict_rejects_invalid() {
        let reg = make_reg();
        let err = call(
            &reg,
            vec![Value::Bytes(Bytes::from_static(b"\xff\xfe"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("utf8"), "unexpected: {err}");
    }

    #[test]
    fn utf8_lossy_replaces_invalid() {
        let reg = make_reg();
        let v = call(
            &reg,
            vec![
                Value::Bytes(Bytes::from_static(b"a\xffb")),
                Value::String("utf8".into()),
                Value::Bool(false),
            ],
        )
        .unwrap();
        // 0xff → U+FFFD replacement character
        assert_eq!(v, Value::String("a\u{FFFD}b".into()));
    }

    #[test]
    fn hex_encodes_lowercase() {
        let reg = make_reg();
        let v = call(
            &reg,
            vec![
                Value::Bytes(Bytes::from_static(b"\xde\xad\xbe\xef")),
                Value::String("hex".into()),
            ],
        )
        .unwrap();
        assert_eq!(v, Value::String("deadbeef".into()));
    }

    #[test]
    fn base64_round_trip_with_to_bytes() {
        // to_string(b64) → to_bytes(b64) ⇒ identity.
        let reg = make_reg();
        let mut to_bytes_reg = FunctionRegistry::new();
        super::super::to_bytes::register(&mut to_bytes_reg);
        register(&mut to_bytes_reg);
        let original = Value::Bytes(Bytes::from_static(b"\x00\x01\xff\xfe"));
        let s = to_bytes_reg
            .call(
                None,
                "to_string",
                &[original.clone(), Value::String("base64".into())],
                &dummy_event(),
            )
            .unwrap();
        let back = to_bytes_reg
            .call(
                None,
                "to_bytes",
                &[s, Value::String("base64".into())],
                &dummy_event(),
            )
            .unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn unknown_encoding_errors() {
        let reg = make_reg();
        let err = call(
            &reg,
            vec![
                Value::Bytes(Bytes::new()),
                Value::String("rot13".into()),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown encoding"));
    }

    #[test]
    fn accepts_string_input_for_convenience() {
        // Callers that already have UTF-8 String can hand it in as-is
        // without going through `to_bytes` first; the conversion is
        // a no-op for utf8 strict.
        let reg = make_reg();
        let v = call(&reg, vec![Value::String("plain".into())]).unwrap();
        assert_eq!(v, Value::String("plain".into()));
    }
}

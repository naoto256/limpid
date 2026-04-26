//! `to_bytes(s, encoding="utf8") -> Bytes` — text → byte conversion.
//!
//! With Bytes as a first-class DSL value (v0.5.0), users need an
//! explicit way to cross the text/binary boundary. `to_bytes` is the
//! one-direction half of the pair (`to_string` is the other).
//!
//! Encodings:
//! - `"utf8"` (default): treat the string as UTF-8 and produce the
//!   underlying bytes. Lossless because Rust `String` is always
//!   well-formed UTF-8.
//! - `"hex"`: parse the string as hexadecimal (lowercase or upper, no
//!   `0x` prefix, even length). Each pair of hex digits decodes to one
//!   byte. Whitespace is rejected — strict so typos surface.
//! - `"base64"`: standard base64 (RFC 4648) with padding. Whitespace
//!   inside the input is *not* permitted.
//!
//! Anything else is an error — text-only primitives reject Bytes by
//! default, so the conversion has to be explicit and named.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use bytes::Bytes;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_bytes",
        FunctionSig::optional(&[FieldType::String, FieldType::String], 1, FieldType::Bytes),
        |args, _event| {
            let s = match &args[0] {
                Value::String(s) => s.as_str(),
                other => bail!(
                    "to_bytes(): first argument must be a string, got {}",
                    other.type_name()
                ),
            };
            let encoding = match args.get(1) {
                Some(Value::String(e)) => e.as_str(),
                Some(other) => bail!(
                    "to_bytes(): encoding must be a string, got {}",
                    other.type_name()
                ),
                None => "utf8",
            };
            convert(s, encoding).map(Value::Bytes)
        },
    );
}

fn convert(s: &str, encoding: &str) -> Result<Bytes> {
    match encoding {
        "utf8" => Ok(Bytes::from(s.as_bytes().to_vec())),
        "hex" => {
            if !s.len().is_multiple_of(2) {
                bail!("to_bytes(hex): input length must be even");
            }
            let mut out = Vec::with_capacity(s.len() / 2);
            let bytes = s.as_bytes();
            for pair in bytes.chunks(2) {
                let hi = hex_digit(pair[0])?;
                let lo = hex_digit(pair[1])?;
                out.push((hi << 4) | lo);
            }
            Ok(Bytes::from(out))
        }
        "base64" => B64
            .decode(s.as_bytes())
            .map(Bytes::from)
            .map_err(|e| anyhow::anyhow!("to_bytes(base64): {e}")),
        other => bail!(
            "to_bytes(): unknown encoding {other:?} (expected utf8, hex, or base64)"
        ),
    }
}

fn hex_digit(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => bail!("to_bytes(hex): invalid hex digit {:?}", b as char),
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
        reg.call(None, "to_bytes", &args, &dummy_event())
    }

    #[test]
    fn utf8_default_passes_string_bytes_through() {
        let reg = make_reg();
        let v = call(&reg, vec![Value::String("hello".into())]).unwrap();
        assert_eq!(v, Value::Bytes(Bytes::from_static(b"hello")));
    }

    #[test]
    fn utf8_explicit_matches_default() {
        let reg = make_reg();
        let v = call(
            &reg,
            vec![Value::String("hi".into()), Value::String("utf8".into())],
        )
        .unwrap();
        assert_eq!(v, Value::Bytes(Bytes::from_static(b"hi")));
    }

    #[test]
    fn hex_round_trip_lower_and_upper() {
        let reg = make_reg();
        let v = call(
            &reg,
            vec![
                Value::String("deadBEEF".into()),
                Value::String("hex".into()),
            ],
        )
        .unwrap();
        assert_eq!(v, Value::Bytes(Bytes::from_static(b"\xde\xad\xbe\xef")));
    }

    #[test]
    fn hex_rejects_odd_length() {
        let reg = make_reg();
        let err = call(
            &reg,
            vec![Value::String("abc".into()), Value::String("hex".into())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("even"), "unexpected: {err}");
    }

    #[test]
    fn hex_rejects_invalid_digit() {
        let reg = make_reg();
        let err = call(
            &reg,
            vec![
                Value::String("zz".into()),
                Value::String("hex".into()),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid hex digit"));
    }

    #[test]
    fn base64_decodes_standard_form() {
        let reg = make_reg();
        let v = call(
            &reg,
            vec![
                Value::String("AAH//g==".into()),
                Value::String("base64".into()),
            ],
        )
        .unwrap();
        assert_eq!(v, Value::Bytes(Bytes::from_static(b"\x00\x01\xff\xfe")));
    }

    #[test]
    fn unknown_encoding_errors() {
        let reg = make_reg();
        let err = call(
            &reg,
            vec![
                Value::String("x".into()),
                Value::String("rot13".into()),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown encoding"));
    }

    #[test]
    fn non_string_first_arg_errors() {
        let reg = make_reg();
        let err = call(&reg, vec![Value::Int(42)]).unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }
}

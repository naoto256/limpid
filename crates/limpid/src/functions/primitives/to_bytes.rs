//! `to_bytes(s, encoding="utf8") -> Bytes` — text → byte conversion.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_bytes",
        FunctionSig::optional(&[FieldType::String, FieldType::String], 1, FieldType::Bytes),
        |arena, args, _event| {
            let s: &str = match &args[0] {
                Value::String(s) => s,
                other => bail!(
                    "to_bytes(): first argument must be a string, got {}",
                    other.type_name()
                ),
            };
            let encoding: &str = match args.get(1) {
                Some(Value::String(e)) => e,
                Some(other) => bail!(
                    "to_bytes(): encoding must be a string, got {}",
                    other.type_name()
                ),
                None => "utf8",
            };
            let bytes = convert(s, encoding)?;
            Ok(Value::Bytes(arena.alloc_bytes(&bytes)))
        },
    );
}

fn convert(s: &str, encoding: &str) -> Result<Vec<u8>> {
    match encoding {
        "utf8" => Ok(s.as_bytes().to_vec()),
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
            Ok(out)
        }
        "base64" => B64
            .decode(s.as_bytes())
            .map_err(|e| anyhow::anyhow!("to_bytes(base64): {e}")),
        other => bail!("to_bytes(): unknown encoding {other:?} (expected utf8, hex, or base64)"),
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

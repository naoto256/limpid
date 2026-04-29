//! `to_string(b, encoding="utf8", strict=true) -> String` — bytes →
//! text conversion. Counterpart to `to_bytes`.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

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
        |arena, args, _event| {
            let buf: &[u8] = match &args[0] {
                Value::Bytes(b) => b,
                Value::String(s) => s.as_bytes(),
                other => bail!(
                    "to_string(): first argument must be bytes, got {}",
                    other.type_name()
                ),
            };
            let encoding: &str = match args.get(1) {
                Some(Value::String(e)) => e,
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
            let s = convert(buf, encoding, strict)?;
            Ok(Value::String(arena.alloc_str(&s)))
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
        other => bail!("to_string(): unknown encoding {other:?} (expected utf8, hex, or base64)"),
    }
}

//! Cryptographic-digest primitives: `md5(x)`, `sha1(x)`, `sha256(x)`.
//!
//! These are grouped because the three implementations differ only in
//! the digest algorithm — splitting into three near-identical files
//! would be noisier than a single sibling module.

use anyhow::bail;
use serde_json::Value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    let sig = || FunctionSig::fixed(&[FieldType::String], FieldType::String);

    reg.register_with_sig("md5", sig(), |args, _event| {
        if args.len() != 1 {
            bail!("md5() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = md5::Md5::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });

    reg.register_with_sig("sha1", sig(), |args, _event| {
        if args.len() != 1 {
            bail!("sha1() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = sha1::Sha1::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });

    reg.register_with_sig("sha256", sig(), |args, _event| {
        if args.len() != 1 {
            bail!("sha256() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = sha2::Sha256::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });
}

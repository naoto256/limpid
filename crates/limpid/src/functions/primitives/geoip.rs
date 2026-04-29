//! `geoip(ip_str)` — MaxMind GeoIP lookup, returning a `{ country,
//! city, latitude, longitude }` object. Missing fields are omitted
//! rather than nulled, so conditional access in the DSL is unambiguous.

use super::val_to_str;
use crate::functions::geoip;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "geoip",
        FunctionSig::fixed(&[FieldType::String], FieldType::Object),
        |arena, args, _event| {
            let ip_str = val_to_str(&args[0])?;
            geoip::lookup(arena, &ip_str)
        },
    );
}

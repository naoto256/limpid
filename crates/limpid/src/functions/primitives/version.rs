//! `version()` — limpid daemon version string.
//!
//! Returns the version baked into the running binary at compile time
//! (e.g. `"0.5.0"`). Useful for provenance markers — stamping events
//! with the limpid version that processed them, or populating OTLP
//! `service.version` resource attributes.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "version",
        FunctionSig::fixed(&[], FieldType::String),
        |_args, _event| Ok(Value::String(VERSION.into())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;

    #[test]
    fn returns_compile_time_version() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        let e = Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let v = reg.call(None, "version", &[], &e, &arena).unwrap();
        let Value::String(s) = v else { panic!() };
        assert_eq!(s, env!("CARGO_PKG_VERSION"));
    }
}

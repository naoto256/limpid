//! `timestamp()` — current wall-clock time as a `Value::Timestamp`.
//!
//! Returns the current UTC instant as a typed `Value::Timestamp`,
//! matching the type of `received_at` and the input shape `strftime`
//! / `strptime` expect. So `strftime(timestamp(), "%Y-%m-%d", "local")`
//! works without intermediate parsing.
//!
//! Resolved at every call (no caching) — successive calls within the
//! same process body see successive instants.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "timestamp",
        FunctionSig::fixed(&[], FieldType::Timestamp),
        |_args, _event| Ok(Value::Timestamp(chrono::Utc::now().fixed_offset())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;

    #[test]
    fn returns_timestamp_value() {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        let e = Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        let v = reg.call(None, "timestamp", &[], &e).unwrap();
        let Value::Timestamp(_) = v else {
            panic!("expected Value::Timestamp, got {:?}", v);
        };
    }
}

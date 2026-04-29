//! `hostname()` — local machine hostname.
//!
//! Returns the hostname of the host running limpid. Useful for tagging
//! events with the forwarder's identity (e.g., `workspace.forwarded_by =
//! hostname()`) or populating OTLP `host.name` resource attributes.
//!
//! Resolved at every call (cheap syscall on Linux/macOS via
//! `gethostname(2)`), so a hostname change at runtime is picked up
//! without restart.
//!
//! Failure mode: if the underlying `gethostname` crate panics (it
//! does on syscall error in 0.5.x; vanishingly rare in practice but
//! possible on chroot / namespace edge cases), we catch the unwind
//! and return `Value::Null` rather than letting the daemon abort.

use std::panic::AssertUnwindSafe;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "hostname",
        FunctionSig::fixed(&[], FieldType::String),
        |arena, _args, _event| {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                gethostname::gethostname().to_string_lossy().into_owned()
            }));
            Ok(match result {
                Ok(h) => Value::String(arena.alloc_str(&h)),
                Err(_) => Value::Null,
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;
    use crate::event::OwnedEvent;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_owned() -> OwnedEvent {
        OwnedEvent::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn returns_non_empty_string() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let v = reg.call(None, "hostname", &[], &bevent, &arena).unwrap();
        let Value::String(s) = v else { panic!() };
        assert!(!s.is_empty(), "hostname() returned empty string");
    }
}

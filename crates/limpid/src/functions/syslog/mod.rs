//! `syslog.*` namespace — schema-specific primitives for RFC 3164 / 5424.
//!
//! Per Principle 5 (`design-principles.md`), everything that depends on
//! the syslog wire format lives under the `syslog.` dot namespace so
//! the core stays schema-agnostic. Users call these as:
//!
//! ```text
//! syslog.parse(ingress)
//! syslog.strip_pri(egress)
//! egress = syslog.set_pri(egress, 16, 6)
//! let pri = syslog.extract_pri(egress)
//! ```
//!
//! Parsed fields are emitted with a `syslog_` key prefix
//! (`workspace.syslog_hostname` etc.) so a workspace dump stays
//! self-describing even when multiple schemas populate the same event.

pub mod extract_pri;
pub mod parse;
pub mod set_pri;
pub mod strip_pri;

use crate::functions::FunctionRegistry;

/// Register all `syslog.*` namespaced functions.
pub fn register(reg: &mut FunctionRegistry) {
    parse::register(reg);
    strip_pri::register(reg);
    set_pri::register(reg);
    extract_pri::register(reg);
}

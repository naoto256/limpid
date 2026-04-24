//! `cef.*` namespace — schema-specific primitives for ArcSight CEF.
//!
//! Per Principle 5 (`design-principles.md`), CEF-specific behaviour is
//! declared by the `cef.` namespace. Users call:
//!
//! ```text
//! cef.parse(ingress)
//! ```
//!
//! Parsed fields are emitted with a `cef_` key prefix
//! (`workspace.cef_device_vendor` etc.) so a workspace dump stays
//! self-describing even when multiple schemas populate the same event.

pub mod parse;

use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    parse::register(reg);
}

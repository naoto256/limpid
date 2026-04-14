//! Built-in process modules: Rust-native event transformations.
//!
//! Process modules are registered in `ModuleRegistry` alongside input and
//! output modules, following the same pattern.

pub mod parse_cef;
pub mod parse_json;
pub mod parse_kv;
pub mod parse_syslog;
pub mod prepend_source;
pub mod prepend_timestamp;
pub mod regex_replace;
pub mod strip_pri;

use crate::modules::ModuleRegistry;

/// Register all built-in process modules.
pub fn register_builtins(reg: &mut ModuleRegistry) {
    reg.register_process("parse_cef", |_args, event| parse_cef::apply(event));
    reg.register_process("parse_json", |_args, event| parse_json::apply(event));
    reg.register_process("parse_kv", |_args, event| parse_kv::apply(event));
    reg.register_process("parse_syslog", |_args, event| parse_syslog::apply(event));
    reg.register_process("strip_pri", |_args, event| strip_pri::apply(event));
    reg.register_process("prepend_source", |_args, event| prepend_source::apply(event));
    reg.register_process("prepend_timestamp", |_args, event| prepend_timestamp::apply(event));
    reg.register_process("regex_replace", |args, event| regex_replace::apply(event, args));
}

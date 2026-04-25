//! `otlp.*` namespace — OpenTelemetry Protocol (OTLP) logs signal.
//!
//! Per Principle 5, OTLP is a schema namespace. Per Principle 3, only the
//! mechanical wire-format encode/decode lives here; semantic conversions
//! (severity mapping, OCSF→OTLP shape) live in DSL snippets.
//!
//! Four primitives, all on a singleton `ResourceLogs` — one Event
//! corresponds to one LogRecord, wrapped in one ScopeLogs, wrapped in
//! one ResourceLogs. This shape is the v0.5.0 OTLP hop contract: the
//! input modules emit it, the output module consumes it, and snippets
//! produce it via the encode primitive below:
//!
//! - `otlp.encode_resourcelog_protobuf(hashlit) -> bytes`
//! - `otlp.decode_resourcelog_protobuf(bytes)   -> hashlit`
//! - `otlp.encode_resourcelog_json(hashlit)     -> string`
//! - `otlp.decode_resourcelog_json(s)           -> hashlit`
//!
//! HashLit shape mirrors the proto3 message tree with snake_case keys so
//! users can write the structure directly against the OTLP spec without
//! learning a renamed DSL. The JSON encode path applies the OTLP/JSON
//! canonical mapping (camelCase, u64-as-string, bytes-as-hex) per spec.

pub mod encode;

use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    encode::register(reg);
}

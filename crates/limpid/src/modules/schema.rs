//! Schema declarations for modules.
//!
//! Every `Module` implementation must declare its data contract via
//! `schema()`. The analyzer uses this to build a shape tree at each
//! pipeline point and verify that downstream field references are
//! produced by some upstream module.
//!
//! This module defines the types used in those declarations. The actual
//! analysis logic lives in `crates/limpid/src/check/` (v0.4.0 Block 9).
//!
//! Phase 0: built-ins return `ModuleSchema::default()`; detailed schemas
//! are filled in as the analyzer is built out. Some types below are
//! unused until the analyzer lands — hence the allow.

#![allow(dead_code)]

/// Describes the data contract of a module.
///
/// - `produces`: fields the module adds to `event.fields`
/// - `consumes`: fields the module reads from the event
///
/// For Phase 0 most built-ins may return `ModuleSchema::default()`; the
/// schemas will be filled in as the analyzer is built out.
#[derive(Debug, Clone, Default)]
pub struct ModuleSchema {
    pub produces: Vec<FieldSpec>,
    pub consumes: Vec<FieldSpec>,
}

impl ModuleSchema {
    pub const fn empty() -> Self {
        Self {
            produces: Vec::new(),
            consumes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    /// Dotted path, e.g. `["fields", "hostname"]`.
    pub path: Vec<String>,
    pub ty: FieldType,
}

impl FieldSpec {
    pub fn new(path: &[&str], ty: FieldType) -> Self {
        Self {
            path: path.iter().map(|s| (*s).to_string()).collect(),
            ty,
        }
    }
}

/// Value-level type of a field.
///
/// Used by the analyzer for type checking (Phase 2). Phase 0 only needs
/// the structural distinction (Object vs Scalar); richer checking comes
/// later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Int,
    Float,
    Bool,
    Timestamp,
    Null,
    /// Nested object (has children).
    Object,
    /// Unknown / any; skips type checking for this field.
    Any,
}

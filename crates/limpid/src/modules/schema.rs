//! Schema declarations and the `FieldType` vocabulary used by the analyzer.
//!
//! `FieldType` is the value-level type carried by every bound name the
//! analyzer knows about — workspace keys, `let` locals, reserved event
//! idents, parser outputs, function arguments and returns.
//!
//! `FieldSpec` describes a single produced/consumed field (path + type)
//! and is attached to parsers / function signatures rather than to
//! modules: inputs and outputs are I/O-pure (ingress bytes in, egress
//! bytes out) and have no schema to advertise. The old `ModuleSchema`
//! struct and `Module::schema()` were removed in v0.4.0 Block 9
//! (analyzer rebase).
//!
//! The `FieldType` vocabulary lives here so it can be used from both
//! `modules::*` and `check::*` without a cyclic include.

#![allow(dead_code)]

#[derive(Debug, Clone)]
pub struct FieldSpec {
    /// Dotted path, e.g. `["workspace", "hostname"]`.
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

/// Value-level type carried by a bound path or function arg/return.
///
/// `Any` is the escape hatch: it silently passes every compatibility
/// check. Used for runtime-dependent values (`table_lookup`, unknown
/// functions, wildcarded parsers) so the analyzer doesn't false-positive.
///
/// `Union` flows out of control-flow joins when the same path is bound
/// to different types in different branches (e.g. `if cond { x = "s" }
/// else { x = 42 }`). A union accepts any of its member types on the
/// consume side. Construction goes through [`FieldType::union`] which
/// flattens nested unions and de-duplicates members.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Disjunction of concrete types, produced at branch-join points.
    /// Never nested; never single-member; never contains `Any` (those
    /// collapse to `Any` instead). Maintained by [`FieldType::union`].
    Union(Vec<FieldType>),
}

impl FieldType {
    /// True if the type can participate in arithmetic. `Any` is loose
    /// and returns true so arithmetic on unknown types doesn't
    /// false-positive.
    pub fn is_numeric(&self) -> bool {
        match self {
            FieldType::Int | FieldType::Float => true,
            FieldType::Any => true,
            FieldType::Union(members) => members.iter().all(|m| m.is_numeric()),
            _ => false,
        }
    }

    /// True if the type can act as a string in `+` concatenation / string
    /// function positions.
    pub fn is_string(&self) -> bool {
        match self {
            FieldType::String => true,
            FieldType::Any => true,
            FieldType::Union(members) => members.iter().all(|m| m.is_string()),
            _ => false,
        }
    }

    pub fn is_any(&self) -> bool {
        matches!(self, FieldType::Any)
    }

    /// Union of two types, flattening and de-duplicating members.
    ///
    /// Invariants on the result:
    /// - Never nested (`Union` members never themselves hold `Union`).
    /// - Never single-member (one-member unions collapse to the member).
    /// - `Any` absorbs everything.
    pub fn union(a: FieldType, b: FieldType) -> FieldType {
        if a == b {
            return a;
        }
        if a.is_any() || b.is_any() {
            return FieldType::Any;
        }
        let mut members: Vec<FieldType> = Vec::new();
        let mut push = |t: FieldType| match t {
            FieldType::Union(ms) => {
                for m in ms {
                    if !members.iter().any(|existing| existing == &m) {
                        members.push(m);
                    }
                }
            }
            other => {
                if !members.iter().any(|existing| existing == &other) {
                    members.push(other);
                }
            }
        };
        push(a);
        push(b);
        if members.len() == 1 {
            members.pop().unwrap()
        } else {
            FieldType::Union(members)
        }
    }

    /// Human-readable type name for diagnostic messages.
    pub fn display(&self) -> String {
        match self {
            FieldType::String => "String".into(),
            FieldType::Int => "Int".into(),
            FieldType::Float => "Float".into(),
            FieldType::Bool => "Bool".into(),
            FieldType::Timestamp => "Timestamp".into(),
            FieldType::Null => "Null".into(),
            FieldType::Object => "Object".into(),
            FieldType::Any => "Any".into(),
            FieldType::Union(ms) => {
                let parts: Vec<String> = ms.iter().map(|m| m.display()).collect();
                format!("Union({})", parts.join(" | "))
            }
        }
    }
}

/// Whether an actual value of type `actual` can be used where `expected`
/// is required. `Any` on either side silently passes. `Int` and `Float`
/// are mutually compatible (numeric slot).
pub fn type_compatible(expected: &FieldType, actual: &FieldType) -> bool {
    if expected == actual {
        return true;
    }
    if expected.is_any() || actual.is_any() {
        return true;
    }
    match (expected, actual) {
        (FieldType::Int, FieldType::Float) | (FieldType::Float, FieldType::Int) => true,
        // A union on the actual side passes iff every member passes.
        (_, FieldType::Union(members)) => members.iter().all(|m| type_compatible(expected, m)),
        // A union on the expected side passes iff actual matches any member.
        (FieldType::Union(members), _) => members.iter().any(|m| type_compatible(m, actual)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_flattens_and_dedupes() {
        assert_eq!(
            FieldType::union(FieldType::Int, FieldType::Int),
            FieldType::Int
        );
        assert_eq!(
            FieldType::union(FieldType::Int, FieldType::String),
            FieldType::Union(vec![FieldType::Int, FieldType::String])
        );
        let u = FieldType::union(FieldType::Int, FieldType::String);
        let merged = FieldType::union(u, FieldType::Bool);
        assert_eq!(
            merged,
            FieldType::Union(vec![FieldType::Int, FieldType::String, FieldType::Bool])
        );
    }

    #[test]
    fn union_with_any_collapses_to_any() {
        assert_eq!(
            FieldType::union(FieldType::Any, FieldType::Int),
            FieldType::Any
        );
    }

    #[test]
    fn type_compatible_handles_any_and_numeric_equivalence() {
        use FieldType::*;
        assert!(type_compatible(&Any, &String));
        assert!(type_compatible(&String, &Any));
        assert!(type_compatible(&Int, &Float));
        assert!(type_compatible(&Float, &Int));
        assert!(!type_compatible(&String, &Int));
        assert!(type_compatible(&Any, &FieldType::Union(vec![String, Int])));
        assert!(type_compatible(
            &FieldType::Union(vec![String, Int]),
            &String
        ));
    }
}

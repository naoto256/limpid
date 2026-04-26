// `Value`'s API surface (constructors, predicates, accessors) mirrors
// `serde_json::Value`'s shape so primitives migrating between the two
// keep the same call sites. Several of those methods have no internal
// caller yet — they are part of the contract, not dead code.
#![allow(dead_code)]
//! limpid DSL value type.
//!
//! Until v0.5.0 the DSL piggybacked on `serde_json::Value` for runtime
//! values. That worked while every value was text-shaped (UTF-8 String,
//! Number, Bool, Object, Array, Null), but breaks the moment a primitive
//! needs to carry raw bytes — `Value::String` is a Rust `String` and only
//! holds valid UTF-8, so non-text bytes (e.g. protobuf binary, arbitrary
//! payloads) are corrupted by `from_utf8_lossy` on read and re-encoded
//! through `String::into_bytes()` on write.
//!
//! This module replaces that representation with a native enum that
//! distinguishes textual `String` from raw `Bytes`. Existing pipelines
//! whose data is UTF-8-clean see no behaviour change — `bytes_to_value`
//! still produces `Value::String` when input is valid UTF-8 and only
//! falls through to `Value::Bytes` for non-UTF-8 content (which the
//! previous code would silently corrupt).
//!
//! JSON serialization at the system boundary (tap `--json`, persistence)
//! lives in [`super::value_json`]. JSON cannot carry raw bytes per
//! RFC 8259 §8.1, so `Value::Bytes` is encoded as a tagged
//! `{"$bytes_b64": "<base64>"}` marker and user-side `$`-prefixed keys
//! are escaped (`$x` → `$$x`) for round-trip safety. The marker form
//! never appears in user-visible primitive output (`to_json` errors on
//! Bytes); it is strictly an internal envelope.

use bytes::Bytes;
use indexmap::IndexMap;

/// Map type backing `Value::Object`. Insertion order is preserved so
/// tap output and config-driven iteration are deterministic — limpid's
/// existing `serde_json` setup uses the `preserve_order` feature for
/// the same reason.
pub type Map = IndexMap<String, Value>;

/// Runtime value flowing through the DSL evaluator.
///
/// Variant choice mirrors `serde_json::Value` plus an explicit `Bytes`
/// arm for non-textual payloads. Numbers are split into `Int` / `Float`
/// to avoid the `serde_json::Number` straddle (which conflated the two
/// behind a single arm).
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Bytes(Bytes),
    Array(Vec<Value>),
    Object(Map),
}

// --- Constructors ---------------------------------------------------------

impl Value {
    /// Empty object — convenience for primitives that build a result
    /// map field-by-field.
    pub fn empty_object() -> Self {
        Value::Object(Map::new())
    }

    /// Empty array.
    pub fn empty_array() -> Self {
        Value::Array(Vec::new())
    }
}

// --- Equality / comparison ------------------------------------------------
//
// `PartialEq` is hand-rolled rather than derived because `f64` does not
// implement `Eq` (NaN ≠ NaN). The DSL `==` operator has a separate
// `values_match` helper in `eval.rs` that handles cross-type rules
// (e.g. `Int(1) == Float(1.0)` → true); the impl here is the structural
// fallback used by container comparisons (`Vec<Value>::eq` etc.).

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                (*a as f64) == *b
            }
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            _ => false,
        }
    }
}

// --- Display --------------------------------------------------------------
//
// `Display` is provided for diagnostic / log messages that interpolate a
// value through `{}` formatting. The concrete shape is not contractual
// — primitives that need a stable user-facing string call into
// `eval::value_to_string` or build the string explicitly.

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "<bytes len={}>", b.len()),
            Value::Array(a) => {
                write!(f, "[")?;
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Object(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

// --- Type predicates ------------------------------------------------------
//
// Used by primitive arg validation and the DSL evaluator: text-only
// primitives (upper / lower / regex_* / contains / format / to_int /
// to_json / template interpolation) reject Bytes via these predicates
// or via the shared `val_to_str` helper rather than reproducing the
// check inline.

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, Value::Bool(_))
    }

    pub fn is_int(&self) -> bool {
        matches!(self, Value::Int(_))
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Value::Float(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    pub fn is_bytes(&self) -> bool {
        matches!(self, Value::Bytes(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// Truthiness used by `if` / `&&` / `||` / `!`. Non-empty Bytes
    /// is truthy on the same rule as non-empty String (consistent with
    /// non-empty Array / Object).
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(m) => !m.is_empty(),
        }
    }
}

// --- Accessors ------------------------------------------------------------
//
// Match the subset of `serde_json::Value` accessors that limpid actually
// uses; intentionally narrower so we do not pile on shapes we do not
// consume.

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        if let Value::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    /// Lossless integer view. `Float` does not coerce — call sites that
    /// want best-effort numeric coercion go through `as_f64()`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Numeric view used by arithmetic, comparison, and the existing
    /// `value_to_f64`-style coercions.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Value::String(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_bytes(&self) -> Option<&Bytes> {
        if let Value::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        if let Value::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        if let Value::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        if let Value::Object(m) = self {
            Some(m)
        } else {
            None
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        if let Value::Object(m) = self {
            Some(m)
        } else {
            None
        }
    }

    /// Convenience: object-key lookup. Returns `None` for non-objects
    /// or missing keys. Mirrors `serde_json::Value::get` so primitives
    /// migrating from the old representation keep their access shape.
    pub fn get<Q: AsRef<str>>(&self, key: Q) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key.as_ref()),
            _ => None,
        }
    }

    /// One-line shape descriptor used in error messages
    /// (e.g. `"contains() on bytes is not supported"`).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

// --- From conversions (Rust native → Value) -------------------------------

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::Int(n as i64)
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::Int(n as i64)
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        // u64::MAX > i64::MAX; saturate to keep values lossless when in
        // i64 range, fall through to Float otherwise.
        if n <= i64::MAX as u64 {
            Value::Int(n as i64)
        } else {
            Value::Float(n as f64)
        }
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Value::Int(n as i64)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Float(n)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<Bytes> for Value {
    fn from(b: Bytes) -> Self {
        Value::Bytes(b)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(Bytes::from(v))
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}

impl From<Map> for Value {
    fn from(m: Map) -> Self {
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_matches_within_type() {
        assert_eq!(Value::Int(1), Value::Int(1));
        assert_eq!(Value::String("a".into()), Value::String("a".into()));
        assert_eq!(
            Value::Bytes(Bytes::from_static(b"\x00\x01")),
            Value::Bytes(Bytes::from_static(b"\x00\x01"))
        );
    }

    #[test]
    fn equality_int_float_cross_compares() {
        // Decision §1: numeric cross-type equality holds at the
        // structural level so `1 == 1.0` agrees inside containers.
        assert_eq!(Value::Int(1), Value::Float(1.0));
        assert_eq!(Value::Float(2.0), Value::Int(2));
    }

    #[test]
    fn equality_bytes_string_disagree() {
        // Decision §1: Bytes vs String is always unequal — even if the
        // bytes spell out the same UTF-8 content. User must convert
        // explicitly via `to_string()` / `to_bytes()`.
        let s = Value::String("hi".into());
        let b = Value::Bytes(Bytes::from_static(b"hi"));
        assert_ne!(s, b);
    }

    #[test]
    fn truthiness_bytes_follows_string_rule() {
        // Decision §12: non-empty bytes truthy, empty bytes falsy.
        assert!(Value::Bytes(Bytes::from_static(b"x")).is_truthy());
        assert!(!Value::Bytes(Bytes::new()).is_truthy());
    }

    #[test]
    fn type_name_covers_every_variant() {
        // Pin the strings used by primitive error messages so a
        // user-facing rename has to come through this test.
        assert_eq!(Value::Null.type_name(), "null");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Int(0).type_name(), "int");
        assert_eq!(Value::Float(0.0).type_name(), "float");
        assert_eq!(Value::String(String::new()).type_name(), "string");
        assert_eq!(Value::Bytes(Bytes::new()).type_name(), "bytes");
        assert_eq!(Value::empty_array().type_name(), "array");
        assert_eq!(Value::empty_object().type_name(), "object");
    }

    #[test]
    fn object_uses_indexmap_preserves_insertion_order() {
        // `serde_json` v1 uses preserve_order; switching the backing map
        // must keep that property so tap output stays deterministic.
        let mut m = Map::new();
        m.insert("z".into(), Value::Int(1));
        m.insert("a".into(), Value::Int(2));
        m.insert("m".into(), Value::Int(3));
        let keys: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["z", "a", "m"]);
    }
}

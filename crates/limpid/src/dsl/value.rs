// Several methods are part of the public Value contract (mirroring
// `serde_json::Value`'s shape) but currently have no internal caller —
// future primitives migrating from the old representation will pick
// them up. Allowed at module scope to keep the API surface intact
// without per-item annotations.
#![allow(dead_code)]
//! limpid DSL value types.
//!
//! Two representations of the same conceptual value tree are kept side
//! by side, distinguishing **where** a value lives:
//!
//! - [`Value<'bump>`] — the *transient* form that flows through
//!   evaluation, execution, and function dispatch inside
//!   [`crate::pipeline::run_pipeline`]. All payloads
//!   (`String`, `Bytes`, `Array`, `Object`) live in the per-event
//!   [`super::arena::EventArena`]; releasing the arena at event end
//!   frees the entire tree in a single chunk-group `dealloc`. The enum
//!   is `Copy` because every payload variant carries an arena
//!   reference, so passing values by value is free.
//! - [`OwnedValue`] — the *boundary* form. Used wherever the value
//!   crosses an event boundary or needs `'static`: channel sends
//!   ([`crate::event::OwnedEvent`]), JSON persistence
//!   (tap / queue / error_log), inject replay, etc. Heap-owned and
//!   `Clone`.
//!
//! Conversion is explicit:
//!
//! - [`OwnedValue::view_in`] — copy an owned value into the arena and
//!   return a borrowed view. Used at `run_pipeline` entry to turn the
//!   incoming workspace into arena-backed values.
//! - [`Value::to_owned_value`] — heap-allocate a fresh owned copy of
//!   the borrowed value. Used at `run_pipeline` exit to hand the result
//!   back to the channel / persistence layer.
//!
//! The enum split exists for one reason: per-event allocation + drop
//! costs (~60% of on-CPU on the v0.5.7 D pipeline baseline) collapse
//! into a single bump-allocator chunk-group free at event end. JSON
//! boundary semantics (the `$bytes_b64` marker / `$`-key escape) live
//! in [`super::value_json`] and operate on `OwnedValue` only — the
//! transient form never reaches a JSON boundary directly.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use compact_str::CompactString;
use indexmap::IndexMap;

use super::arena::EventArena;

// ===========================================================================
// Owned (boundary) representation
// ===========================================================================

/// Map type backing [`OwnedValue::Object`]. Insertion order is
/// preserved so tap output and config-driven iteration are
/// deterministic — limpid's existing `serde_json` setup uses the
/// `preserve_order` feature for the same reason.
pub type Map = IndexMap<String, OwnedValue>;

/// Heap-owned boundary form of the runtime value. Mirrors the variant
/// shape of [`Value`] but every payload is owned (`CompactString`,
/// `Bytes`, `Vec`, `IndexMap`) so it survives outside the per-event
/// arena.
///
/// `String` payload uses [`CompactString`] (24-byte inline budget on
/// 64-bit) so typical OCSF / syslog field values (`"INFO"`, `"sshd"`,
/// IPv4 strings) cross the `BorrowedEvent::to_owned()` boundary
/// without a heap allocation. Strings longer than 24 bytes fall back
/// to a heap pointer transparently — same `Display` / `Deref<str>`
/// surface as `String`, so callers continue to see a `&str` view.
#[derive(Debug, Clone)]
pub enum OwnedValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(CompactString),
    Bytes(Bytes),
    /// Wall-clock instant, normalised to UTC. Internally an epoch
    /// position; no per-value offset metadata. Source-claimed timezone
    /// information from `strptime` input is used to *decode* the wall
    /// time correctly but is not stored. Render in a non-UTC offset by
    /// passing the explicit `timezone` argument to `strftime`.
    Timestamp(DateTime<Utc>),
    Array(Vec<OwnedValue>),
    Object(Map),
}

impl OwnedValue {
    /// Empty object — convenience for boundary-side construction.
    pub fn empty_object() -> Self {
        OwnedValue::Object(Map::new())
    }

    /// Empty array.
    pub fn empty_array() -> Self {
        OwnedValue::Array(Vec::new())
    }

    /// Copy this owned value into `arena` and return a borrowed
    /// transient view. Strings, bytes, array contents, and object keys
    /// are all alloc'd into the arena so the returned [`Value`] outlives
    /// nothing past the arena.
    pub fn view_in<'bump>(&self, arena: &EventArena<'bump>) -> Value<'bump> {
        match self {
            OwnedValue::Null => Value::Null,
            OwnedValue::Bool(b) => Value::Bool(*b),
            OwnedValue::Int(n) => Value::Int(*n),
            OwnedValue::Float(n) => Value::Float(*n),
            OwnedValue::String(s) => Value::String(arena.alloc_str(s.as_str())),
            OwnedValue::Bytes(b) => Value::Bytes(arena.alloc_bytes(b)),
            OwnedValue::Timestamp(dt) => Value::Timestamp(*dt),
            OwnedValue::Array(items) => {
                let mut out = bumpalo::collections::Vec::with_capacity_in(items.len(), arena.bump());
                for item in items {
                    out.push(item.view_in(arena));
                }
                Value::Array(out.into_bump_slice())
            }
            OwnedValue::Object(map) => {
                let mut out = bumpalo::collections::Vec::with_capacity_in(map.len(), arena.bump());
                for (k, v) in map {
                    out.push((arena.alloc_str(k), v.view_in(arena)));
                }
                Value::Object(out.into_bump_slice())
            }
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, OwnedValue::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            OwnedValue::Null => "null",
            OwnedValue::Bool(_) => "bool",
            OwnedValue::Int(_) => "int",
            OwnedValue::Float(_) => "float",
            OwnedValue::String(_) => "string",
            OwnedValue::Bytes(_) => "bytes",
            OwnedValue::Timestamp(_) => "timestamp",
            OwnedValue::Array(_) => "array",
            OwnedValue::Object(_) => "object",
        }
    }
}

// PartialEq is hand-rolled because `f64` is not `Eq` (NaN ≠ NaN). The
// DSL `==` operator has a separate `values_match` helper in `eval.rs`
// that handles cross-type rules; the impl here is the structural fallback
// used by container comparisons.
impl PartialEq for OwnedValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (OwnedValue::Null, OwnedValue::Null) => true,
            (OwnedValue::Bool(a), OwnedValue::Bool(b)) => a == b,
            (OwnedValue::Int(a), OwnedValue::Int(b)) => a == b,
            (OwnedValue::Float(a), OwnedValue::Float(b)) => a == b,
            (OwnedValue::Int(a), OwnedValue::Float(b))
            | (OwnedValue::Float(b), OwnedValue::Int(a)) => (*a as f64) == *b,
            (OwnedValue::String(a), OwnedValue::String(b)) => a == b,
            (OwnedValue::Bytes(a), OwnedValue::Bytes(b)) => a == b,
            (OwnedValue::Timestamp(a), OwnedValue::Timestamp(b)) => a == b,
            (OwnedValue::Array(a), OwnedValue::Array(b)) => a == b,
            (OwnedValue::Object(a), OwnedValue::Object(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            _ => false,
        }
    }
}

// === From conversions for OwnedValue ===
//
// Only OwnedValue carries `From` impls. Constructing a `Value<'bump>`
// requires an arena, so it must go through explicit allocator calls;
// implicit `T -> Value<'bump>` would invite heap leaks (a `String::from`
// the closure forgot to alloc) and was deliberately not provided.

impl From<bool> for OwnedValue {
    fn from(b: bool) -> Self {
        OwnedValue::Bool(b)
    }
}

impl From<i64> for OwnedValue {
    fn from(n: i64) -> Self {
        OwnedValue::Int(n)
    }
}

impl From<i32> for OwnedValue {
    fn from(n: i32) -> Self {
        OwnedValue::Int(n as i64)
    }
}

impl From<u32> for OwnedValue {
    fn from(n: u32) -> Self {
        OwnedValue::Int(n as i64)
    }
}

impl From<u64> for OwnedValue {
    fn from(n: u64) -> Self {
        // u64::MAX > i64::MAX; saturate to keep values lossless when in
        // i64 range, fall through to Float otherwise.
        if n <= i64::MAX as u64 {
            OwnedValue::Int(n as i64)
        } else {
            OwnedValue::Float(n as f64)
        }
    }
}

impl From<usize> for OwnedValue {
    fn from(n: usize) -> Self {
        OwnedValue::Int(n as i64)
    }
}

impl From<f64> for OwnedValue {
    fn from(n: f64) -> Self {
        OwnedValue::Float(n)
    }
}

impl From<&str> for OwnedValue {
    fn from(s: &str) -> Self {
        OwnedValue::String(CompactString::from(s))
    }
}

impl From<String> for OwnedValue {
    fn from(s: String) -> Self {
        OwnedValue::String(CompactString::from(s))
    }
}

impl From<CompactString> for OwnedValue {
    fn from(s: CompactString) -> Self {
        OwnedValue::String(s)
    }
}

impl From<Bytes> for OwnedValue {
    fn from(b: Bytes) -> Self {
        OwnedValue::Bytes(b)
    }
}

impl From<DateTime<Utc>> for OwnedValue {
    fn from(dt: DateTime<Utc>) -> Self {
        OwnedValue::Timestamp(dt)
    }
}

impl From<chrono::DateTime<chrono::FixedOffset>> for OwnedValue {
    fn from(dt: chrono::DateTime<chrono::FixedOffset>) -> Self {
        OwnedValue::Timestamp(dt.with_timezone(&Utc))
    }
}

impl From<Vec<u8>> for OwnedValue {
    fn from(v: Vec<u8>) -> Self {
        OwnedValue::Bytes(Bytes::from(v))
    }
}

impl From<Vec<OwnedValue>> for OwnedValue {
    fn from(v: Vec<OwnedValue>) -> Self {
        OwnedValue::Array(v)
    }
}

impl From<Map> for OwnedValue {
    fn from(m: Map) -> Self {
        OwnedValue::Object(m)
    }
}

// ===========================================================================
// Arena-backed (transient) representation
// ===========================================================================

/// Runtime value flowing through the DSL evaluator. All non-scalar
/// payloads live in the [`super::arena::EventArena`] borrowed for the
/// `'bump` lifetime; the entire tree is freed in a single bump-allocator
/// chunk release when the arena drops at end-of-event.
///
/// Variant choice mirrors [`OwnedValue`]; the only structural change is
/// that `Object` is a frozen `&'bump [(&'bump str, Value<'bump>)]`
/// rather than a hash map. Insertion order is preserved by construction
/// (the builder pushes entries in order, then freezes the slice with
/// `into_bump_slice`). Lookup is linear; for the typical limpid object
/// shape (~10 keys) this is faster than the hash + entry-table indirection
/// the previous `IndexMap`-backed form paid.
///
/// `Copy` is implemented because every variant is either a scalar or an
/// arena reference — so passing values around is free, no `.clone()`
/// dance needed inside hot paths.
#[derive(Debug, Clone, Copy)]
pub enum Value<'bump> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(&'bump str),
    Bytes(&'bump [u8]),
    /// See [`OwnedValue::Timestamp`] for the wall-clock semantics —
    /// identical here, just stored alongside the borrowed payload
    /// variants.
    Timestamp(DateTime<Utc>),
    Array(&'bump [Value<'bump>]),
    Object(&'bump [(&'bump str, Value<'bump>)]),
}

impl<'bump> Value<'bump> {
    /// Empty object — convenience for primitives that build a result
    /// map field-by-field. Equivalent to `Value::Object(&[])`.
    pub fn empty_object() -> Self {
        Value::Object(&[])
    }

    /// Empty array. Equivalent to `Value::Array(&[])`.
    pub fn empty_array() -> Self {
        Value::Array(&[])
    }

    /// Allocate `s` into the arena and wrap as `Value::String`.
    pub fn string_in(arena: &EventArena<'bump>, s: &str) -> Self {
        Value::String(arena.alloc_str(s))
    }

    /// Allocate `b` into the arena and wrap as `Value::Bytes`.
    pub fn bytes_in(arena: &EventArena<'bump>, b: &[u8]) -> Self {
        Value::Bytes(arena.alloc_bytes(b))
    }

    /// Heap-allocate a fresh [`OwnedValue`] from this borrowed view.
    /// Used at `run_pipeline` exit to hand the result to the channel.
    ///
    /// Takes `self` by value because `Value<'bump>` is `Copy` — passing
    /// it through avoids the `&Value` reference layer at the call site
    /// without changing semantics.
    pub fn to_owned_value(self) -> OwnedValue {
        match self {
            Value::Null => OwnedValue::Null,
            Value::Bool(b) => OwnedValue::Bool(b),
            Value::Int(n) => OwnedValue::Int(n),
            Value::Float(n) => OwnedValue::Float(n),
            Value::String(s) => OwnedValue::String(CompactString::from(s)),
            Value::Bytes(b) => OwnedValue::Bytes(Bytes::copy_from_slice(b)),
            Value::Timestamp(dt) => OwnedValue::Timestamp(dt),
            Value::Array(items) => OwnedValue::Array(
                items.iter().map(|v| v.to_owned_value()).collect(),
            ),
            Value::Object(entries) => {
                let mut map = Map::with_capacity(entries.len());
                for (k, v) in entries {
                    map.insert((*k).to_string(), v.to_owned_value());
                }
                OwnedValue::Object(map)
            }
        }
    }

    // --- Type predicates ---
    //
    // Used by primitive arg validation and the DSL evaluator: text-only
    // primitives reject `Bytes` via these rather than reproducing the
    // check inline.

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

    pub fn is_timestamp(&self) -> bool {
        matches!(self, Value::Timestamp(_))
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
            Value::Timestamp(_) => true,
            Value::Array(a) => !a.is_empty(),
            Value::Object(m) => !m.is_empty(),
        }
    }

    // --- Accessors ---
    //
    // Mirrors the subset of `serde_json::Value` accessors that limpid
    // actually uses. Returned `&str` / `&[u8]` / slice references all
    // borrow at `'bump` so callers can keep them past the immediate
    // method call.

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

    pub fn as_str(&self) -> Option<&'bump str> {
        if let Value::String(s) = self {
            Some(*s)
        } else {
            None
        }
    }

    pub fn as_bytes(&self) -> Option<&'bump [u8]> {
        if let Value::Bytes(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_timestamp(&self) -> Option<DateTime<Utc>> {
        if let Value::Timestamp(dt) = self {
            Some(*dt)
        } else {
            None
        }
    }

    pub fn as_array(&self) -> Option<&'bump [Value<'bump>]> {
        if let Value::Array(a) = self {
            Some(*a)
        } else {
            None
        }
    }

    pub fn as_object(&self) -> Option<&'bump [(&'bump str, Value<'bump>)]> {
        if let Value::Object(m) = self {
            Some(*m)
        } else {
            None
        }
    }

    /// Convenience: object-key lookup. Returns `None` for non-objects
    /// or missing keys. Linear scan (`Object` is a frozen entries slice
    /// in insertion order).
    pub fn get(&self, key: &str) -> Option<Value<'bump>> {
        match self {
            Value::Object(entries) => entries
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v),
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
            Value::Timestamp(_) => "timestamp",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

// PartialEq for `Value<'bump>`. Same hand-roll rationale as
// `OwnedValue`; `Object` compares by insertion-order-aware structural
// equality (same key-set, same order — keeps tap output stable).
impl<'bump> PartialEq for Value<'bump> {
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
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va == vb)
            }
            _ => false,
        }
    }
}

// `Display` for diagnostics. Concrete shape is not contractual —
// primitives that need a stable user-facing string call into
// `eval::value_to_string` or build the string explicitly.
impl<'bump> std::fmt::Display for Value<'bump> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "<bytes len={}>", b.len()),
            Value::Timestamp(dt) => write!(f, "{}", dt.to_rfc3339()),
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
            Value::Object(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
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

impl std::fmt::Display for OwnedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnedValue::Null => write!(f, "null"),
            OwnedValue::Bool(b) => write!(f, "{b}"),
            OwnedValue::Int(n) => write!(f, "{n}"),
            OwnedValue::Float(n) => write!(f, "{n}"),
            OwnedValue::String(s) => write!(f, "{s}"),
            OwnedValue::Bytes(b) => write!(f, "<bytes len={}>", b.len()),
            OwnedValue::Timestamp(dt) => write!(f, "{}", dt.to_rfc3339()),
            OwnedValue::Array(a) => {
                write!(f, "[")?;
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            OwnedValue::Object(m) => {
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

// ===========================================================================
// Builders for arena-backed objects / arrays
// ===========================================================================

/// Insertion-order builder for [`Value::Object`]. Use when you need to
/// accumulate entries dynamically and then freeze the result. Drops the
/// inner `bumpalo::Vec` on `finish` — the produced `&'bump [...]` lives
/// in the arena.
pub struct ObjectBuilder<'bump> {
    entries: bumpalo::collections::Vec<'bump, (&'bump str, Value<'bump>)>,
    arena: &'bump bumpalo::Bump,
}

impl<'bump> ObjectBuilder<'bump> {
    pub fn new(arena: &EventArena<'bump>) -> Self {
        Self {
            entries: bumpalo::collections::Vec::new_in(arena.bump()),
            arena: arena.bump(),
        }
    }

    pub fn with_capacity(arena: &EventArena<'bump>, cap: usize) -> Self {
        Self {
            entries: bumpalo::collections::Vec::with_capacity_in(cap, arena.bump()),
            arena: arena.bump(),
        }
    }

    /// Push an entry. Insertion order is preserved by the underlying
    /// arena vec; duplicate keys are NOT detected here (debug builds may
    /// add a `debug_assert!` later — release stays linear-scan-fast).
    pub fn push(&mut self, key: &'bump str, value: Value<'bump>) {
        self.entries.push((key, value));
    }

    /// Push an entry, allocating the key into the arena first.
    pub fn push_str(&mut self, key: &str, value: Value<'bump>) {
        let k = self.arena.alloc_str(key);
        self.entries.push((k, value));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Freeze the builder into a `Value::Object` whose entries slice
    /// lives in the arena.
    pub fn finish(self) -> Value<'bump> {
        Value::Object(self.entries.into_bump_slice())
    }
}

/// Insertion-order builder for [`Value::Array`].
pub struct ArrayBuilder<'bump> {
    items: bumpalo::collections::Vec<'bump, Value<'bump>>,
}

impl<'bump> ArrayBuilder<'bump> {
    pub fn new(arena: &EventArena<'bump>) -> Self {
        Self {
            items: bumpalo::collections::Vec::new_in(arena.bump()),
        }
    }

    pub fn with_capacity(arena: &EventArena<'bump>, cap: usize) -> Self {
        Self {
            items: bumpalo::collections::Vec::with_capacity_in(cap, arena.bump()),
        }
    }

    pub fn push(&mut self, value: Value<'bump>) {
        self.items.push(value);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn finish(self) -> Value<'bump> {
        Value::Array(self.items.into_bump_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_arena<R>(f: impl for<'b> FnOnce(&EventArena<'b>) -> R) -> R {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        f(&arena)
    }

    #[test]
    fn equality_matches_within_type() {
        with_arena(|arena| {
            assert_eq!(Value::Int(1), Value::Int(1));
            assert_eq!(
                Value::String(arena.alloc_str("a")),
                Value::String(arena.alloc_str("a"))
            );
            assert_eq!(
                Value::Bytes(arena.alloc_bytes(b"\x00\x01")),
                Value::Bytes(arena.alloc_bytes(b"\x00\x01"))
            );
        });
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
        // Decision §1: Bytes vs String is always unequal.
        with_arena(|arena| {
            let s = Value::String(arena.alloc_str("hi"));
            let b = Value::Bytes(arena.alloc_bytes(b"hi"));
            assert_ne!(s, b);
        });
    }

    #[test]
    fn truthiness_bytes_follows_string_rule() {
        with_arena(|arena| {
            assert!(Value::Bytes(arena.alloc_bytes(b"x")).is_truthy());
            assert!(!Value::Bytes(arena.alloc_bytes(b"")).is_truthy());
        });
    }

    #[test]
    fn type_name_covers_every_variant() {
        // Pin the strings used by primitive error messages so a
        // user-facing rename has to come through this test.
        with_arena(|arena| {
            assert_eq!(Value::Null.type_name(), "null");
            assert_eq!(Value::Bool(true).type_name(), "bool");
            assert_eq!(Value::Int(0).type_name(), "int");
            assert_eq!(Value::Float(0.0).type_name(), "float");
            assert_eq!(Value::String(arena.alloc_str("")).type_name(), "string");
            assert_eq!(Value::Bytes(arena.alloc_bytes(b"")).type_name(), "bytes");
            assert_eq!(Value::empty_array().type_name(), "array");
            assert_eq!(Value::empty_object().type_name(), "object");
        });
    }

    #[test]
    fn object_builder_preserves_insertion_order() {
        with_arena(|arena| {
            let mut b = ObjectBuilder::new(arena);
            b.push_str("z", Value::Int(1));
            b.push_str("a", Value::Int(2));
            b.push_str("m", Value::Int(3));
            let obj = b.finish();
            let entries = obj.as_object().unwrap();
            let keys: Vec<&str> = entries.iter().map(|(k, _)| *k).collect();
            assert_eq!(keys, vec!["z", "a", "m"]);
        });
    }

    #[test]
    fn owned_value_view_in_round_trip() {
        with_arena(|arena| {
            let mut m = Map::new();
            m.insert("k".into(), OwnedValue::Int(42));
            m.insert("s".into(), OwnedValue::String("hello".into()));
            let owned = OwnedValue::Object(m);

            let view = owned.view_in(arena);
            let back = view.to_owned_value();
            assert_eq!(owned, back);
        });
    }

    #[test]
    fn owned_object_uses_indexmap_preserves_insertion_order() {
        let mut m = Map::new();
        m.insert("z".into(), OwnedValue::Int(1));
        m.insert("a".into(), OwnedValue::Int(2));
        m.insert("m".into(), OwnedValue::Int(3));
        let keys: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["z", "a", "m"]);
    }
}

//! `nest_dotted_keys(value)` — convert flat-dotted Object keys into
//! nested Objects.
//!
//! Some upstreams (Filebeat / Logstash JSON emitters used by zeek /
//! suricata modules, certain Splunk HEC sources, OpenSearch ingest
//! pipelines) flatten nested JSON for Elasticsearch indexing
//! conventions: `{"id": {"orig_h": "1.1.1.1"}}` becomes
//! `{"id.orig_h": "1.1.1.1"}`. limpid DSL does not support bracket-
//! subscript access (`body["id.orig_h"]`), so dotted keys are
//! unreachable from a parser. This primitive un-flattens them so
//! parsers can address fields as `body.id.orig_h` regardless of which
//! upstream emitted the JSON.
//!
//! Semantics (recursive):
//!
//! - `Object` keys containing `.` are split at every `.` and built
//!   into a chain of nested `Object`s. `{"a.b.c": 1}` becomes
//!   `{"a": {"b": {"c": 1}}}`.
//! - Sibling dotted keys with the same prefix merge under that
//!   prefix: `{"id.orig_h": "x", "id.orig_p": 80}` →
//!   `{"id": {"orig_h": "x", "orig_p": 80}}`.
//! - The recursion walks into values too: `Object` values have their
//!   own dotted keys un-flattened; `Array` elements that are
//!   themselves Objects are processed element-wise.
//! - Non-Object / non-Array inputs return unchanged.
//!
//! Collisions are loud-fail:
//!
//! - **Duplicate leaf**: `{"a": 1, "a": 2}` already isn't producible
//!   from JSON (parse_json deduplicates by last-wins), but if a
//!   caller constructs one, the duplicate triggers an error.
//! - **Leaf vs branch**: `{"a": 1, "a.b": 2}` cannot be merged — `a`
//!   is both a scalar and the start of a nested path. Errors out
//!   with a message identifying the conflicting key.
//! - **Empty segment**: a key like `"a..b"` or `".a"` or `"a."`
//!   produces an empty segment, which is treated as a hard error
//!   (almost always a typo or escaped-dot mishandling upstream).
//!
//! Round-trip note: this primitive is the inverse of the (not yet
//! implemented) `flatten_dotted_keys`. Together they let pipelines
//! normalise between the two JSON shapes without losing data.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::dsl::arena::EventArena;
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::{ArrayBuilder, ObjectBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};

/// Maximum dotted-key segment count. Filebeat / Logstash JSON output
/// typically flattens 2-4 levels of nesting; 32 leaves plenty of
/// headroom while preventing an attacker-supplied JSON like
/// `{"a.a.a...(100K dots)": 1}` from blowing the stack via recursive
/// `insert_path` calls.
const MAX_DOTTED_DEPTH: usize = 32;

/// Maximum recursion depth for `nest` walking into Object / Array
/// values. `parse_json` (serde_json) already enforces a 128-deep
/// limit at JSON-parse time, so this is a defence-in-depth bound for
/// the rare case `nest_dotted_keys` is invoked on a Value built by
/// other means (e.g. composed in-DSL).
const MAX_VALUE_DEPTH: usize = 64;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "nest_dotted_keys",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Any),
        |arena, args, _event| nest(arena, &args[0]),
    );
}

/// Either a leaf value or a sub-tree, built up while merging sibling
/// dotted keys before materialisation back into the bump arena.
enum Node<'bump> {
    Leaf(Value<'bump>),
    Branch(BTreeMap<String, Node<'bump>>),
}

fn nest<'bump>(arena: &EventArena<'bump>, value: &Value<'bump>) -> Result<Value<'bump>> {
    nest_inner(arena, value, 0)
}

fn nest_inner<'bump>(
    arena: &EventArena<'bump>,
    value: &Value<'bump>,
    depth: usize,
) -> Result<Value<'bump>> {
    if depth > MAX_VALUE_DEPTH {
        bail!(
            "nest_dotted_keys(): value nesting exceeds depth limit ({})",
            MAX_VALUE_DEPTH
        );
    }
    match value {
        Value::Object(entries) => {
            // First pass: build a key trie so sibling dotted keys with a
            // common prefix merge under that prefix.
            let mut root: BTreeMap<String, Node<'bump>> = BTreeMap::new();
            for (key, val) in entries.iter() {
                let segments: Vec<&str> = key.split('.').collect();
                if segments.iter().any(|s| s.is_empty()) {
                    bail!(
                        "nest_dotted_keys(): key '{}' has an empty segment (leading/trailing/double dot)",
                        key
                    );
                }
                if segments.len() > MAX_DOTTED_DEPTH {
                    bail!(
                        "nest_dotted_keys(): key '{}' has {} dotted segments, exceeds limit ({})",
                        key,
                        segments.len(),
                        MAX_DOTTED_DEPTH
                    );
                }
                let nested_val = nest_inner(arena, val, depth + 1)?;
                insert_path(&mut root, &segments, nested_val, key)?;
            }
            // Second pass: walk the trie and emit a real bump-allocated Object.
            Ok(materialise(arena, root))
        }
        Value::Array(items) => {
            let mut builder = ArrayBuilder::with_capacity(arena, items.len());
            for item in items.iter() {
                builder.push(nest_inner(arena, item, depth + 1)?);
            }
            Ok(builder.finish())
        }
        other => Ok(*other),
    }
}

fn insert_path<'bump>(
    node: &mut BTreeMap<String, Node<'bump>>,
    segments: &[&str],
    value: Value<'bump>,
    original_key: &str,
) -> Result<()> {
    let (head, rest) = segments.split_first().expect("non-empty segments");
    if rest.is_empty() {
        match node.entry((*head).to_string()) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(Node::Leaf(value));
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => bail!(
                "nest_dotted_keys(): key '{}' conflicts with an earlier sibling at segment '{}'",
                original_key,
                head
            ),
        }
    } else {
        let entry = node
            .entry((*head).to_string())
            .or_insert_with(|| Node::Branch(BTreeMap::new()));
        match entry {
            Node::Branch(sub) => insert_path(sub, rest, value, original_key),
            Node::Leaf(_) => bail!(
                "nest_dotted_keys(): key '{}' tries to nest under '{}' which is already a leaf value",
                original_key,
                head
            ),
        }
    }
}

fn materialise<'bump>(
    arena: &EventArena<'bump>,
    tree: BTreeMap<String, Node<'bump>>,
) -> Value<'bump> {
    let mut builder = ObjectBuilder::with_capacity(arena, tree.len());
    for (k, v) in tree {
        let key_ref = arena.alloc_str(&k);
        let value = match v {
            Node::Leaf(val) => val,
            Node::Branch(sub) => materialise(arena, sub),
        };
        builder.push(key_ref, value);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;

    fn parse_and_nest(json: &str) -> Result<String> {
        // Leak a bump + arena to keep their lifetime 'static for ergonomic
        // assertions; bump objects are tiny and only live as long as the
        // test process.
        let bump: &'static bumpalo::Bump = Box::leak(Box::new(bumpalo::Bump::new()));
        let arena: &'static EventArena<'static> = Box::leak(Box::new(EventArena::new(bump)));
        let parsed: serde_json::Value = serde_json::from_str(json)?;
        let val = json_to_value(arena, &parsed);
        let nested = nest(arena, &val)?;
        Ok(value_to_json(&nested))
    }

    fn json_to_value<'bump>(
        arena: &'bump EventArena<'bump>,
        v: &serde_json::Value,
    ) -> Value<'bump> {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::Null
                }
            }
            serde_json::Value::String(s) => Value::String(arena.alloc_str(s)),
            serde_json::Value::Array(items) => {
                let mut b = ArrayBuilder::with_capacity(arena, items.len());
                for i in items {
                    b.push(json_to_value(arena, i));
                }
                b.finish()
            }
            serde_json::Value::Object(map) => {
                let mut b = ObjectBuilder::with_capacity(arena, map.len());
                for (k, v) in map {
                    b.push(arena.alloc_str(k), json_to_value(arena, v));
                }
                b.finish()
            }
        }
    }

    fn value_to_json(v: &Value<'_>) -> String {
        // Use limpid's own owned-value Serialize via OwnedValue conversion.
        // For test purposes, a hand-rolled walker is enough.
        match v {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::String(s) => format!("\"{}\"", s),
            Value::Bytes(b) => format!("\"<{} bytes>\"", b.len()),
            Value::Array(items) => {
                let parts: Vec<String> = items.iter().map(value_to_json).collect();
                format!("[{}]", parts.join(","))
            }
            Value::Object(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("\"{}\":{}", k, value_to_json(v)))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            Value::Timestamp(t) => format!("\"{}\"", t.to_rfc3339()),
        }
    }

    #[test]
    fn nests_simple_pair() {
        // Single dotted key splits into a 2-level Object.
        let out = parse_and_nest(r#"{"id.orig_h":"1.1.1.1"}"#).unwrap();
        assert_eq!(out, r#"{"id":{"orig_h":"1.1.1.1"}}"#);
    }

    #[test]
    fn merges_sibling_keys() {
        // Sibling dotted keys with a common prefix collapse together.
        let out = parse_and_nest(
            r#"{"id.orig_h":"1.1.1.1","id.orig_p":80,"id.resp_h":"2.2.2.2","ts":123}"#,
        )
        .unwrap();
        // BTreeMap orders keys lexicographically.
        assert_eq!(
            out,
            r#"{"id":{"orig_h":"1.1.1.1","orig_p":80,"resp_h":"2.2.2.2"},"ts":123}"#
        );
    }

    #[test]
    fn nests_three_levels() {
        let out = parse_and_nest(r#"{"a.b.c":1,"a.b.d":2}"#).unwrap();
        assert_eq!(out, r#"{"a":{"b":{"c":1,"d":2}}}"#);
    }

    #[test]
    fn passes_through_plain_keys() {
        let out = parse_and_nest(r#"{"ts":123,"uid":"X"}"#).unwrap();
        assert_eq!(out, r#"{"ts":123,"uid":"X"}"#);
    }

    #[test]
    fn recurses_into_nested_objects() {
        // Dotted keys inside a sub-object are also un-flattened.
        let out = parse_and_nest(r#"{"outer":{"inner.a":1,"inner.b":2}}"#).unwrap();
        assert_eq!(out, r#"{"outer":{"inner":{"a":1,"b":2}}}"#);
    }

    #[test]
    fn recurses_into_arrays() {
        let out = parse_and_nest(r#"{"items":[{"x.y":1},{"x.y":2}]}"#).unwrap();
        assert_eq!(out, r#"{"items":[{"x":{"y":1}},{"x":{"y":2}}]}"#);
    }

    #[test]
    fn rejects_leaf_branch_collision() {
        // `a` is set as a scalar AND `a.b` tries to nest under it — ambiguous.
        let err = parse_and_nest(r#"{"a":1,"a.b":2}"#).unwrap_err();
        assert!(err.to_string().contains("a.b"));
    }

    #[test]
    fn rejects_empty_segment() {
        let err = parse_and_nest(r#"{"a..b":1}"#).unwrap_err();
        assert!(err.to_string().contains("empty segment"));
    }

    #[test]
    fn rejects_dotted_key_above_segment_depth_limit() {
        // 100 dot-separated segments — well past MAX_DOTTED_DEPTH (32).
        // Without the limit, this would recurse 100-deep into insert_path
        // and ultimately enable stack-overflow DoS for attacker-controlled
        // JSON. With the limit, we bail loud and fast.
        let key: String = (0..100)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(".");
        let json = format!(r#"{{"{}":1}}"#, key);
        let err = parse_and_nest(&json).unwrap_err();
        assert!(
            err.to_string().contains("dotted segments") && err.to_string().contains("limit"),
            "expected segment-depth error, got: {}",
            err
        );
    }

    #[test]
    fn rejects_value_above_nesting_depth_limit() {
        // 80-deep nested Object — past MAX_VALUE_DEPTH (64). Construct
        // directly because serde_json would itself bail at 128 deep.
        let mut json = String::from("1");
        for _ in 0..80 {
            json = format!(r#"{{"a":{json}}}"#);
        }
        let err = parse_and_nest(&json).unwrap_err();
        assert!(
            err.to_string().contains("value nesting"),
            "expected value-depth error, got: {}",
            err
        );
    }

    #[test]
    fn accepts_depth_at_segment_limit() {
        // Exactly 32 segments — at the limit but not over. Should succeed.
        let key: String = (0..32)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(".");
        let json = format!(r#"{{"{}":1}}"#, key);
        let out = parse_and_nest(&json).expect("32-segment key should be accepted");
        assert!(out.starts_with("{\"a0\":{"));
    }

    #[test]
    fn returns_non_object_unchanged() {
        let bump: &'static bumpalo::Bump = Box::leak(Box::new(bumpalo::Bump::new()));
        let arena: &'static EventArena<'static> = Box::leak(Box::new(EventArena::new(bump)));
        let v = Value::String("hello");
        let out = nest(arena, &v).unwrap();
        assert_eq!(value_to_json(&out), r#""hello""#);
    }
}

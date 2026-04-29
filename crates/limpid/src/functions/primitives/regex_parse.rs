//! `regex_parse(target, pattern)` — named-capture based extraction.
//!
//! Returns an Object with one key per named capture group in `pattern`.
//! Capture names containing `.` build nested objects.
//!
//! ## Implementation note: `__DOT__` marker
//!
//! The `regex` family disallows `.` in capture names, so we preprocess
//! the pattern to mangle `(?P<a.b>…)` to `(?P<a__DOT__b>…)`, compile,
//! then demangle when reading capture names back out. `__DOT__` is a
//! reserved internal marker — capture names that contain the literal
//! string `__DOT__` will be misinterpreted.

use std::collections::BTreeMap;

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};

use super::{get_cached_regex, val_to_str};
use crate::functions::{FunctionRegistry, FunctionSig, ParserInfo};
use crate::modules::schema::FieldType;

const DOT_MARKER: &str = "__DOT__";

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "regex_parse",
        FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Any),
        |arena, args, _event| {
            let target = val_to_str(&args[0])?;
            let pattern = val_to_str(&args[1])?;

            let mangled = mangle_pattern(&pattern);
            let re =
                get_cached_regex(&mangled).map_err(|e| anyhow::anyhow!("invalid regex: {}", e))?;

            // No named captures at all → empty object so a bare statement is a no-op.
            if re.capture_names().flatten().next().is_none() {
                return Ok(Value::empty_object());
            }

            let caps = match re.captures(&target) {
                Some(c) => c,
                None => return Ok(Value::Null),
            };

            // Stage 1: gather captures into a heap-side intermediate
            // tree (`Tree`). We can't build the arena slice incrementally
            // for nested groups because each `Object` slice is frozen on
            // `finish()`. Stage 2 walks the heap tree and emits the final
            // arena-backed Value.
            let mut root = Tree::default();
            for name in re.capture_names().flatten() {
                let original = demangle(name);
                if let Some(m) = caps.name(name) {
                    insert_path(&mut root, &original, m.as_str());
                }
            }
            Ok(materialise(arena, &root))
        },
    );
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "regex_parse",
        produces: Vec::new(),
        wildcards: true,
    });
}

#[derive(Default)]
struct Tree {
    leaf: Option<String>,
    children: BTreeMap<String, Tree>,
    /// Insertion order of children — `children` itself uses BTreeMap for
    /// stable per-key access while `order` preserves the order the
    /// regex's capture-name iterator handed them to us.
    order: Vec<String>,
}

fn insert_path(node: &mut Tree, path: &str, value: &str) {
    let mut parts = path.split('.').peekable();
    let mut current = node;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            // Leaf: drop the value here. If a previous iteration
            // already turned this slot into a subtree, the subtree
            // wins (matches the historical `set_nested` policy).
            if !current.children.contains_key(part) {
                if !current.order.iter().any(|s| s == part) {
                    current.order.push(part.to_string());
                }
                current
                    .children
                    .entry(part.to_string())
                    .or_default()
                    .leaf = Some(value.to_string());
            }
            return;
        }
        if !current.order.iter().any(|s| s == part) {
            current.order.push(part.to_string());
        }
        current = current.children.entry(part.to_string()).or_default();
    }
}

fn materialise<'bump>(arena: &EventArena<'bump>, node: &Tree) -> Value<'bump> {
    let mut builder = ObjectBuilder::with_capacity(arena, node.order.len());
    for key in &node.order {
        let child = match node.children.get(key) {
            Some(c) => c,
            None => continue,
        };
        if let Some(leaf) = &child.leaf {
            builder.push_str(key, Value::String(arena.alloc_str(leaf)));
        } else {
            builder.push_str(key, materialise(arena, child));
        }
    }
    builder.finish()
}

/// Replace `.` inside `(?P<…>)` / `(?<…>)` capture names with `__DOT__`
/// so the regex engine accepts the pattern. Other parts of the pattern
/// pass through verbatim.
fn mangle_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let opener = if bytes[i..].starts_with(b"(?P<") {
            Some(4)
        } else if bytes[i..].starts_with(b"(?<") {
            Some(3)
        } else {
            None
        };

        if let Some(prefix_len) = opener {
            out.push_str(&pattern[i..i + prefix_len]);
            i += prefix_len;
            let start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            let name = &pattern[start..i];
            out.push_str(&name.replace('.', DOT_MARKER));
        // Closing '>' (if any) emitted on the next iteration.
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn demangle(s: &str) -> String {
    s.replace(DOT_MARKER, ".")
}

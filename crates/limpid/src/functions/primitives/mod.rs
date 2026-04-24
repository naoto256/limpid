//! Flat-namespace primitives.
//!
//! Functions in this subtree are schema-agnostic: format parsers
//! (`parse_json`, `parse_kv`), regex helpers, hashing, timestamp
//! formatting, table lookup, GeoIP, string helpers, and so on. They are
//! registered with no namespace and called in the DSL as bare
//! `name(args)` — see [`crate::functions::FunctionRegistry::register`].
//!
//! Schema-specific functions (`syslog.*`, `cef.*`, …) live in sibling
//! modules next to this one. See `design-principles.md` Principle 5.
//!
//! The split into one-file-per-function was introduced in v0.3.0 Block 4
//! to stop `functions/mod.rs` from becoming a megafile once the set of
//! primitives grew past a dozen entries.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;

use super::FunctionRegistry;
use super::table::TableStore;

// One module per primitive function. Each module exposes a `register(reg, ...)`
// entry point that inserts its function(s) into the registry.
pub mod append;
pub mod contains;
pub mod csv_parse;
pub mod find_by;
pub mod format;
pub mod geoip;
pub mod hashes;
pub mod len;
pub mod lower;
pub mod parse_json;
pub mod parse_kv;
pub mod regex_extract;
pub mod regex_match;
pub mod regex_parse;
pub mod regex_replace;
pub mod strftime;
pub mod table;
pub mod to_int;
pub mod to_json;
pub mod upper;

/// Register all flat-namespace primitives.
pub fn register(reg: &mut FunctionRegistry, table_store: TableStore) {
    append::register(reg);
    contains::register(reg);
    csv_parse::register(reg);
    find_by::register(reg);
    lower::register(reg);
    upper::register(reg);
    regex_match::register(reg);
    regex_extract::register(reg);
    regex_parse::register(reg);
    regex_replace::register(reg);
    to_int::register(reg);
    to_json::register(reg);
    table::register(reg, table_store);
    geoip::register(reg);
    hashes::register(reg);
    len::register(reg);
    format::register(reg);
    strftime::register(reg);
    parse_json::register(reg);
    parse_kv::register(reg);
}

// ---------------------------------------------------------------------------
// Shared helpers used by multiple primitive implementations.
// ---------------------------------------------------------------------------

/// Coerce a JSON value to a string the way DSL arithmetic / string
/// primitives expect: string-through, null-as-empty, everything else via
/// Display. Exposed here because several primitive modules need the
/// exact same coercion.
pub(crate) fn val_to_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Thread-local regex cache used by `regex_match` / `regex_extract` /
/// `regex_replace`. The cache is bounded — when full, it is cleared
/// wholesale rather than LRU-evicted, which is simpler and fine for the
/// expected usage (a handful of distinct patterns per pipeline).
const REGEX_CACHE_MAX: usize = 256;

pub(crate) fn get_cached_regex(pattern: &str) -> Result<regex_lite::Regex, regex_lite::Error> {
    thread_local! {
        static CACHE: RefCell<HashMap<String, regex_lite::Regex>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex_lite::Regex::new(pattern)?;
        if cache.len() >= REGEX_CACHE_MAX {
            cache.clear();
        }
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// Parse `+HH:MM` / `-HH:MM` (or `+HHMM` / `-HHMM`) into a `FixedOffset`.
/// Used by `strftime` for the optional timezone argument.
pub(crate) fn parse_fixed_offset(s: &str) -> Option<chrono::FixedOffset> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    let (h_str, m_str) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        rest.split_at(2)
    } else {
        return None;
    };
    let h: i32 = h_str.parse().ok()?;
    let m: i32 = m_str.parse().ok()?;
    let secs = sign * (h * 3600 + m * 60);
    chrono::FixedOffset::east_opt(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fixed_offset_variants() {
        assert_eq!(
            parse_fixed_offset("+09:00").map(|o| o.local_minus_utc()),
            Some(9 * 3600)
        );
        assert_eq!(
            parse_fixed_offset("-05:30").map(|o| o.local_minus_utc()),
            Some(-(5 * 3600 + 30 * 60))
        );
        assert_eq!(
            parse_fixed_offset("+0900").map(|o| o.local_minus_utc()),
            Some(9 * 3600)
        );
        assert!(parse_fixed_offset("UTC").is_none());
        assert!(parse_fixed_offset("").is_none());
        assert!(parse_fixed_offset("09:00").is_none());
    }
}

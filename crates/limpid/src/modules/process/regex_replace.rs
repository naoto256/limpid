//! regex_replace: replaces all matches of a regex pattern in event.egress.
//!
//! Usage: `process regex_replace("pattern", "replacement")`
//!
//! The replacement string supports capture group references: `$1`, `$2`, etc.

use std::cell::RefCell;
use std::collections::HashMap;

use bytes::Bytes;
use regex_lite::Regex;

use crate::event::Event;
use crate::modules::ProcessError;

// Thread-local regex cache with size limit
const REGEX_CACHE_MAX: usize = 256;

thread_local! {
    static REGEX_CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

fn get_cached_regex(pattern: &str) -> Result<Regex, ProcessError> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = Regex::new(pattern)
            .map_err(|e| ProcessError::Failed(format!("regex_replace: invalid pattern: {}", e)))?;
        if cache.len() >= REGEX_CACHE_MAX {
            cache.clear();
        }
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

pub fn apply(mut event: Event, args: &[serde_json::Value]) -> Result<Event, ProcessError> {
    let pattern = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
        ProcessError::Failed("regex_replace: first argument (pattern) must be a string".into())
    })?;

    let replacement = args.get(1).and_then(|v| v.as_str()).ok_or_else(|| {
        ProcessError::Failed("regex_replace: second argument (replacement) must be a string".into())
    })?;

    let re = get_cached_regex(pattern)?;

    let msg = String::from_utf8_lossy(&event.egress);
    let replaced = re.replace_all(&msg, replacement);

    if let std::borrow::Cow::Owned(s) = replaced {
        event.egress = Bytes::from(s);
    }
    // Cow::Borrowed means no match — egress unchanged, no allocation

    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::net::SocketAddr;

    fn make_event(msg: &str) -> Event {
        Event::new(
            Bytes::from(msg.to_string()),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    fn str_args(args: &[&str]) -> Vec<Value> {
        args.iter().map(|s| Value::String(s.to_string())).collect()
    }

    #[test]
    fn test_basic_replace() {
        let e = make_event("hello world");
        let result = apply(e, &str_args(&["world", "rust"])).unwrap();
        assert_eq!(&*result.egress, b"hello rust");
    }

    #[test]
    fn test_replace_all_occurrences() {
        let e = make_event("foo bar foo baz foo");
        let result = apply(e, &str_args(&["foo", "qux"])).unwrap();
        assert_eq!(&*result.egress, b"qux bar qux baz qux");
    }

    #[test]
    fn test_capture_group() {
        let e = make_event("date=2026-04-15 time=04:23:17");
        let result = apply(
            e,
            &str_args(&[r"date=(\d{4})-(\d{2})-(\d{2})", "date=$1/$2/$3"]),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&result.egress).as_ref(),
            "date=2026/04/15 time=04:23:17"
        );
    }

    #[test]
    fn test_no_match_unchanged() {
        let e = make_event("hello world");
        let result = apply(e, &str_args(&["xyz", "replaced"])).unwrap();
        assert_eq!(&*result.egress, b"hello world");
    }

    #[test]
    fn test_missing_pattern() {
        let e = make_event("hello");
        assert!(apply(e, &[]).is_err());
    }

    #[test]
    fn test_missing_replacement() {
        let e = make_event("hello");
        assert!(apply(e, &str_args(&["hello"])).is_err());
    }

    #[test]
    fn test_invalid_regex() {
        let e = make_event("hello");
        assert!(apply(e, &str_args(&["(unclosed", "x"])).is_err());
    }
}

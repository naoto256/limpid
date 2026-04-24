//! parse_kv: parses key=value pairs from the egress into the event workspace.
//!
//! Handles common syslog key-value formats used by FortiGate, Palo Alto, etc.
//!
//! Supports:
//! - `key=value` (space-delimited)
//! - `key="quoted value"` (quoted values with spaces)
//! - `key=value1,value2` (comma in unquoted values is included)
//!
//! All parsed key-value pairs are stored under `workspace.*`.
//! The original egress is preserved.

use serde_json::Value;

use crate::event::Event;
use crate::modules::ProcessError;

pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    let msg = String::from_utf8_lossy(&event.egress).into_owned();
    let pairs = parse_kv_pairs(&msg);

    for (key, value) in pairs {
        event.workspace.insert(key, Value::String(value));
    }

    Ok(event)
}

/// Safe substring from byte offsets, handling potential non-UTF-8 boundaries.
fn safe_slice(input: &str, start: usize, end: usize) -> &str {
    let s = start.min(input.len());
    let e = end.min(input.len());
    // Find valid char boundaries
    let s = (s..=e).find(|&i| input.is_char_boundary(i)).unwrap_or(e);
    let e = (s..=e).rfind(|&i| input.is_char_boundary(i)).unwrap_or(s);
    &input[s..e]
}

fn parse_kv_pairs(input: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Skip whitespace
        while i < len && bytes[i] == b' ' {
            i += 1;
        }
        if i >= len {
            break;
        }

        // Read key: sequence of non-= non-space chars
        let key_start = i;
        while i < len && bytes[i] != b'=' && bytes[i] != b' ' {
            i += 1;
        }

        if i >= len || bytes[i] != b'=' {
            // No '=' found, skip this token
            while i < len && bytes[i] != b' ' {
                i += 1;
            }
            continue;
        }

        let key = safe_slice(input, key_start, i);
        i += 1; // skip '='

        if i >= len {
            pairs.push((key.to_string(), String::new()));
            break;
        }

        // Read value
        let value = if bytes[i] == b'"' {
            // Quoted value
            i += 1; // skip opening quote
            let val_start = i;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2; // skip escaped char
                } else {
                    i += 1;
                }
            }
            let val = safe_slice(input, val_start, i);
            if i < len {
                i += 1; // skip closing quote
            }
            val.to_string()
        } else {
            // Unquoted value: read until space
            let val_start = i;
            while i < len && bytes[i] != b' ' {
                i += 1;
            }
            safe_slice(input, val_start, i).to_string()
        };

        if !key.is_empty() {
            pairs.push((key.to_string(), value));
        }
    }

    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_event(msg: &str) -> Event {
        Event::new(
            Bytes::from(msg.to_string()),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn test_basic_kv() {
        let event = make_event("src=10.0.0.1 dst=192.168.1.1 action=accept");
        let result = apply(event).unwrap();
        assert_eq!(result.workspace["src"], Value::String("10.0.0.1".into()));
        assert_eq!(result.workspace["dst"], Value::String("192.168.1.1".into()));
        assert_eq!(result.workspace["action"], Value::String("accept".into()));
    }

    #[test]
    fn test_quoted_value() {
        let event = make_event(r#"msg="login failed" user=admin src=10.0.0.1"#);
        let result = apply(event).unwrap();
        assert_eq!(
            result.workspace["msg"],
            Value::String("login failed".into())
        );
        assert_eq!(result.workspace["user"], Value::String("admin".into()));
    }

    #[test]
    fn test_fortinet_style() {
        let event = make_event(
            "date=2026-04-15 time=10:30:00 devname=FW01 srcip=10.0.0.1 dstip=192.168.1.1 action=deny",
        );
        let result = apply(event).unwrap();
        assert_eq!(result.workspace["date"], Value::String("2026-04-15".into()));
        assert_eq!(result.workspace["devname"], Value::String("FW01".into()));
        assert_eq!(result.workspace["action"], Value::String("deny".into()));
    }

    #[test]
    fn test_empty_value() {
        let event = make_event("key1= key2=value2");
        let result = apply(event).unwrap();
        assert_eq!(result.workspace["key1"], Value::String("".into()));
        assert_eq!(result.workspace["key2"], Value::String("value2".into()));
    }

    #[test]
    fn test_non_kv_tokens_skipped() {
        let event = make_event("garbage src=10.0.0.1 more_garbage dst=1.2.3.4");
        let result = apply(event).unwrap();
        assert_eq!(result.workspace["src"], Value::String("10.0.0.1".into()));
        assert_eq!(result.workspace["dst"], Value::String("1.2.3.4".into()));
        assert!(!result.workspace.contains_key("garbage"));
    }
}

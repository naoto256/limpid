//! parse_cef: parses CEF (Common Event Format) messages into event fields.

use serde_json::Value;

use crate::event::Event;
use crate::modules::ProcessError;

/// Parse CEF (Common Event Format) from `raw` and expand into `fields`.
///
/// CEF format:
/// ```text
/// CEF:Version|Device Vendor|Device Product|Device Version|Signature ID|Name|Severity|Extensions
/// ```
///
/// Extensions are `key=value` pairs. Values containing `=` are delimited by
/// the next recognized key (keys are sequences of alphanumeric/underscore chars
/// followed by `=`).
pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    // Copy raw to owned String up front to avoid borrow conflict
    let raw = String::from_utf8_lossy(&event.raw).into_owned();

    // Find "CEF:" prefix (may be preceded by syslog header)
    let cef_start = raw
        .find("CEF:")
        .ok_or_else(|| ProcessError::Failed("no CEF header found".into()))?;

    let cef_body = &raw[cef_start + 4..]; // skip "CEF:"

    // Split header fields by '|' (first 7 pipes)
    let mut parts = Vec::new();
    let mut remaining = cef_body;
    for _ in 0..7 {
        if let Some(pos) = remaining.find('|') {
            parts.push(&remaining[..pos]);
            remaining = &remaining[pos + 1..];
        } else {
            return Err(ProcessError::Failed("incomplete CEF header".into()));
        }
    }
    // `remaining` is now the extensions string

    event.fields.insert("cef_version".into(), Value::String(parts[0].to_string()));
    event.fields.insert("device_vendor".into(), Value::String(parts[1].to_string()));
    event.fields.insert("device_product".into(), Value::String(parts[2].to_string()));
    event.fields.insert("device_version".into(), Value::String(parts[3].to_string()));
    event.fields.insert("signature_id".into(), Value::String(parts[4].to_string()));
    event.fields.insert("name".into(), Value::String(parts[5].to_string()));
    event.fields.insert("cef_severity".into(), Value::String(parts[6].to_string()));

    // Parse extensions: key=value pairs
    parse_cef_extensions(remaining, &mut event);

    Ok(event)
}

/// Parse CEF extension key=value pairs.
///
/// Keys are alphanumeric/underscore sequences ending with `=`.
/// Values run until the next `key=` pattern or end of string.
fn parse_cef_extensions(extensions: &str, event: &mut Event) {
    if extensions.is_empty() {
        return;
    }

    // Collect (key, start_of_value) pairs by scanning for `key=` patterns
    let mut key_positions: Vec<(String, usize)> = Vec::new();
    let bytes = extensions.as_bytes();
    let mut i = 0;

    // First key starts at position 0
    while i < bytes.len() {
        // Try to read a key: [a-zA-Z0-9_]+ followed by '='
        let key_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' && i > key_start {
            let key = &extensions[key_start..i];
            i += 1; // skip '='
            key_positions.push((key.to_string(), i));

            // Skip value: advance until we hit the next key=
            // We detect a new key by: space + alphanumeric/_ sequence + '='
            while i < bytes.len() {
                if bytes[i] == b' ' {
                    // Look ahead for potential key
                    let lookahead = i + 1;
                    let mut j = lookahead;
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'=' && j > lookahead {
                        // Found next key, value ends here
                        break;
                    }
                }
                i += 1;
            }
        } else {
            // Not a valid key, skip character
            i += 1;
        }
    }

    // Extract values
    for idx in 0..key_positions.len() {
        let (ref key, val_start) = key_positions[idx];
        let val_end = if idx + 1 < key_positions.len() {
            // Value ends at the space before the next key
            // Find the space before next key's start
            let next_val_start = key_positions[idx + 1].1;
            let next_key_len = key_positions[idx + 1].0.len();
            // next key position: next_val_start - key_len - 1(=) - 1(space)
            next_val_start.saturating_sub(next_key_len + 2).max(val_start)
        } else {
            extensions.len()
        };

        let value = extensions[val_start..val_end].trim();
        event.fields.insert(key.clone(), Value::String(value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_event(raw: &str) -> Event {
        Event::new(
            Bytes::from(raw.to_string()),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn test_parse_cef_basic() {
        let event = make_event(
            "CEF:0|Fortinet|FortiGate|7.0|1234|Firewall event|5|src=10.0.0.1 dst=10.0.0.2 act=deny",
        );
        let result = apply(event).unwrap();

        assert_eq!(result.fields["cef_version"], Value::String("0".into()));
        assert_eq!(result.fields["device_vendor"], Value::String("Fortinet".into()));
        assert_eq!(result.fields["device_product"], Value::String("FortiGate".into()));
        assert_eq!(result.fields["signature_id"], Value::String("1234".into()));
        assert_eq!(result.fields["src"], Value::String("10.0.0.1".into()));
        assert_eq!(result.fields["dst"], Value::String("10.0.0.2".into()));
        assert_eq!(result.fields["act"], Value::String("deny".into()));
    }

    #[test]
    fn test_parse_cef_with_syslog_header() {
        let event = make_event(
            "<134>CEF:0|Security|IDS|1.0|100|Attack|8|src=192.168.1.1",
        );
        let result = apply(event).unwrap();

        assert_eq!(result.fields["device_vendor"], Value::String("Security".into()));
        assert_eq!(result.fields["src"], Value::String("192.168.1.1".into()));
    }

    #[test]
    fn test_parse_cef_no_extensions() {
        let event = make_event("CEF:0|A|B|1.0|1|Test|3|");
        let result = apply(event).unwrap();

        assert_eq!(result.fields["device_vendor"], Value::String("A".into()));
        assert!(!result.fields.contains_key("src"));
    }

    #[test]
    fn test_parse_cef_missing_header() {
        let event = make_event("not a CEF message");
        assert!(apply(event).is_err());
    }
}

//! parse_json: parses JSON messages and expands top-level keys into event fields.

use serde_json::Value;

use crate::event::Event;
use crate::modules::ProcessError;

/// Parse `message` (or `raw` if message is identical to raw) as JSON
/// and expand top-level keys into `fields`.
pub fn apply(event: Event) -> Result<Event, ProcessError> {
    let text = String::from_utf8_lossy(&event.message);
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ProcessError::Failed(format!("JSON parse error: {}", e)))?;

    let mut event = event;
    if let Value::Object(map) = parsed {
        for (key, value) in map {
            event.fields.insert(key, value);
        }
    } else {
        // Non-object JSON: store under "_json" key
        event.fields.insert("_json".into(), parsed);
    }

    Ok(event)
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
    fn test_parse_json_object() {
        let event = make_event(r#"{"host":"web01","level":"error","msg":"timeout"}"#);
        let result = apply(event).unwrap();

        assert_eq!(result.fields["host"], Value::String("web01".into()));
        assert_eq!(result.fields["level"], Value::String("error".into()));
        assert_eq!(result.fields["msg"], Value::String("timeout".into()));
    }

    #[test]
    fn test_parse_json_nested() {
        let event = make_event(r#"{"host":"web01","meta":{"region":"ap-northeast-1"}}"#);
        let result = apply(event).unwrap();

        assert_eq!(result.fields["host"], Value::String("web01".into()));
        assert!(result.fields["meta"].is_object());
    }

    #[test]
    fn test_parse_json_array() {
        let event = make_event(r#"[1,2,3]"#);
        let result = apply(event).unwrap();

        assert!(result.fields.contains_key("_json"));
        assert!(result.fields["_json"].is_array());
    }

    #[test]
    fn test_parse_json_invalid() {
        let event = make_event("not json at all");
        assert!(apply(event).is_err());
    }
}

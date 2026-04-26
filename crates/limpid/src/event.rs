//! Event: the internal message representation flowing through pipelines.
//!
//! Each event has an immutable `ingress` (bytes as received from the input)
//! and a mutable `egress` (bytes that will be handed to the output), plus
//! typed metadata and a free-form `workspace` (pipeline-local scratch
//! namespace). `ingress` / `egress` frame the hop contract: what came in,
//! what goes out.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::net::SocketAddr;

use crate::dsl::value::Value;
use crate::dsl::value_json::{json_to_value, value_to_json};

#[derive(Debug, Clone)]
pub struct Event {
    /// Wall-clock time at which this hop received the event. Set once
    /// by the input layer (`Event::new` → `Utc::now()`); never overwritten
    /// from payload contents (Principle 2: input is dumb transport).
    /// Source-claimed event time, when extractable, lives in workspace
    /// fields populated by parser primitives — typically captured under
    /// a per-schema namespace (`workspace.syslog = syslog.parse(ingress)`
    /// then `workspace.syslog.timestamp`; CEF's `rt` extension surfaces
    /// as `workspace.cef.rt` after `workspace.cef = cef.parse(...)`).
    pub received_at: DateTime<Utc>,
    pub source: SocketAddr,
    pub ingress: Bytes,
    pub egress: Bytes,
    pub workspace: HashMap<String, Value>,
}

impl Event {
    pub fn new(ingress: Bytes, source: SocketAddr) -> Self {
        Self {
            received_at: Utc::now(),
            source,
            egress: ingress.clone(),
            ingress,
            workspace: HashMap::new(),
        }
    }

    /// Serialize the event to a JSON Value via the marker / escape
    /// boundary rules in `dsl::value_json`. Non-UTF-8 `ingress` /
    /// `egress` content surfaces as `Value::Bytes` and is encoded with
    /// the `$bytes_b64` marker; UTF-8-clean content stays a plain JSON
    /// string. Workspace values flow through the same boundary.
    pub fn to_json_value(&self) -> JsonValue {
        let mut map = serde_json::Map::new();
        // Wire form is unix nanoseconds (i64) — matches OTLP
        // `time_unix_nano` and is lossless against RFC3339. Receivers
        // (`inject --json`, downstream tooling) parse the integer back
        // into a `Value::Timestamp`.
        let nanos = self.received_at.timestamp_nanos_opt().unwrap_or(0);
        map.insert("received_at".into(), JsonValue::Number(nanos.into()));
        map.insert("source".into(), JsonValue::String(self.source.to_string()));
        map.insert("ingress".into(), bytes_to_json(&self.ingress));
        map.insert("egress".into(), bytes_to_json(&self.egress));
        if !self.workspace.is_empty() {
            let ws: crate::dsl::Map = self
                .workspace
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            // Workspace serialization can fail only on non-finite floats;
            // surface as an empty placeholder rather than panicking, the
            // event itself stays diagnosable.
            let ws_json = value_to_json(&Value::Object(ws))
                .unwrap_or(JsonValue::Object(serde_json::Map::new()));
            map.insert("workspace".into(), ws_json);
        }
        JsonValue::Object(map)
    }

    /// Serialize the event to a JSON string.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(&self.to_json_value()).unwrap_or_default()
    }

    /// Deserialize an event from a JSON string. Inverse of
    /// [`to_json_string`]. Workspace values pass back through the
    /// JSON boundary so `$bytes_b64` markers rehydrate as
    /// `Value::Bytes`.
    pub fn from_json(json_str: &str) -> Option<Self> {
        let v: JsonValue = serde_json::from_str(json_str).ok()?;
        let ingress = json_to_bytes(v.get("ingress")?)?;
        let source_str = v.get("source")?.as_str()?;
        let source: SocketAddr = source_str.parse().ok()?;
        // i64 unix nanoseconds — the wire form documented in `to_json_value`.
        // Pre-0.5 RFC3339 captures need to be migrated before replay.
        let received_at = v
            .get("received_at")
            .and_then(|v| v.as_i64())
            .map(chrono::DateTime::<Utc>::from_timestamp_nanos)?;
        let egress = v
            .get("egress")
            .and_then(json_to_bytes)
            .unwrap_or_else(|| ingress.clone());

        let mut event = Self {
            received_at,
            source,
            ingress,
            egress,
            workspace: HashMap::new(),
        };

        if let Some(workspace) = v.get("workspace")
            && let Ok(Value::Object(map)) = json_to_value(workspace)
        {
            for (k, val) in map {
                event.workspace.insert(k, val);
            }
        }

        Some(event)
    }
}

/// Serialize a byte buffer for the event's JSON form. UTF-8-clean
/// payloads become plain JSON strings (the historical limpid shape);
/// non-UTF-8 payloads surface as a `$bytes_b64` marker so binary
/// `ingress` / `egress` round-trips through tap and persistence
/// without corruption.
fn bytes_to_json(b: &Bytes) -> JsonValue {
    match std::str::from_utf8(b) {
        Ok(s) => JsonValue::String(s.to_string()),
        Err(_) => value_to_json(&Value::Bytes(b.clone())).unwrap_or(JsonValue::Null),
    }
}

/// Inverse of [`bytes_to_json`]: accept either a plain JSON string
/// (UTF-8-clean) or a `$bytes_b64` marker object.
fn json_to_bytes(v: &JsonValue) -> Option<Bytes> {
    if let Some(s) = v.as_str() {
        return Some(Bytes::from(s.to_string()));
    }
    if let Ok(Value::Bytes(b)) = json_to_value(v) {
        return Some(b);
    }
    None
}

//! Event: the internal message representation flowing through pipelines.
//!
//! Each event has an immutable `ingress` (bytes as received from the input)
//! and a mutable `egress` (bytes that will be handed to the output), plus
//! typed metadata and a free-form `workspace` (pipeline-local scratch
//! namespace). `ingress` / `egress` frame the hop contract: what came in,
//! what goes out.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: DateTime<Utc>,
    pub source: SocketAddr,
    pub facility: Option<u8>,
    pub severity: Option<u8>,
    pub ingress: Bytes,
    pub egress: Bytes,
    pub workspace: HashMap<String, Value>,
}

impl Event {
    pub fn new(ingress: Bytes, source: SocketAddr) -> Self {
        Self {
            timestamp: Utc::now(),
            source,
            facility: None,
            severity: None,
            egress: ingress.clone(),
            ingress,
            workspace: HashMap::new(),
        }
    }

    /// Serialize the event to a JSON Value.
    pub fn to_json_value(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "timestamp".into(),
            Value::String(self.timestamp.to_rfc3339()),
        );
        map.insert("source".into(), Value::String(self.source.to_string()));
        if let Some(f) = self.facility {
            map.insert("facility".into(), Value::Number(f.into()));
        }
        if let Some(s) = self.severity {
            map.insert("severity".into(), Value::Number(s.into()));
        }
        map.insert(
            "ingress".into(),
            Value::String(String::from_utf8_lossy(&self.ingress).into_owned()),
        );
        map.insert(
            "egress".into(),
            Value::String(String::from_utf8_lossy(&self.egress).into_owned()),
        );
        if !self.workspace.is_empty() {
            map.insert(
                "workspace".into(),
                Value::Object(self.workspace.clone().into_iter().collect()),
            );
        }
        Value::Object(map)
    }

    /// Serialize the event to a JSON string.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(&self.to_json_value()).unwrap_or_default()
    }

    /// Deserialize an event from a JSON string.
    pub fn from_json(json_str: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json_str).ok()?;
        let ingress = v.get("ingress")?.as_str()?.to_string();
        let source_str = v.get("source")?.as_str()?;
        let source: SocketAddr = source_str.parse().ok()?;
        let timestamp = v
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))?;

        let mut event = Self {
            timestamp,
            source,
            facility: v
                .get("facility")
                .and_then(|v| v.as_u64())
                .and_then(|v| u8::try_from(v).ok()),
            severity: v
                .get("severity")
                .and_then(|v| v.as_u64())
                .and_then(|v| u8::try_from(v).ok()),
            ingress: Bytes::from(ingress.clone()),
            egress: Bytes::from(
                v.get("egress")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&ingress)
                    .to_string(),
            ),
            workspace: HashMap::new(),
        };

        if let Some(workspace) = v.get("workspace").and_then(|v| v.as_object()) {
            for (k, val) in workspace {
                event.workspace.insert(k.clone(), val.clone());
            }
        }

        Some(event)
    }
}

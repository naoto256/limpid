//! Event: the internal message representation flowing through pipelines.
//!
//! Each event carries an immutable `ingress` (bytes as received from
//! the input) and a mutable `egress` (bytes that will be handed to the
//! output), plus typed metadata and a free-form `workspace` (pipeline-
//! local scratch namespace). `ingress` / `egress` frame the hop
//! contract: what came in, what goes out.
//!
//! Two representations live side by side:
//!
//! - [`OwnedEvent`] — the boundary form. Heap-owned `workspace`
//!   ([`HashMap<String, OwnedValue>`]). Used wherever the event leaves
//!   pipeline-internal scope: channel sends between input/runtime/output,
//!   JSON persistence (tap, queue, `error_log`), the dead-letter queue
//!   context, and `--test-pipeline` setup.
//! - [`BorrowedEvent<'bump>`] — the per-event arena form. `workspace`
//!   is a [`bumpalo::collections::Vec<'_, (&'bump str, Value<'bump>)>`],
//!   so DSL evaluation/execution stays inside the arena and the entire
//!   tree (including all string keys) is freed in one chunk-group
//!   `dealloc` at end of event.
//!
//! Boundary conversions:
//!
//! - [`OwnedEvent::view_in`] — copy the workspace into the arena and
//!   produce a borrowed event. Called at `run_pipeline` entry.
//! - [`BorrowedEvent::to_owned`] — heap-allocate a fresh `OwnedEvent`
//!   from the borrowed form. Called at `run_pipeline` exit when an
//!   output is reached, and when a process-level error needs to land
//!   in the DLQ context (which holds an `OwnedEvent` for replay).

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::net::SocketAddr;

use crate::dsl::arena::EventArena;
use crate::dsl::value::{Map, OwnedValue, Value};
use crate::dsl::value_json::{json_to_value, value_to_json};

// ===========================================================================
// Owned (boundary) event
// ===========================================================================

#[derive(Debug, Clone)]
pub struct OwnedEvent {
    /// Wall-clock time at which this hop received the event. Set once
    /// by the input layer (`OwnedEvent::new` → `Utc::now()`); never
    /// overwritten from payload contents (Principle 2: input is dumb
    /// transport). Source-claimed event time, when extractable, lives
    /// in workspace fields populated by parser primitives — typically
    /// captured under a per-schema namespace
    /// (`workspace.syslog = syslog.parse(ingress)` then
    /// `workspace.syslog.timestamp`; CEF's `rt` extension surfaces as
    /// `workspace.cef.rt` after `workspace.cef = cef.parse(...)`).
    pub received_at: DateTime<Utc>,
    pub source: SocketAddr,
    pub ingress: Bytes,
    pub egress: Bytes,
    pub workspace: HashMap<String, OwnedValue>,
}

impl OwnedEvent {
    pub fn new(ingress: Bytes, source: SocketAddr) -> Self {
        Self {
            received_at: Utc::now(),
            source,
            egress: ingress.clone(),
            ingress,
            workspace: HashMap::new(),
        }
    }

    /// Copy this owned event into `arena` and return a [`BorrowedEvent`]
    /// view. Workspace string keys are alloc'd into the arena and each
    /// value is recursively viewed (see [`OwnedValue::view_in`]).
    /// `ingress` and `egress` are `Bytes` (refcounted), so cloning them
    /// across the boundary is cheap and there is no per-event payload
    /// alloc.
    pub fn view_in<'bump>(&self, arena: &EventArena<'bump>) -> BorrowedEvent<'bump> {
        let mut workspace =
            bumpalo::collections::Vec::with_capacity_in(self.workspace.len(), arena.bump());
        for (k, v) in &self.workspace {
            workspace.push((arena.alloc_str(k), v.view_in(arena)));
        }
        BorrowedEvent {
            received_at: self.received_at,
            source: self.source,
            ingress: self.ingress.clone(),
            egress: self.egress.clone(),
            workspace,
        }
    }

    /// Serialise the event to a JSON Value via the marker / escape
    /// boundary rules in `dsl::value_json`. Non-UTF-8 `ingress` /
    /// `egress` content surfaces as `OwnedValue::Bytes` and is encoded
    /// with the `$bytes_b64` marker; UTF-8-clean content stays a plain
    /// JSON string. Workspace values flow through the same boundary.
    pub fn to_json_value(&self) -> JsonValue {
        let mut map = serde_json::Map::new();
        // Wire form is unix nanoseconds (i64) — matches OTLP
        // `time_unix_nano` and is lossless against RFC3339. Receivers
        // (`inject --json`, downstream tooling) parse the integer back
        // into a `Value::Timestamp`.
        let nanos = self.received_at.timestamp_nanos_opt().unwrap_or(0);
        map.insert("received_at".into(), JsonValue::Number(nanos.into()));
        // Wire form mirrors the DSL: `source` is an object with `ip`
        // (String) and `port` (Int) since v0.5.6. The flat
        // `"source": "ip:port"` form prior versions emitted is no
        // longer accepted (`from_json` is strict to keep round-trip
        // semantics simple). JSONL files captured by 0.5.5 or earlier
        // need a one-shot `jq` migration before replay; see the
        // 0.5.6 CHANGELOG entry for the recipe.
        let mut source_obj = serde_json::Map::new();
        source_obj.insert("ip".into(), JsonValue::String(self.source.ip().to_string()));
        source_obj.insert("port".into(), JsonValue::Number(self.source.port().into()));
        map.insert("source".into(), JsonValue::Object(source_obj));
        map.insert("ingress".into(), bytes_to_json(&self.ingress));
        map.insert("egress".into(), bytes_to_json(&self.egress));
        if !self.workspace.is_empty() {
            let ws: Map = self
                .workspace
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            // Workspace serialization can fail only on non-finite
            // floats; surface as an empty placeholder rather than
            // panicking, the event itself stays diagnosable.
            let ws_json = value_to_json(&OwnedValue::Object(ws))
                .unwrap_or(JsonValue::Object(serde_json::Map::new()));
            map.insert("workspace".into(), ws_json);
        }
        JsonValue::Object(map)
    }

    /// Serialise the event to a JSON string.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(&self.to_json_value()).unwrap_or_default()
    }

    /// Deserialise an event from a JSON string. Inverse of
    /// [`to_json_string`]. Workspace values pass back through the
    /// JSON boundary so `$bytes_b64` markers rehydrate as
    /// `OwnedValue::Bytes`.
    pub fn from_json(json_str: &str) -> Option<Self> {
        let v: JsonValue = serde_json::from_str(json_str).ok()?;
        let ingress = json_to_bytes(v.get("ingress")?)?;
        // Source is the v0.5.6+ object form `{ip, port}` — matches the
        // DSL ident shape and what `to_json_value` emits. The legacy
        // flat-string form `"ip:port"` from earlier limpid versions is
        // not accepted; pre-1.0 breaking change documented in CHANGELOG.
        let source_obj = v.get("source")?.as_object()?;
        let ip_str = source_obj.get("ip")?.as_str()?;
        let port = source_obj.get("port")?.as_u64()?;
        if port > u16::MAX as u64 {
            return None;
        }
        let source: SocketAddr = format!("{}:{}", ip_str, port).parse().ok()?;
        // i64 unix nanoseconds — the wire form documented in
        // `to_json_value`. Pre-0.5 RFC3339 captures need to be
        // migrated before replay.
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
            && let Ok(OwnedValue::Object(map)) = json_to_value(workspace)
        {
            for (k, val) in map {
                event.workspace.insert(k, val);
            }
        }

        Some(event)
    }
}

/// Backwards-compatible alias for the pre-v0.6.0 public name. Most
/// internal call sites have migrated to the [`OwnedEvent`] /
/// [`BorrowedEvent`] split, but disk-queue replay, control-plane
/// inject, error_log, and tap subscribers still operate on the owned
/// form via this alias — kept for ergonomic continuity at those
/// boundary points rather than as a transitional shim.
pub type Event = OwnedEvent;

// ===========================================================================
// Borrowed (per-event arena) event
// ===========================================================================

/// Per-event arena form of the runtime event. Fresh on every entry to
/// [`crate::pipeline::run_pipeline`]; dropped when the arena drops at
/// end of event, releasing the entire workspace tree in a single
/// chunk-group `dealloc`.
///
/// Semantics mirror [`OwnedEvent`]:
///
/// - `received_at` / `source` — typed metadata, scalar, copy-cheap.
/// - `ingress` / `egress` — `bytes::Bytes`. These are reference-counted
///   buffers, so handing them across the boundary is a refcount bump,
///   not a copy. They are NOT alloc'd inside `arena`, by design — the
///   per-event arena's win is on the `Value` tree (string keys, object
///   slices, primitive results), not on the byte payload, which lives
///   one Arc-level deeper.
/// - `workspace` — `bumpalo::collections::Vec<(&'bump str, Value<'bump>)>`.
///   Insertion order preserved by construction, lookup is linear scan.
///   At typical limpid object sizes (≤30 keys) this beats `IndexMap`'s
///   hash + entry-table indirection on a per-event basis (see the
///   v0.6.0 baseline — `IndexMap` ops were 11.8% on-CPU).
pub struct BorrowedEvent<'bump> {
    pub received_at: DateTime<Utc>,
    pub source: SocketAddr,
    pub ingress: Bytes,
    pub egress: Bytes,
    pub workspace: bumpalo::collections::Vec<'bump, (&'bump str, Value<'bump>)>,
}

impl<'bump> BorrowedEvent<'bump> {
    /// Heap-allocate a fresh [`OwnedEvent`] from this borrowed view.
    /// Called at `run_pipeline` exit and at error path setup
    /// (`ErroredEventContext` holds an `OwnedEvent` because the DLQ
    /// outlives the per-event arena).
    pub fn to_owned(&self) -> OwnedEvent {
        let mut workspace = HashMap::with_capacity(self.workspace.len());
        for (k, v) in self.workspace.iter() {
            workspace.insert((*k).to_string(), v.to_owned_value());
        }
        OwnedEvent {
            received_at: self.received_at,
            source: self.source,
            ingress: self.ingress.clone(),
            egress: self.egress.clone(),
            workspace,
        }
    }

    /// Return the workspace value bound to `key`, if any. Linear scan
    /// in insertion order.
    pub fn workspace_get(&self, key: &str) -> Option<Value<'bump>> {
        self.workspace
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
    }

    /// Insert or replace the workspace entry for `key`. The key is
    /// expected to already live in the arena — call sites that hold a
    /// `String` should `arena.alloc_str(...)` first; for ergonomics
    /// see [`Self::workspace_set_str`].
    pub fn workspace_set(&mut self, key: &'bump str, value: Value<'bump>) {
        if let Some(slot) = self
            .workspace
            .iter_mut()
            .find(|(k, _)| *k == key)
        {
            slot.1 = value;
        } else {
            self.workspace.push((key, value));
        }
    }

    /// Insert or replace the workspace entry for `key`, copying the
    /// key into the arena first.
    pub fn workspace_set_str(
        &mut self,
        arena: &EventArena<'bump>,
        key: &str,
        value: Value<'bump>,
    ) {
        if let Some(slot) = self.workspace.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.workspace.push((arena.alloc_str(key), value));
        }
    }

    /// Remove the workspace entry for `key` and return its value.
    pub fn workspace_remove(&mut self, key: &str) -> Option<Value<'bump>> {
        if let Some(idx) = self.workspace.iter().position(|(k, _)| *k == key) {
            Some(self.workspace.remove(idx).1)
        } else {
            None
        }
    }
}

// ===========================================================================
// JSON ingress/egress helpers (shared with OwnedEvent boundary)
// ===========================================================================

/// Serialize a byte buffer for the event's JSON form. UTF-8-clean
/// payloads become plain JSON strings (the historical limpid shape);
/// non-UTF-8 payloads surface as a `$bytes_b64` marker so binary
/// `ingress` / `egress` round-trips through tap and persistence
/// without corruption.
fn bytes_to_json(b: &Bytes) -> JsonValue {
    match std::str::from_utf8(b) {
        Ok(s) => JsonValue::String(s.to_string()),
        Err(_) => value_to_json(&OwnedValue::Bytes(b.clone())).unwrap_or(JsonValue::Null),
    }
}

/// Inverse of [`bytes_to_json`]: accept either a plain JSON string
/// (UTF-8-clean) or a `$bytes_b64` marker object.
fn json_to_bytes(v: &JsonValue) -> Option<Bytes> {
    if let Some(s) = v.as_str() {
        return Some(Bytes::from(s.to_string()));
    }
    if let Ok(OwnedValue::Bytes(b)) = json_to_value(v) {
        return Some(b);
    }
    None
}

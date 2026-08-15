//! Event: the internal message representation flowing through pipelines.
//!
//! Each event carries an immutable UUIDv7 `key`, an immutable `ingress`
//! (bytes as received from the input), and a mutable `egress` (bytes that
//! will be handed to the output), plus typed metadata and a free-form
//! `workspace` (pipeline-local scratch namespace). `ingress` / `egress`
//! frame the hop contract: what came in, what goes out.
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
use std::sync::Arc;

use crate::dsl::arena::EventArena;
use crate::dsl::value::{Map, OwnedValue, Value};
use crate::dsl::value_json::{json_to_value, value_to_json};

// ===========================================================================
// Ack token — input → pipeline-worker-completion ack
// ===========================================================================
//
// Tail / journal inputs persist a cursor (file offset / journal cursor) so a
// daemon restart resumes near where it left off. Pre-fix that cursor was
// advanced the instant `tx.send(event).await` succeeded — i.e. as soon as the
// event reached the pipeline-worker channel. If the daemon crashed before the
// worker ran the pipeline, the cursor on disk pointed PAST events that had
// never been processed, and a restart silently skipped them.
//
// The fix moves the cursor advance to "pipeline worker finished with the
// event". The token below is the carrier: a `Drop`-fired ack that the input
// embeds in each event it emits, and that fires automatically when the last
// reference to the event is released — typically when `process_event` returns
// in `runtime::run_pipeline_workers`. All termination shapes (Finished,
// Dropped, Errored, even a panic that unwinds the worker task) ack the same
// way because the mechanism is `Drop`, not an explicit call.
//
// Why `Drop`-fired rather than explicit `event.ack()`:
//   - Adds nothing to the pipeline code path (no plumbing through every
//     branch, no risk of missing a termination variant on a future change).
//   - Naturally fans out: if the same `OwnedEvent` is shared across multiple
//     fan-in workers via `&Event`, the single Arc fires once on the outer
//     scope drop — i.e. after every worker has run.
//   - Naturally degrades for inputs that don't use it: `OwnedEvent::new`
//     constructs with `ack: None`, and `drop_ack()` is a no-op.
//
// The token is held under an `Arc` so cloning is cheap when the runtime
// clones an event for a DLQ side-band; only the LAST surviving clone fires
// the ack. In practice the DLQ write completes inside `process_event` (the
// `write_errored_to_dlq` call is `.await`-ed) so this currently matters only
// for the disk-queue persist path (see below).
//
// Disk-queue path: when `pipeline::run_pipeline` routes an event to a disk-
// backed output, the runtime hands `QueueSender::send` a freshly-owned
// `Event` (built via `BorrowedEvent::to_owned()` with `ack: None`). The
// disk-queue copy does NOT extend the input-side ack; instead the queue's
// own ack lifecycle (per `QueueAckHandle` / `ack_to`) advances the disk
// cursor when each event's `consume` disposition resolves. The disk queue
// flushes each segment write but does not call an explicit fsync — closing
// that final durability gap is operator-territory (see
// `docs/operations/error-log.md` for the recovery-readiness contract).

/// Acknowledgement message sent back from the pipeline worker to the input.
///
/// Carries the input-defined position (typically a file offset for tail or a
/// journald cursor for journal) so the input's ack-reader task can advance
/// its watermark and persist the new position to its state file.
#[derive(Debug, Clone)]
pub enum AckPosition {
    /// Byte offset within a tail'd file, namespaced by the tail input's
    /// `generation` counter. The generation bumps on every rotation /
    /// truncation, so a late ack from a worker still holding an
    /// `AckHandle` for the previous file is detectable by the input and
    /// silently dropped — preventing it from poisoning the post-rotation
    /// watermark (a silent data-loss path: a stale ack interpreted under
    /// the new file's byte namespace would mark bytes 0..old_offset as
    /// already-processed on the next start).
    Offset { generation: u64, offset: u64 },
    /// Opaque cursor token from a systemd journal entry. Constructed
    /// only when the `journal` cargo feature is enabled. Cursors are
    /// globally monotonic within a boot ID (and uniquely identify an
    /// entry across boots), so they do not need a generation namespace
    /// the way file offsets do.
    #[allow(dead_code)]
    Cursor(String),
}

/// Drop-fired ack handle. Constructed by the input layer and embedded into
/// the event; when the last reference to the containing event is released,
/// the handle's `Drop` impl sends the carried `AckPosition` over the input's
/// ack channel.
///
/// The send is fire-and-forget (`try_send`): if the input has already shut
/// down and the receiver was dropped, the ack is harmlessly lost — the input
/// will restart at the last persisted (older-or-equal) watermark and re-read
/// the event, which is the at-least-once contract we want anyway.
#[derive(Debug)]
pub struct AckHandle {
    /// Interior mutability so the input's send-failure path can `disarm`
    /// through a shared `Arc`. `std::sync::Mutex<Option<_>>` is cheap for
    /// a one-shot signal — contention is impossible by construction (the
    /// only writer is the disarm call and the only reader is Drop).
    position: std::sync::Mutex<Option<AckPosition>>,
    /// `tokio::sync::mpsc::UnboundedSender` — unbounded because the ack
    /// channel must never apply backpressure on the pipeline worker; if it
    /// did, a slow ack-reader could deadlock the whole pipeline. Memory
    /// growth is bounded by the input → pipeline channel queue depth, which
    /// is already a configurable backpressure point (`queue_size`).
    tx: tokio::sync::mpsc::UnboundedSender<AckPosition>,
}

impl AckHandle {
    pub fn new(position: AckPosition, tx: tokio::sync::mpsc::UnboundedSender<AckPosition>) -> Self {
        Self {
            position: std::sync::Mutex::new(Some(position)),
            tx,
        }
    }

    /// Cancel the pending ack so `Drop` will not fire. Used by inputs on
    /// the un-recoverable send-failure path: the event never reached the
    /// pipeline, so advancing the cursor based on its position would
    /// silently drop the line. The line will be re-read on the next poll
    /// once the input rewinds its read offset.
    pub fn disarm(&self) {
        if let Ok(mut g) = self.position.lock() {
            *g = None;
        }
    }
}

impl Drop for AckHandle {
    fn drop(&mut self) {
        let pos = self.position.lock().ok().and_then(|mut g| g.take());
        if let Some(pos) = pos {
            // Receiver-gone is the expected shutdown shape; ignore.
            let _ = self.tx.send(pos);
        }
    }
}

// ===========================================================================
// Owned (boundary) event
// ===========================================================================

#[derive(Debug, Clone)]
pub struct OwnedEvent {
    /// Immutable event identity, minted once at the input boundary before
    /// fan-out and retained by clones, persistence, and replay.
    pub key: uuid::Uuid,
    /// Wall-clock time at which this hop received the event. Set once
    /// by the input layer (`OwnedEvent::new` → `Utc::now()`); never
    /// overwritten from payload contents (Principle 2: input is dumb
    /// transport). Source-claimed event time, when extractable, lives
    /// in workspace fields populated by parser primitives — typically
    /// captured under a per-schema namespace
    /// (`workspace.syslog = syslog.parse(ingress)` then
    /// `workspace.syslog.timestamp`; CEF's `rt` extension surfaces as
    /// `workspace.cef.extension.rt` after
    /// `workspace.cef = cef.parse(...)`).
    pub received_at: DateTime<Utc>,
    pub source: SocketAddr,
    pub ingress: Bytes,
    pub egress: Bytes,
    pub workspace: HashMap<String, OwnedValue>,
    /// Optional drop-fired ack used by inputs that persist a cursor
    /// (tail / journal). `None` for inputs that don't have a position to
    /// advance (syslog, OTLP, unix_socket, …) and for events synthesised
    /// by the boundary (DLQ replay, control-plane inject, `--test-pipeline`).
    /// Skipped on JSON serialisation by design — the wire form is
    /// runtime-only state with no persistent meaning.
    #[allow(dead_code)] // read via Drop; field exists solely to control lifetime
    pub ack: Option<Arc<AckHandle>>,
}

impl OwnedEvent {
    pub fn new(ingress: Bytes, source: SocketAddr) -> Self {
        Self {
            key: uuid::Uuid::now_v7(),
            received_at: Utc::now(),
            source,
            egress: ingress.clone(),
            ingress,
            workspace: HashMap::new(),
            ack: None,
        }
    }

    /// Variant of [`new`] that embeds a drop-fired ack token.
    ///
    /// Called by tail / journal inputs (the only inputs with a cursor to
    /// advance). The pipeline-worker layer never inspects `ack`: it fires
    /// automatically when the last surviving clone of this event drops at
    /// the end of `runtime::process_event`.
    pub fn with_ack(ingress: Bytes, source: SocketAddr, ack: Arc<AckHandle>) -> Self {
        Self {
            key: uuid::Uuid::now_v7(),
            received_at: Utc::now(),
            source,
            egress: ingress.clone(),
            ingress,
            workspace: HashMap::new(),
            ack: Some(ack),
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
            key: self.key,
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
    /// JSON string. Workspace values flow through the same boundary
    /// (present iff non-empty).
    pub fn to_json_value(&self) -> JsonValue {
        self.to_json_value_with(true)
    }

    /// As [`Self::to_json_value`], but always omits the `workspace`
    /// key regardless of population. Used by the output-flavor `tap`
    /// projection so the tap JSON shape stays queue-kind independent:
    /// disk-backed queues preserve the full workspace on their WAL
    /// snapshot (see `to_owned` at the pipeline's output statement),
    /// but the operator-facing tap output must not expose that
    /// disk/memory-queue difference — see docs/src/operations/tap.md
    /// for the contract.
    pub fn to_json_value_without_workspace(&self) -> JsonValue {
        self.to_json_value_with(false)
    }

    fn to_json_value_with(&self, include_workspace: bool) -> JsonValue {
        let mut map = serde_json::Map::new();
        map.insert(
            "key".into(),
            JsonValue::String(self.key.hyphenated().to_string()),
        );
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
        if include_workspace && !self.workspace.is_empty() {
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
        let key = match v.get("key") {
            Some(JsonValue::String(raw)) => {
                let parsed = uuid::Uuid::parse_str(raw).ok()?;
                if parsed.get_version_num() != 7
                    || parsed.get_variant() != uuid::Variant::RFC4122
                    || parsed.hyphenated().to_string() != *raw
                {
                    return None;
                }
                parsed
            }
            Some(_) => return None,
            None => uuid::Uuid::now_v7(),
        };
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
            key,
            received_at,
            source,
            ingress,
            egress,
            workspace: HashMap::new(),
            // Deserialised events are by definition past the input boundary
            // (DLQ replay, control-plane inject) — no upstream cursor to
            // advance, so no ack token.
            ack: None,
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
/// - `key` / `received_at` / `source` — typed metadata, scalar, copy-cheap.
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
    pub key: uuid::Uuid,
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
            key: self.key,
            received_at: self.received_at,
            source: self.source,
            ingress: self.ingress.clone(),
            egress: self.egress.clone(),
            workspace,
            // BorrowedEvent does not carry an ack — it's the per-event arena
            // view. Cloning back to owned for DLQ / disk-queue persistence
            // produces a fresh OwnedEvent whose lifetime is decoupled from
            // the original ack. The original OwnedEvent (held one frame up
            // in `process_event`) still owns the Arc and fires the ack on
            // its own scope exit, which is exactly the "pipeline-worker
            // completion" semantics we want.
            ack: None,
        }
    }

    /// Heap-allocate a fresh [`OwnedEvent`] from this borrowed view,
    /// dropping the `workspace` on the floor.
    ///
    /// Used at the pipeline `output` boundary for memory-backed queues,
    /// where no downstream consumer reads `workspace`: every sink's
    /// `consume` reads `egress` (with `file`'s dynamic path evaluator
    /// reading `source` / `received_at` and `kafka`'s optional key
    /// reading `source.ip`), the DLQ record projection stores only
    /// `OutputEvent`'s five fields, and the analyzer rejects
    /// `workspace` on the output config side at load time. Skipping
    /// the workspace deep-clone here avoids the per-event `HashMap<
    /// String, OwnedValue>` allocation + string-key allocations +
    /// `Value` tree materialisation that dominated the hot path once
    /// [`ProcessChain`] populated a non-trivial workspace — the
    /// dominant contributor to the D-shape throughput regression that
    /// landed with `b7625bb`.
    ///
    /// Not used on disk-backed queues (their WAL persists the full
    /// `Event` JSON including `workspace`) or in `--test-pipeline`
    /// mode (whose CLI display shows the populated workspace). The
    /// caller (`PipelineStatement::Output` in
    /// [`exec_pipeline_stmt`]) decides via its capture policy which
    /// of `to_owned` / `to_owned_without_workspace` to invoke.
    pub fn to_owned_without_workspace(&self) -> OwnedEvent {
        OwnedEvent {
            key: self.key,
            received_at: self.received_at,
            source: self.source,
            ingress: self.ingress.clone(),
            egress: self.egress.clone(),
            // `HashMap::new()` here does not allocate a backing table
            // — that only happens on the first `insert`. So this is a
            // zero-heap-touch construction; the four Bytes/scalar
            // copies dominate.
            workspace: HashMap::new(),
            ack: None,
        }
    }

    /// Take an arena-local shallow snapshot of this borrowed view.
    ///
    /// Every field is a shallow copy: `source` and `received_at` are
    /// `Copy` scalars; `ingress` and `egress` bump their `Bytes`
    /// refcounts; the workspace vec is copied into a fresh
    /// `bumpalo::Vec` of `(&'bump str, Value<'bump>)` pairs. The key
    /// slices and the arena-side `Value` referents are shared with
    /// the original view — they live in `arena` and are
    /// `Copy`-flavored (see `dsl::value::Value`). The workspace
    /// `Value`s themselves do not carry interior mutability, so the
    /// two views observe the same immutable data even if the source
    /// view later `workspace_set`s new entries or replaces existing
    /// slots (both operations mutate this view's `bumpalo::Vec` of
    /// index entries, not the arena-side value tree they point to).
    ///
    /// Cost model: one bump alloc (the new workspace `Vec`'s slice) +
    /// a memcpy of the `(ptr, len, value)` triples + two `Bytes`
    /// refcount bumps. No `HashMap` allocation, no `String` key
    /// allocations, no `Value` tree materialization — the deep-clone
    /// path that `to_owned` walks is not entered.
    ///
    /// Called by the `ProcessChain` executor to hold a stable
    /// pre-process view for the Err arm's DLQ context without paying
    /// `to_owned`'s heap materialization on every success. The Err
    /// arm still calls `.to_owned()` on the snapshot at DLQ-record
    /// build time to cross the arena boundary once — only the failure
    /// path pays that cost, not every successful process call.
    pub fn snapshot_in(&self, arena: &EventArena<'bump>) -> BorrowedEvent<'bump> {
        let mut workspace =
            bumpalo::collections::Vec::with_capacity_in(self.workspace.len(), arena.bump());
        for entry in self.workspace.iter() {
            workspace.push(*entry);
        }
        BorrowedEvent {
            key: self.key,
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
        if let Some(slot) = self.workspace.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.workspace.push((key, value));
        }
    }

    /// Insert or replace the workspace entry for `key`, copying the
    /// key into the arena first.
    pub fn workspace_set_str(&mut self, arena: &EventArena<'bump>, key: &str, value: Value<'bump>) {
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

#[cfg(test)]
mod boundary_tests {
    use super::*;

    fn sample_event() -> OwnedEvent {
        let mut ev = OwnedEvent::new(
            Bytes::from_static(b"<13>hello world"),
            "192.0.2.10:5140".parse::<SocketAddr>().unwrap(),
        );
        ev.egress = Bytes::from_static(b"rendered");
        ev.workspace
            .insert("k_str".into(), OwnedValue::String("v".into()));
        ev.workspace.insert("k_int".into(), OwnedValue::Int(42));
        ev.workspace.insert(
            "k_bytes".into(),
            OwnedValue::Bytes(Bytes::from_static(&[0xff, 0x00, 0xab])),
        );
        ev
    }

    #[test]
    fn view_in_then_to_owned_round_trips_event() {
        // The pipeline runs against `BorrowedEvent<'bump>`; failure
        // paths (errored / disk-queue / dlq / inject) need to recover
        // the equivalent `OwnedEvent`. The round-trip through
        // `view_in(arena) → to_owned()` must preserve every field
        // including workspace order-insensitive equality of keys.
        let original = sample_event();
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let borrowed = original.view_in(&arena);
        let recovered: OwnedEvent = borrowed.to_owned();
        assert_eq!(recovered.key, original.key);
        assert_eq!(recovered.received_at, original.received_at);
        assert_eq!(recovered.source, original.source);
        assert_eq!(recovered.ingress, original.ingress);
        assert_eq!(recovered.egress, original.egress);
        assert_eq!(recovered.workspace.len(), original.workspace.len());
        for (k, v) in &original.workspace {
            assert_eq!(recovered.workspace.get(k), Some(v), "key {k} lost");
        }
    }

    #[test]
    fn to_json_value_then_from_json_round_trips_event() {
        // This boundary is exercised every time a disk-queue replay /
        // tap snapshot / error-log entry / inject command needs to
        // serialise and re-hydrate an event. A regression in either
        // direction would silently drop fields. We hit each
        // representative value variant: String, Int, Bytes (which
        // routes through the `$bytes_b64` escape pathway).
        let original = sample_event();
        let json = original.to_json_value();
        assert_eq!(
            json["key"].as_str(),
            Some(original.key.hyphenated().to_string().as_str())
        );
        let serialized = serde_json::to_string(&json).unwrap();
        let recovered =
            OwnedEvent::from_json(&serialized).expect("from_json must accept its own to_json");
        assert_eq!(recovered.key, original.key);
        assert_eq!(recovered.received_at, original.received_at);
        assert_eq!(recovered.source, original.source);
        assert_eq!(recovered.ingress, original.ingress);
        assert_eq!(recovered.egress, original.egress);
        assert_eq!(recovered.workspace.len(), original.workspace.len());
        // Bytes round-trip: the value must come back as Bytes (not as
        // a base64 String), otherwise downstream consumers that
        // pattern-match on OwnedValue::Bytes would silently see
        // String instead.
        match recovered.workspace.get("k_bytes") {
            Some(OwnedValue::Bytes(b)) => assert_eq!(&b[..], &[0xff, 0x00, 0xab]),
            other => panic!("k_bytes round-tripped as wrong variant: {other:?}"),
        }
    }

    #[test]
    fn to_json_value_without_workspace_strips_populated_workspace() {
        // Contract pin for the 0.7.10 tap-output-JSON change: the
        // workspace-less projection must drop the `workspace` key
        // even when the source event carries populated workspace
        // entries. The output-flavor `tap` uses this projection to
        // make its JSON shape independent of the queue backend
        // (disk-backed queues preserve workspace on the snapshot; the
        // tap must strip it uniformly). If a future change puts the
        // key back, this test fires before docs / snippets drift.
        let ev = sample_event();
        assert!(
            !ev.workspace.is_empty(),
            "test fixture must populate workspace"
        );
        let stripped = ev.to_json_value_without_workspace();
        let obj = stripped.as_object().expect("must produce object");
        assert!(!obj.contains_key("workspace"), "workspace must be absent");
        // The remaining shape stays byte-identical to `to_json_value` sans workspace,
        // so downstream jq expressions that already project egress / source / etc.
        // keep working verbatim.
        assert!(obj.contains_key("received_at"));
        assert!(obj.contains_key("source"));
        assert!(obj.contains_key("ingress"));
        assert!(obj.contains_key("egress"));
    }

    #[test]
    fn to_owned_without_workspace_produces_empty_workspace() {
        // Pin the counterpart at the pipeline output boundary: the
        // memory-queue snapshot must not carry workspace even when the
        // borrowed event's workspace has entries. `to_owned` still
        // carries them for the disk-queue / `--test-pipeline` paths.
        let original = sample_event();
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let borrowed = original.view_in(&arena);
        let light = borrowed.to_owned_without_workspace();
        assert!(light.workspace.is_empty(), "workspace must be empty");
        assert_eq!(light.key, original.key);
        // Every other DLQ / sink-relevant field must survive verbatim,
        // matching the same round-trip guarantees as `to_owned`.
        assert_eq!(light.received_at, original.received_at);
        assert_eq!(light.source, original.source);
        assert_eq!(light.ingress, original.ingress);
        assert_eq!(light.egress, original.egress);
    }

    // ---- snapshot_in invariant pins ----
    //
    // `BorrowedEvent::snapshot_in` produces an arena-local shallow
    // view used by `ProcessChain` to hold a stable pre-call event for
    // the Err arm's DLQ context. The design relies on three invariants
    // that a future refactor could silently break — pin them here so
    // any accidental introduction of interior mutability or index
    // aliasing surfaces at test time rather than as a hard-to-catch
    // production drift.

    #[test]
    fn snapshot_in_is_stable_when_source_view_adds_a_new_workspace_key() {
        // Invariant (a): after taking the snapshot, mutating the
        // source view by adding a new workspace key must not appear
        // in the snapshot. This locks the shallow-copy semantics —
        // the snapshot's workspace vec is its own storage, not an
        // alias into the source's.
        let mut original = OwnedEvent::new(
            Bytes::from_static(b"ingress"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        original
            .workspace
            .insert("pre".into(), OwnedValue::String("initial".into()));
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let mut view = original.view_in(&arena);
        let snapshot = view.snapshot_in(&arena);
        assert_eq!(snapshot.key, original.key);

        // Mutate the source view after snapshotting.
        view.workspace_set_str(&arena, "added_after_snapshot", Value::Int(42));

        // The snapshot must not observe the new key.
        assert!(snapshot.workspace_get("added_after_snapshot").is_none());
        // The pre-existing key must still be visible on the snapshot,
        // proving the shallow copy captured it.
        assert!(matches!(
            snapshot.workspace_get("pre"),
            Some(Value::String("initial"))
        ));
    }

    #[test]
    fn snapshot_in_retains_old_value_when_source_view_replaces_slot() {
        // Invariant (b): if the source view replaces an existing
        // workspace slot's value after the snapshot, the snapshot's
        // matching entry must retain the pre-call value. This locks
        // that `workspace_set` mutates the source view's index vec,
        // not the arena-side referent — the snapshot's own index vec
        // continues to point at the pre-mutation `Value`.
        let mut original = OwnedEvent::new(
            Bytes::from_static(b"ingress"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        original
            .workspace
            .insert("route".into(), OwnedValue::String("original".into()));
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let mut view = original.view_in(&arena);
        let snapshot = view.snapshot_in(&arena);

        // Replace the slot on the source view.
        let replaced = Value::String(arena.alloc_str("replaced"));
        view.workspace_set_str(&arena, "route", replaced);

        // Snapshot still shows the original value.
        assert!(matches!(
            snapshot.workspace_get("route"),
            Some(Value::String("original"))
        ));
        // Source view now shows the replacement.
        assert!(matches!(
            view.workspace_get("route"),
            Some(Value::String("replaced"))
        ));
    }

    #[test]
    fn snapshot_in_retains_original_egress_when_source_view_rewrites_it() {
        // Invariant (c): `egress` is `Bytes`, i.e. a refcounted
        // handle. If the source view assigns a fresh `Bytes` to
        // `egress` after the snapshot, the snapshot's `egress` must
        // still point at the original bytes — refcounts, not shared
        // storage.
        let mut original = OwnedEvent::new(
            Bytes::from_static(b"ingress"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        original.egress = Bytes::from_static(b"pre-call egress");
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let mut view = original.view_in(&arena);
        let snapshot = view.snapshot_in(&arena);

        // Rewrite egress on the source view.
        view.egress = Bytes::from_static(b"post-call egress");

        assert_eq!(&snapshot.egress[..], b"pre-call egress");
        assert_eq!(&view.egress[..], b"post-call egress");
    }

    #[test]
    fn from_json_round_trips_received_at_nanos() {
        // received_at is serialised as i64 unix nanoseconds (OTLP
        // `time_unix_nano` parity) by `to_json_value` and decoded with
        // the same precision by `from_json`. Pin the full-nanosecond
        // round-trip so a future "drop sub-second" change is an
        // explicit decision rather than a silent regression.
        let mut original = OwnedEvent::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        original.received_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(1_700_000_000_123_456_789);
        let json = serde_json::to_string(&original.to_json_value()).unwrap();
        let recovered = OwnedEvent::from_json(&json).unwrap();
        assert_eq!(
            recovered.received_at.timestamp_nanos_opt(),
            original.received_at.timestamp_nanos_opt()
        );
    }

    #[test]
    fn constructors_mint_distinct_version_7_keys_before_fan_out() {
        let source = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let first = OwnedEvent::new(Bytes::from_static(b"first"), source);
        let second = OwnedEvent::new(Bytes::from_static(b"second"), source);

        let (ack_tx, _ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let ack = Arc::new(AckHandle::new(
            AckPosition::Offset {
                generation: 1,
                offset: 2,
            },
            ack_tx,
        ));
        let third = OwnedEvent::with_ack(Bytes::from_static(b"third"), source, Arc::clone(&ack));
        let fourth = OwnedEvent::with_ack(Bytes::from_static(b"fourth"), source, ack);

        let _: uuid::Uuid = first.key;
        let keys = [first.key, second.key, third.key, fourth.key];
        assert!(keys.iter().all(|key| key.get_version_num() == 7));
        assert!(
            keys.iter()
                .all(|key| key.get_variant() == uuid::Variant::RFC4122)
        );
        assert_eq!(
            keys.into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        assert_eq!(first.clone().key, first.key);
    }

    #[test]
    fn legacy_json_without_key_mints_one_on_first_read() {
        let original = sample_event();
        let mut json = original.to_json_value();
        json.as_object_mut().unwrap().remove("key");

        let encoded = serde_json::to_string(&json).unwrap();
        let first = OwnedEvent::from_json(&encoded).unwrap();
        let second = OwnedEvent::from_json(&encoded).unwrap();
        assert_eq!(first.key.get_version_num(), 7);
        assert_eq!(second.key.get_version_num(), 7);
        assert_ne!(first.key, original.key);
        assert_ne!(second.key, original.key);
        assert_ne!(first.key, second.key);
    }

    #[test]
    fn present_event_key_must_be_a_canonical_version_7_uuid() {
        let original = sample_event();
        let mut json = original.to_json_value();

        json["key"] = JsonValue::String("not-a-uuid".into());
        assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());

        json["key"] = JsonValue::String("550e8400-e29b-41d4-a716-446655440000".into());
        assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());

        json["key"] = JsonValue::String(original.key.simple().to_string());
        assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());

        json["key"] = JsonValue::String("0198A3B4-4D7E-7C20-8B11-9F4E6A2D1357".into());
        assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());

        json["key"] = JsonValue::String("0198a3b4-4d7e-7c20-0b11-9f4e6a2d1357".into());
        assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());

        for invalid in [
            JsonValue::Null,
            JsonValue::Number(7.into()),
            JsonValue::Bool(true),
            serde_json::json!({ "uuid": original.key.hyphenated().to_string() }),
        ] {
            json["key"] = invalid;
            assert!(OwnedEvent::from_json(&serde_json::to_string(&json).unwrap()).is_none());
        }
    }
}

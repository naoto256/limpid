//! systemd journal input: reads entries from the systemd journal.
//!
//! Wire format (LOTL — Living Off The Land):
//! `ingress` is one journald entry serialised as a single-line UTF-8
//! JSON object, shaped to be journalctl-`-o json`-compatible for the
//! fields libsystemd exposes. Field values match journalctl, but
//! `__SEQNUM` / `__SEQNUM_ID` (newer journalctl) are not surfaced —
//! this input's `sd_journal_*` FFI binding (`journal_sys`) doesn't
//! call `sd_journal_get_seqnum`. Key order is
//! insertion order from libsystemd and is not guaranteed to match
//! journalctl's output ordering.
//!
//! - field names preserved as journald exposes them (`PRIORITY`,
//!   `_PID`, `__REALTIME_TIMESTAMP`, `SYSLOG_IDENTIFIER`, `MESSAGE`, …)
//! - field order: insertion order from libsystemd
//! - UTF-8-clean values: JSON strings
//! - non-UTF-8 byte values: JSON array of integers
//!   (`[104, 101, 108, 108, 111]`) — journalctl convention
//! - numeric-looking fields like `PRIORITY` remain JSON strings
//!   (`"6"`); the DSL caller does the int conversion if needed
//!
//! Workspace stays empty — parsing is done in the process layer
//! (typically with the `parse_journald` snippet, which delegates to
//! `parse_json(ingress)`).
//!
//! Requires the `journal` feature and `libsystemd-dev` at compile time.
//! Only available on Linux systems with systemd.
//!
//! Properties:
//!   match     "SYSLOG_FACILITY=10"   — optional journal match filter
//!   state_file "/var/lib/limpid/journal/cursor"  — optional cursor persistence
//!   poll_interval "1s"               — optional (default: 1s)

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::journal_sys::Journal;
use anyhow::Result;
use bytes::Bytes;
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::{error, info, warn};

use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::{AckHandle, AckPosition, Event};
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const JOURNAL_SOURCE: &str = "127.0.0.1:0";

const JOURNAL_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "match",
        required: false,
        // Repeatable so operators can express both AND (across
        // different fields) and OR (repeat the same field). libsystemd's
        // `sd_journal_add_match` treats consecutive calls with the same
        // FIELD name as disjunctive (OR) and calls across different
        // FIELD names as conjunctive (AND); see `docs/src/inputs/journal.md`
        // for the operator-facing summary.
        repeatable: true,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "state_file",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "poll_interval",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
];

pub struct JournalInput {
    matches: Vec<String>,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    metrics: Arc<InputMetrics>,
}

impl Module for JournalInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(JOURNAL_INPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
        // Repeatable `match` — walk every occurrence so multi-filter
        // configs (`match "F=a" match "F=b"` for OR, `match "F=a"
        // match "G=b"` for AND) are all threaded through to
        // `sd_journal_add_match` at reader start.
        let matches: Vec<String> = properties
            .iter()
            .filter_map(|p| match p {
                crate::dsl::ast::Property::KeyValue { key, value, .. } if key == "match" => {
                    match &value.kind {
                        crate::dsl::ast::ExprKind::StringLit(s) => Some(s.clone()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();

        // Load-time format validation: each match must be
        // `FIELD=value`. A malformed entry here becomes a hard config
        // error at daemon startup / `--check` time so operators do
        // not discover the typo at runtime as an "reader terminated"
        // log line. `libsystemd` will still reject any well-formed
        // string it doesn't like (lowercase field name, empty field,
        // NUL bytes, etc.) at the runtime `match_add` boundary — that
        // path is handled below in `run_journal_reader`.
        for m in &matches {
            if !m.contains('=') {
                anyhow::bail!(
                    "input '{}': journal `match` requires `FIELD=value` shape (got \"{}\") — \
                     each match string must contain '='; see \
                     `docs/src/inputs/journal.md` for the filter combining rules",
                    name,
                    m
                );
            }
        }

        let state_file = props::get_string(properties, "state_file").map(PathBuf::from);
        let poll_interval = match props::get_string(properties, "poll_interval") {
            Some(s) => props::parse_duration(&s)?,
            None => DEFAULT_POLL_INTERVAL,
        };

        Ok(Self {
            matches,
            state_file,
            poll_interval,
            metrics: InputMetrics::register(&ctx.metrics, name)?,
        })
    }
}

impl HasMetrics for JournalInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for JournalInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        info!("journal input started");

        let source_addr = JOURNAL_SOURCE.parse().unwrap();
        let matches = self.matches.clone();
        let state_file = self.state_file.clone();
        let poll_interval = self.poll_interval;
        let metrics = Arc::clone(&self.metrics);

        // Journal API is synchronous — run in a blocking thread.
        // Channel payload: (entry-as-JSON-bytes, cursor)
        let (entry_tx, mut entry_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, String)>(1024);

        // The reader is parked inside `spawn_blocking`, so `abort()`
        // on the handle below is effectively a no-op once execution
        // begins (tokio only cancels not-yet-started blocking tasks).
        // Signal shutdown explicitly via an atomic flag the reader
        // polls between journal reads, so an idle reader exits within
        // bounded latency instead of leaking until the next entry —
        // which may never arrive on a quiet system.
        let reader_shutdown = Arc::new(AtomicBool::new(false));
        let reader_shutdown_for_thread = Arc::clone(&reader_shutdown);

        let journal_handle = tokio::task::spawn_blocking(move || {
            run_journal_reader(
                matches,
                state_file,
                poll_interval,
                entry_tx,
                reader_shutdown_for_thread,
            )
        });

        // Ack channel: pipeline workers drop the per-event AckHandle on
        // completion, which sends the carried journald cursor back here.
        // Cursors are opaque strings (not numeric), so the watermark is
        // simply "the most recent cursor we saw acked" — journald
        // guarantees forward progression within a single boot ID, and
        // saving an older cursor is harmless (re-read on restart). For
        // multi-boot timelines the cursor still uniquely identifies the
        // entry, so this remains correct under journalctl semantics.
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();

        loop {
            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("journal: shutting down");
                        // Tell the blocking reader to exit; `abort()`
                        // alone would not (the reader is already
                        // running on a blocking thread).
                        reader_shutdown.store(true, Ordering::Relaxed);
                        journal_handle.abort();
                        // Drain in-flight acks one last time so the
                        // most-recent processed cursor lands on disk.
                        if let Some(last) = drain_cursor_acks(&mut ack_rx)
                            && let Some(ref sf) = self.state_file
                        {
                            save_cursor(sf, &last);
                        }
                        break;
                    }
                }

                Some(pos) = ack_rx.recv() => {
                    if let AckPosition::Cursor(mut cur) = pos {
                        // Coalesce any other acks already queued so we
                        // don't fsync-spam on a busy pipeline. The last
                        // one wins (journald cursors are monotonic
                        // forward within a boot ID).
                        while let Ok(AckPosition::Cursor(next)) = ack_rx.try_recv() {
                            cur = next;
                        }
                        if let Some(ref sf) = self.state_file {
                            save_cursor(sf, &cur);
                        }
                    }
                }

                entry = entry_rx.recv() => {
                    match entry {
                        Some((bytes, cursor)) => {
                            metrics.events_received.inc();
                            let ack = Arc::new(AckHandle::new(
                                AckPosition::Cursor(cursor),
                                ack_tx.clone(),
                            ));
                            let event = Event::with_ack(
                                Bytes::from(bytes),
                                source_addr,
                                Arc::clone(&ack),
                            );
                            if let Err(send_err) = tx.send(event).await {
                                // Disarm so the un-processed event's
                                // ack does NOT advance the cursor.
                                ack.disarm();
                                drop(send_err);
                                break;
                            }
                            // Cursor persistence happens via the ack
                            // branch above, not here — pre-fix this
                            // block saved the cursor on send-success,
                            // which is the at-most-once gap we're
                            // closing.
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}

/// Drain the ack channel and return the most-recent cursor (or None if
/// empty). Used at shutdown to flush an in-flight watermark.
fn drain_cursor_acks(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AckPosition>) -> Option<String> {
    let mut last: Option<String> = None;
    while let Ok(pos) = rx.try_recv() {
        if let AckPosition::Cursor(c) = pos {
            last = Some(c);
        }
    }
    last
}

/// Encode one journal entry's fields into a `serde_json::Value`
/// shaped to be journalctl-`-o json`-compatible for the fields
/// libsystemd exposes (= see caveats below).
///
/// - field name (always UTF-8 by journald spec) → JSON object key
/// - UTF-8-clean field value → JSON string
/// - non-UTF-8 field value → JSON array of integers (byte values)
/// - field with no value (rare) → JSON null
///
/// In addition to enumerated data fields, this also surfaces
/// journald's trusted address metadata as `__`-prefixed keys
/// (matching the journalctl convention):
///   - `__CURSOR`              — opaque resume token
///   - `__REALTIME_TIMESTAMP`  — wall-clock microseconds since epoch (string)
///   - `__MONOTONIC_TIMESTAMP` — boot-relative microseconds (string)
///
/// `__SEQNUM` / `__SEQNUM_ID` (newer journalctl) are not surfaced —
/// `journal_sys` doesn't bind `sd_journal_get_seqnum`. Add if a caller
/// needs it.
///
/// Key order is not guaranteed to match journalctl; this is a JSON
/// object so order is a serialisation detail, not a semantic one.
/// Field set and values must match — that's the LOTL contract.
fn collect_entry_fields(journal: &mut Journal) -> Result<JsonMap<String, JsonValue>> {
    let mut map = JsonMap::new();
    journal.restart_data();
    while let Some(field) = journal.enumerate_data()? {
        let name = match std::str::from_utf8(field.name()) {
            Ok(s) => s.to_string(),
            Err(_) => {
                // Field names are UTF-8 by journald spec; if we hit a
                // non-UTF-8 name treat it as a corrupt entry and skip
                // the field rather than poison the JSON object.
                warn!("journal: skipping field with non-UTF-8 name");
                continue;
            }
        };
        let value = match field.value() {
            None => JsonValue::Null,
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => JsonValue::String(s.to_string()),
                Err(_) => {
                    // journalctl convention: non-UTF-8 values become
                    // an array of byte integers
                    JsonValue::Array(
                        bytes
                            .iter()
                            .map(|b| JsonValue::Number((*b as u64).into()))
                            .collect(),
                    )
                }
            },
        };
        map.insert(name, value);
    }

    // Trusted address metadata (set by libsystemd, not the
    // application; not visible via enumerate_data). All values are
    // formatted as JSON strings to match journalctl's output, where
    // numeric-looking journald fields are always strings.
    if let Ok(cursor) = journal.cursor() {
        map.insert("__CURSOR".to_string(), JsonValue::String(cursor));
    }
    if let Ok(usec) = journal.timestamp_usec() {
        map.insert(
            "__REALTIME_TIMESTAMP".to_string(),
            JsonValue::String(usec.to_string()),
        );
    }
    if let Ok((mono_usec, _boot_id)) = journal.monotonic_timestamp() {
        map.insert(
            "__MONOTONIC_TIMESTAMP".to_string(),
            JsonValue::String(mono_usec.to_string()),
        );
    }

    Ok(map)
}

/// Sleep up to `total` but wake early when `shutdown` is set.
///
/// The journal reader's idle path used to be a plain
/// `std::thread::sleep(poll_interval)`, which on a quiet system
/// could nap for the full poll interval — and crucially could not
/// see the orchestrator's shutdown signal during that nap, because
/// `spawn_blocking` tasks can't be aborted once running. Sleeping
/// in small quanta and re-checking the flag bounds shutdown latency
/// to roughly one quantum regardless of `poll_interval`.
fn interruptible_sleep(shutdown: &AtomicBool, total: Duration) {
    const QUANTUM: Duration = Duration::from_millis(100);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let nap = remaining.min(QUANTUM);
        std::thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
}

/// Synchronous journal reader running in a blocking thread.
fn run_journal_reader(
    matches: Vec<String>,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    tx: tokio::sync::mpsc::Sender<(Vec<u8>, String)>,
    shutdown: Arc<AtomicBool>,
) {
    let mut journal = match Journal::open() {
        Ok(j) => j,
        Err(e) => {
            error!("journal: failed to open: {}", e);
            return;
        }
    };

    // Apply match filters. Format was validated at load time
    // (`from_properties` rejects any string without '='), so
    // `split_once('=')` here is defensively resolved as `else { … }`
    // rather than `unwrap()`. libsystemd's `sd_journal_add_match`
    // rejects field names it doesn't recognise (lowercase, empty,
    // NUL-containing, etc.) — we collect every rejection so the
    // operator sees the full list in one pass and terminate the
    // reader if any filter failed. Semantics: a filter that
    // libsystemd cannot install matches nothing, so the reader
    // exiting with zero events *is* the configured behaviour — the
    // `error!` line is only there so the operator knows which
    // filter string was refused. Other inputs are unaffected.
    let mut match_add_failed = false;
    for m in &matches {
        let Some((key, val)) = m.split_once('=') else {
            error!(
                "journal input: match '{}' has no '=' separator — should have been rejected at \
                 load time; reader terminating",
                m
            );
            return;
        };
        if let Err(e) = journal.match_add(key, val) {
            error!(
                "journal input: match_add rejected filter '{}': {} — this filter cannot be \
                 satisfied by any journal entry",
                m, e
            );
            match_add_failed = true;
        }
    }
    if match_add_failed {
        error!(
            "journal input: one or more match filters were rejected by libsystemd; reader \
             terminating (semantics: no journal entry can satisfy the specified configuration, \
             so zero events is the correct output — fix the offending filter(s) and restart)"
        );
        return;
    }

    // Seek to saved cursor or end.
    //
    // First-start (no `state_file` or an empty one) is the case
    // hardware validation caught previously: with a `match` filter
    // whose view of the journal is empty at daemon start
    // (e.g. a brand-new `SYSLOG_IDENTIFIER` that has never been
    // logged before), `sd_journal_seek_tail` + `sd_journal_previous`
    // leaves the read pointer in an implementation-defined state and
    // the poll loop below never advances to new arrivals. Branch on
    // `previous()`'s return value: on `> 0` we anchored at the last
    // matching entry (the historical working path); on `0` (empty
    // match view) fall back to `seek_head` — with no past matching
    // entries there is no history to replay, and `next()` in the
    // poll loop cleanly advances to the first newly-arrived entry.
    if let Some(cursor) = state_file.as_ref().and_then(load_cursor) {
        if let Err(e) = journal.seek_cursor(&cursor) {
            warn!(
                "journal: failed to seek to cursor, starting from end: {}",
                e
            );
            anchor_at_tail_or_head(&mut journal);
        } else {
            // Skip the entry at the cursor (already processed).
            let _ = journal.next();
        }
    } else {
        anchor_at_tail_or_head(&mut journal);
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match journal.next() {
            Ok(n) if n > 0 => {
                let map = match collect_entry_fields(&mut journal) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("journal: failed to enumerate entry fields: {}", e);
                        continue;
                    }
                };

                // Serialise to bytes WITHOUT trailing newline. The
                // Event boundary itself is the record separator;
                // journalctl-o-json puts a `\n` after each line for
                // stream framing, but `Event.ingress` already carries
                // exactly one entry so there is nothing to separate.
                let bytes = match serde_json::to_vec(&JsonValue::Object(map)) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("journal: failed to serialise entry to JSON: {}", e);
                        continue;
                    }
                };

                let cursor = journal.cursor().unwrap_or_default();

                if tx.blocking_send((bytes, cursor)).is_err() {
                    break; // receiver dropped
                }
            }
            Ok(_) => {
                // No more entries, wait
                interruptible_sleep(&shutdown, poll_interval);
            }
            Err(e) => {
                warn!("journal: read error: {}", e);
                interruptible_sleep(&shutdown, poll_interval);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn interruptible_sleep_returns_promptly_when_flag_set_before_call() {
        // Defensive baseline: if the flag is already set before the
        // sleep starts, the loop never naps. Latency must be near
        // zero.
        let shutdown = AtomicBool::new(true);
        let started = Instant::now();
        interruptible_sleep(&shutdown, Duration::from_secs(60));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "pre-set flag must short-circuit immediately; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn interruptible_sleep_wakes_within_one_quantum_when_flag_flips() {
        // Regression: the previous reader used plain
        // `std::thread::sleep(poll_interval)`, so on an idle host the
        // shutdown signal couldn't preempt the nap. With chunked
        // sleeps re-checking the flag, the wake-up latency is bounded
        // by one QUANTUM (~100ms) regardless of how long the caller
        // asked us to sleep.
        let shutdown = Arc::new(AtomicBool::new(false));
        let flip = Arc::clone(&shutdown);

        // Flip the flag from another thread mid-sleep.
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flip.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        // Ask for a 30 s sleep that we'd never tolerate in shutdown.
        interruptible_sleep(&shutdown, Duration::from_secs(30));
        let elapsed = started.elapsed();
        flipper.join().unwrap();

        // Wake-up must happen well under one second: at most one
        // QUANTUM (100 ms) after the flip at 150 ms = ~250 ms upper
        // bound. Allow generous slack for CI scheduling jitter.
        assert!(
            elapsed < Duration::from_secs(1),
            "flag flip must interrupt the sleep; took {:?}",
            elapsed
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "should not return before the first quantum elapses; took {:?}",
            elapsed
        );
    }

    // -----------------------------------------------------------------
    // Cursor-ack drain helpers — coverage for the at-most-once gap fix
    // -----------------------------------------------------------------
    //
    // We can't exercise the full `run()` without a live libsystemd
    // journal, but the cursor-watermark logic is isolated in
    // `drain_cursor_acks` and the AckHandle drop semantics — both of
    // which are independent of the journal feature.

    #[tokio::test]
    async fn drain_cursor_acks_returns_most_recent_cursor() {
        // The journal cursor channel is drained at shutdown so the
        // last-seen cursor lands on disk before exit. Coalescing rule:
        // the most recent cursor wins (journald cursors are monotonic
        // forward within a boot ID).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();
        tx.send(AckPosition::Cursor("c1".into())).unwrap();
        tx.send(AckPosition::Cursor("c2".into())).unwrap();
        tx.send(AckPosition::Cursor("c3".into())).unwrap();
        assert_eq!(drain_cursor_acks(&mut rx), Some("c3".into()));
    }

    #[tokio::test]
    async fn drain_cursor_acks_returns_none_on_empty_channel() {
        // Shutdown drain must not block, and must not synthesise a
        // value when there is nothing to flush — the caller treats
        // None as "no cursor to save", preserving the prior watermark.
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();
        assert_eq!(drain_cursor_acks(&mut rx), None);
    }

    #[tokio::test]
    async fn ack_handle_with_cursor_fires_on_drop() {
        // The journal ack path piggy-backs on the same AckHandle drop
        // mechanism as tail; the only difference is the AckPosition
        // variant. Pin that the Cursor variant survives the round-trip
        // through Drop intact.
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();
        let handle = Arc::new(AckHandle::new(
            AckPosition::Cursor("s=abc;i=1".into()),
            ack_tx,
        ));
        drop(handle);
        match ack_rx.recv().await {
            Some(AckPosition::Cursor(c)) => assert_eq!(c, "s=abc;i=1"),
            other => panic!("expected Cursor variant, got {:?}", other),
        }
    }

    /// Build a `ModuleProperties` for `type journal` from a raw DSL
    /// input body — the parser's normal path with the `type` prefix
    /// stripped by `ModuleProperties`, so `from_properties` sees the
    /// same shape it does at daemon startup.
    fn journal_properties_from_body(body: &str) -> crate::dsl::module_props::ModuleProperties {
        let src = format!(
            "def input j {{ type journal {} }} def output o {{ type file path \"/tmp/o.log\" }} \
             def pipeline p {{ input j; output o }}",
            body
        );
        let config = crate::dsl::parser::parse_config(&src).expect("parse should succeed");
        for def in &config.definitions {
            if let crate::dsl::ast::Definition::Input(input) = def
                && input.name == "j"
            {
                return input.properties.clone();
            }
        }
        panic!("input 'j' must exist");
    }

    #[test]
    fn from_properties_accepts_multiple_match_filters() {
        // Repeatable `match` — libsystemd combines same-field entries
        // as OR and different-field entries as AND. The load-time
        // parser must accept every occurrence so both semantics are
        // available to operators.
        let props = journal_properties_from_body(
            r#"match "SYSLOG_IDENTIFIER=app1" match "SYSLOG_IDENTIFIER=app2" match "_UID=1000""#,
        );
        let input = JournalInput::from_properties(
            "j",
            &props,
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("valid config should parse");
        assert_eq!(input.matches.len(), 3);
        assert_eq!(input.matches[0], "SYSLOG_IDENTIFIER=app1");
        assert_eq!(input.matches[1], "SYSLOG_IDENTIFIER=app2");
        assert_eq!(input.matches[2], "_UID=1000");
    }

    #[test]
    fn from_properties_rejects_match_without_equals_separator() {
        // Every `match` string must contain `=` (FIELD=value shape).
        // A typo like `match "SYSLOG_IDENTIFIER app"` (space instead
        // of `=`) must fail at load time so operators do not
        // discover the mistake at runtime as a terminated reader.
        let props = journal_properties_from_body(r#"match "no_equals_separator""#);
        let err = match JournalInput::from_properties(
            "j",
            &props,
            &crate::modules::BuildContext::for_testing(),
        ) {
            Ok(_) => panic!("malformed match must reject at load time"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no_equals_separator") && msg.contains("FIELD=value"),
            "reject message must name the offending value and remediation; got: {}",
            msg
        );
    }

    #[test]
    fn from_properties_rejects_mixed_valid_and_invalid_match_filters() {
        // Reject at the first malformed entry even if earlier ones
        // parse cleanly — an operator with a typo in the second
        // filter should not have to notice the loud diagnostic buried
        // between valid ones.
        let props =
            journal_properties_from_body(r#"match "SYSLOG_IDENTIFIER=app" match "typo_here""#);
        let err = match JournalInput::from_properties(
            "j",
            &props,
            &crate::modules::BuildContext::for_testing(),
        ) {
            Ok(_) => panic!("malformed match must reject at load time"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("typo_here"));
    }

    #[test]
    fn schema_rejects_unknown_property_on_journal_input() {
        // Regression pin for the `cursor_file` typo (the correct
        // property name is `state_file`). Schema validation must
        // reject any property not listed in `JOURNAL_INPUT_SCHEMA`;
        // the analyzer produces a `Level::Error` diagnostic
        // that both `--check` and daemon startup surface.
        use crate::check::analyze;
        use crate::dsl::span::SourceMap;
        use crate::pipeline::CompiledConfig;

        let src = r#"
def input j { type journal cursor_file "/tmp/j.cursor" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input j; output o }
"#;
        let config = crate::dsl::parser::parse_config(src).expect("parse should succeed");
        let compiled = CompiledConfig::from_config(config).expect("compile should succeed");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        let diags = analyze(&compiled, &sm);
        let unknown_cursor_file = diags.iter().any(|d| {
            d.level == crate::check::Level::Error
                && d.message.contains("cursor_file")
                && d.message.contains("unknown property")
        });
        assert!(
            unknown_cursor_file,
            "--check must reject `cursor_file` (the correct property is `state_file`) with a \
             Level::Error diagnostic; got: {:?}",
            diags
        );
    }

    #[test]
    fn save_cursor_then_load_round_trips() {
        // The cursor watermark must survive a write+read round-trip.
        // Pre-fix this was exercised on every event; post-fix it runs
        // only on ack arrival or shutdown, so pinning the
        // serialisation contract independently is even more important
        // (regressions would now be visible only after the next
        // restart).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor");
        save_cursor(&path, "s=zzz;i=42");
        assert_eq!(load_cursor(&path), Some("s=zzz;i=42".into()));
    }

    #[test]
    fn interruptible_sleep_completes_full_duration_without_signal() {
        // Sanity: when the flag never flips, the helper actually
        // sleeps roughly the requested time.
        let shutdown = AtomicBool::new(false);
        let started = Instant::now();
        interruptible_sleep(&shutdown, Duration::from_millis(250));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(240),
            "should sleep close to requested duration; took {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "should not over-sleep wildly; took {:?}",
            elapsed
        );
    }
}

/// Anchor the journal read pointer at "just after the last matching
/// entry" — so the poll loop's `next()` advances to the FIRST entry
/// that arrives after daemon start, and no history is replayed.
///
/// The obvious `seek_tail()` + `previous()` sequence works when the
/// active `match` view has at least one past entry (`previous`
/// returns `n > 0`), but silently fails when the view is empty
/// (`previous` returns `0` and the read pointer is
/// implementation-defined). Branch on `previous()`'s return so the
/// empty-match-view case (`Ok(0)`) falls through to `seek_head()` —
/// with no history to replay by definition, `seek_head` + poll-loop
/// `next()` picks up the first future match cleanly.
///
/// A `previous()` **error** is deliberately NOT treated the same way:
/// `Err` does not imply an empty view — a non-empty view can also
/// error here — and `seek_head` on a non-empty view would replay
/// every past matching entry. On `Err` we keep the tail-anchored
/// position established by the preceding `seek_tail()` and let the
/// poll loop's `next()` advance from there; the operator sees the
/// warn line.
///
/// Errors are logged (`warn!`) but not fatal: `seek_tail` / `previous`
/// / `seek_head` failing here means the reader will still call
/// `next()` in the poll loop against whatever cursor state the
/// journal actually holds, and the operator will see the log line.
/// Terminating the reader on a seek error would be stricter than
/// necessary — the caller already made match_add work, and a poll
/// loop against an inherited cursor is safer than crashing.
fn anchor_at_tail_or_head(journal: &mut Journal) {
    if let Err(e) = journal.seek_tail() {
        warn!(
            "journal: seek_tail failed, poll loop will start from whatever position the journal \
             defaulted to: {}",
            e
        );
        return;
    }
    match journal.previous() {
        Ok(n) if n > 0 => {
            // Anchored at the last matching entry; the poll loop's
            // `next()` will advance past it into new arrivals.
        }
        Ok(_) => {
            // Empty match view — no past matching entries. `seek_tail`
            // may have left the read pointer in an implementation-
            // defined state, so re-anchor at head. Since the match
            // view is empty by construction there is no history to
            // replay; the very next `next()` in the poll loop will
            // wait until a matching entry actually appears.
            if let Err(e) = journal.seek_head() {
                warn!(
                    "journal: seek_head fallback failed after empty-match previous(); poll loop \
                     may not detect new matching entries reliably: {}",
                    e
                );
            }
        }
        Err(e) => {
            // Journal-level error walking backward. Do NOT fall back
            // to `seek_head`: the view may be non-empty, and re-
            // seeking to head would replay historical matching
            // entries. Keep the tail-anchored position from
            // `seek_tail()` and let the poll loop's `next()` advance
            // from there.
            warn!(
                "journal: previous() after seek_tail failed ({}) — keeping the tail-anchored \
                 position from seek_tail; poll loop will advance from there",
                e
            );
        }
    }
}

fn load_cursor(path: &PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_cursor(path: &PathBuf, cursor: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp_path, cursor).and_then(|_| std::fs::rename(&tmp_path, path))
    {
        warn!(
            "journal: failed to save cursor: {} — events may be re-delivered on restart",
            e
        );
        let _ = std::fs::remove_file(&tmp_path);
    }
}

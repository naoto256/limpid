//! systemd journal input: reads entries from the systemd journal.
//!
//! Wire format (LOTL — Living Off The Land):
//! `ingress` is one journald entry serialised as a single-line UTF-8
//! JSON object, equivalent to one line of `journalctl -o json`. The
//! field set and values match journalctl byte-for-byte; key order
//! within the JSON object is not guaranteed to match (JSON object
//! ordering is a serialisation detail).
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
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use serde_json::{Map as JsonMap, Value as JsonValue};
use systemd::journal::{Journal, OpenOptions};
use tracing::{error, info, warn};

use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const JOURNAL_SOURCE: &str = "127.0.0.1:0";

const JOURNAL_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "match",
        required: false,
        repeatable: false,
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

    fn from_properties(_name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let mut matches = Vec::new();
        if let Some(m) = props::get_string(properties, "match") {
            matches.push(m);
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
            metrics: Arc::new(InputMetrics::default()),
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

        let journal_handle = tokio::task::spawn_blocking(move || {
            run_journal_reader(matches, state_file, poll_interval, entry_tx)
        });

        loop {
            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("journal: shutting down");
                        journal_handle.abort();
                        break;
                    }
                }

                entry = entry_rx.recv() => {
                    match entry {
                        Some((bytes, cursor)) => {
                            metrics.events_received.fetch_add(1, Ordering::Relaxed);
                            let event = Event::new(Bytes::from(bytes), source_addr);
                            if tx.send(event).await.is_err() {
                                break;
                            }
                            if let Some(ref sf) = self.state_file {
                                save_cursor(sf, &cursor);
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}

/// Encode one journal entry's fields into a `serde_json::Value`
/// equivalent to `journalctl -o json` output.
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
/// the systemd-0.10.x crate exposes no equivalent API. Add when
/// upstream support lands.
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

/// Synchronous journal reader running in a blocking thread.
fn run_journal_reader(
    matches: Vec<String>,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    tx: tokio::sync::mpsc::Sender<(Vec<u8>, String)>,
) {
    let mut journal = match OpenOptions::default().open() {
        Ok(j) => j,
        Err(e) => {
            error!("journal: failed to open: {}", e);
            return;
        }
    };

    // Apply match filters (format: "FIELD=value")
    for m in &matches {
        if let Some((key, val)) = m.split_once('=') {
            if let Err(e) = journal.match_add(key, val) {
                warn!("journal: failed to add match '{}': {}", m, e);
            }
        } else {
            warn!(
                "journal: invalid match format '{}', expected 'FIELD=value'",
                m
            );
        }
    }

    // Seek to saved cursor or end
    if let Some(cursor) = state_file.as_ref().and_then(|f| load_cursor(f)) {
        if let Err(e) = journal.seek_cursor(&cursor) {
            warn!(
                "journal: failed to seek to cursor, starting from end: {}",
                e
            );
            let _ = journal.seek_tail();
            let _ = journal.previous();
        } else {
            // Skip the entry at the cursor (already processed)
            let _ = journal.next();
        }
    } else {
        let _ = journal.seek_tail();
        let _ = journal.previous();
    }

    loop {
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
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                warn!("journal: read error: {}", e);
                std::thread::sleep(poll_interval);
            }
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

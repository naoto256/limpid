//! systemd journal input: reads entries from the systemd journal.
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
use systemd::journal::{Journal, OpenOptions};
use tracing::{error, info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module, ModuleSchema};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const JOURNAL_SOURCE: &str = "127.0.0.1:0";

pub struct JournalInput {
    matches: Vec<String>,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    metrics: Arc<InputMetrics>,
}

impl Module for JournalInput {
    fn schema() -> ModuleSchema {
        ModuleSchema::default()
    }

    fn from_properties(_name: &str, properties: &[Property]) -> Result<Self> {
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

        // Journal API is synchronous — run in a blocking thread
        let (entry_tx, mut entry_rx) = tokio::sync::mpsc::channel::<(String, String)>(1024);

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
                        Some((message, cursor)) => {
                            metrics.events_received.fetch_add(1, Ordering::Relaxed);
                            let event = Event::new(Bytes::from(message), source_addr);
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

/// Read a field value from the current journal entry as a String.
fn get_field(journal: &mut Journal, field: &str) -> Option<String> {
    journal.get_data(field).ok().and_then(|entry| {
        entry?
            .value()
            .map(|v| String::from_utf8_lossy(v).into_owned())
    })
}

/// Synchronous journal reader running in a blocking thread.
fn run_journal_reader(
    matches: Vec<String>,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    tx: tokio::sync::mpsc::Sender<(String, String)>,
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
                // Build message from journal entry fields
                let message = match get_field(&mut journal, "MESSAGE") {
                    Some(msg) => msg,
                    None => continue,
                };

                let cursor = journal.cursor().unwrap_or_default();

                // Build a syslog-like message: "IDENTIFIER[PID]: MESSAGE"
                let identifier = get_field(&mut journal, "SYSLOG_IDENTIFIER")
                    .or_else(|| get_field(&mut journal, "_COMM"))
                    .unwrap_or_default();
                let pid = get_field(&mut journal, "SYSLOG_PID")
                    .or_else(|| get_field(&mut journal, "_PID"));

                let formatted = if let Some(pid) = pid {
                    format!("{}[{}]: {}", identifier, pid, message)
                } else if !identifier.is_empty() {
                    format!("{}: {}", identifier, message)
                } else {
                    message
                };

                if tx.blocking_send((formatted, cursor)).is_err() {
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

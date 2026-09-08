//! Native Windows Event Log input with journal-equivalent checkpoint policy.
use super::{
    windows_event_log_json, windows_event_log_state as state,
    windows_event_log_sys::{StartPosition, Subscription},
};
use crate::{
    dsl::{
        props,
        schema::{PropertySpec, PropertyValueKind},
    },
    event::{AckHandle, AckPosition, Event},
    metrics::InputMetrics,
    modules::{HasMetrics, Input, Module},
};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "channel",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "query",
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
pub struct WindowsEventLogInput {
    channel: String,
    query: String,
    state_file: Option<PathBuf>,
    poll: Duration,
    metrics: Arc<InputMetrics>,
}
impl Module for WindowsEventLogInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SCHEMA)
    }
    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
        let channel = props::get_string(properties, "channel")
            .context("windows_event_log requires channel")?;
        let query = props::get_string(properties, "query").unwrap_or_else(|| "*".into());
        if channel.is_empty() || channel.contains('\0') || query.contains('\0') {
            anyhow::bail!("Event Log channel/query must be nonempty channel and NUL-free strings");
        }
        let poll = props::get_string(properties, "poll_interval")
            .map(|s| props::parse_duration(&s))
            .transpose()?
            .unwrap_or(Duration::from_secs(1));
        Ok(Self {
            channel,
            query,
            poll,
            state_file: props::get_string(properties, "state_file").map(PathBuf::from),
            metrics: InputMetrics::register(&ctx.metrics, name)?,
        })
    }
}
impl HasMetrics for WindowsEventLogInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn save_ack(path: Option<&std::path::Path>, position: AckPosition) {
    if let (Some(path), AckPosition::Cursor(bookmark)) = (path, position)
        && let Err(error) = state::save(path, &bookmark)
    {
        tracing::warn!(
            "Event Log bookmark save failed: {error}; events may be re-read after restart"
        );
    }
}

#[async_trait::async_trait]
impl Input for WindowsEventLogInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(Arc::clone(&stop));
        let thread_stop = Arc::clone(&stop);
        let (entries, mut receiver) = tokio::sync::mpsc::channel(16);
        let (ready, readiness) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let state_file = self.state_file.clone();
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            let (mut subscription, warning) =
                match state::subscribe(&self.channel, &self.query, state_file.as_deref()) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return Err(error.into());
                    }
                };
            if let Some(error) = warning {
                tracing::warn!(
                    "Event Log bookmark resume failed: {error}; starting from new events"
                );
            }
            let _ = ready.send(Ok(()));
            let wait = self
                .poll
                .clamp(Duration::from_millis(1), Duration::from_millis(100));
            while !thread_stop.load(Ordering::Acquire) {
                match subscription.next(wait) {
                    Ok(Some(record)) => {
                        let bytes = windows_event_log_json::encode(&record.xml)
                            .map_err(anyhow::Error::msg)?;
                        if entries.blocking_send((bytes, record.bookmark)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    // A cleared/rotated channel can invalidate a live result set.
                    // Match journal's warning-and-tail fallback, without writing
                    // a new checkpoint until a later event is acknowledged.
                    Err(error) if error.raw_os_error() == Some(15011) => {
                        tracing::warn!(
                            "Event Log result set became stale: {error}; starting from new events"
                        );
                        subscription =
                            Subscription::open(&self.channel, &self.query, StartPosition::Future)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        });
        match readiness.await {
            Ok(Ok(())) => crate::modules::input_startup_ready(),
            result => {
                stop.store(true, Ordering::Release);
                receiver.close();
                let _ = worker.await;
                anyhow::bail!("Event Log startup failed: {result:?}");
            }
        }
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
                Some(position) = ack_rx.recv() => {
                    let mut last = position;
                    while let Ok(next) = ack_rx.try_recv() { last = next; }
                    save_ack(self.state_file.as_deref(), last);
                }
                entry = receiver.recv() => {
                    let Some((bytes, bookmark)) = entry else { break; };
                    self.metrics.events_received.inc(); self.metrics.bytes_received.inc_by(bytes.len() as u64);
                    let ack = Arc::new(AckHandle::new(AckPosition::Cursor(bookmark), ack_tx.clone()));
                    let event = Event::with_ack(bytes.into(), "127.0.0.1:0".parse().expect("literal address"), Arc::clone(&ack));
                    let sent = loop {
                        tokio::select! {
                            biased;
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow() { break false; }
                            }
                            permit = tx.reserve() => {
                                match permit {
                                    Ok(permit) => { permit.send(event); break true; }
                                    Err(_) => break false,
                                }
                            }
                        }
                    };
                    if !sent { ack.disarm(); break; }
                }
            }
        }
        stop.store(true, Ordering::Release);
        receiver.close();
        let result = worker.await.context("Event Log reader task panicked")?;
        let mut last = None;
        while let Ok(position) = ack_rx.try_recv() {
            last = Some(position);
        }
        if let Some(position) = last {
            save_ack(self.state_file.as_deref(), position);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stops_reader(initial: bool, close_watch: bool) {
        let registry = crate::metrics::Registry::new();
        let input = WindowsEventLogInput {
            channel: "System".into(),
            query: "*[System[EventID=999999]]".into(),
            state_file: None,
            poll: Duration::from_secs(30),
            metrics: InputMetrics::register(&registry, "stop-reader").unwrap(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(initial);
        if close_watch {
            drop(shutdown_tx);
        }
        tokio::time::timeout(Duration::from_secs(3), input.run(tx, shutdown_rx))
            .await
            .expect("native reader must stop and join within the budget")
            .expect("reader shutdown must be clean");
    }

    #[tokio::test]
    async fn closed_watch_stops_and_joins_native_reader() {
        stops_reader(false, true).await;
    }

    #[tokio::test]
    async fn already_requested_shutdown_stops_and_joins_native_reader() {
        stops_reader(true, false).await;
    }
}

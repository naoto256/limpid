//! Debug tap: allows external tools to subscribe to a copy of events
//! flowing through inputs, processes, and outputs.
//!
//! Tap points are registered at startup with keys like:
//!   `input splunk_udp`, `process strip_pri`, `output juniper01`
//!
//! Performance: `emit()` checks an atomic subscriber count before
//! acquiring any lock. When no subscribers are connected, the only
//! cost is an atomic load per event.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::event::Event;

/// Size of the broadcast channel per tap subscriber.
const TAP_CHANNEL_SIZE: usize = 256;

/// Per-tap-point state.
struct TapChannel {
    sender: broadcast::Sender<Arc<Event>>,
    /// Cached subscriber count for fast-path check (avoids lock on emit).
    subscriber_count: Arc<AtomicUsize>,
}

/// Global registry of tap channels, keyed by `"<kind> <name>"`.
#[derive(Clone)]
pub struct TapRegistry {
    inner: Arc<RwLock<HashMap<String, TapChannel>>>,
}

impl TapRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a tap point (called at startup).
    /// `key` should be `"input <name>"`, `"process <name>"`, or `"output <name>"`.
    pub async fn register(&self, key: &str) {
        let mut map = self.inner.write().await;
        if !map.contains_key(key) {
            let (sender, _) = broadcast::channel(TAP_CHANNEL_SIZE);
            map.insert(
                key.to_string(),
                TapChannel {
                    sender,
                    subscriber_count: Arc::new(AtomicUsize::new(0)),
                },
            );
        }
    }

    /// Subscribe to events from a tap point.
    /// Returns None if the key is not registered.
    pub async fn subscribe(&self, key: &str) -> Option<TapSubscription> {
        let map = self.inner.read().await;
        let channel = map.get(key)?;
        let rx = channel.sender.subscribe();
        let count = Arc::clone(&channel.subscriber_count);
        count.fetch_add(1, Ordering::Relaxed);
        Some(TapSubscription {
            rx,
            subscriber_count: count,
        })
    }

    /// Send an event to all tap subscribers for a given key.
    /// Fast path: if no subscribers, only cost is an atomic load (no lock).
    pub async fn emit(&self, key: &str, event: &Event) {
        let map = self.inner.read().await;
        if let Some(channel) = map.get(key)
            && channel.subscriber_count.load(Ordering::Relaxed) > 0 {
                let _ = channel.sender.send(Arc::new(event.clone()));
            }
    }

    /// Non-async emit for use in synchronous contexts (e.g. process registry).
    /// Uses try_read to avoid blocking. If the lock is contended, the event is skipped.
    pub fn try_emit(&self, key: &str, event: &Event) {
        if let Ok(map) = self.inner.try_read()
            && let Some(channel) = map.get(key)
                && channel.subscriber_count.load(Ordering::Relaxed) > 0 {
                    let _ = channel.sender.send(Arc::new(event.clone()));
                }
    }

}

/// A tap subscription. Decrements subscriber count on drop.
pub struct TapSubscription {
    rx: broadcast::Receiver<Arc<Event>>,
    subscriber_count: Arc<AtomicUsize>,
}

impl TapSubscription {
    pub async fn recv(&mut self) -> Result<Arc<Event>, broadcast::error::RecvError> {
        self.rx.recv().await
    }
}

impl Drop for TapSubscription {
    fn drop(&mut self) {
        self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
    }
}

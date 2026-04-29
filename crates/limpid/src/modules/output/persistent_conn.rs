//! Shared persistent-connection orchestration for stream-oriented outputs.
//!
//! TCP and Unix-socket outputs both hold a `Mutex<Option<Stream>>` and
//! follow the same dance on every `write()`:
//!
//! 1. Lock the slot.
//! 2. If a stream is cached, try the framed write. On success bump the
//!    metric and return.
//! 3. On any write error, drop the stream so the next step reconnects.
//! 4. If the slot is empty (initial call, or just-dropped broken conn),
//!    dial a fresh connection, cache it, and write once more.
//!
//! The framing (octet counting vs. newline-terminated, etc.) and the
//! concrete stream type live in the caller — this helper only owns the
//! reconnect + metric-increment loop so that a third persistent-conn
//! output (e.g. TLS) can plug in without reimplementing the dance.
//!
//! Dispatch stays hardcoded per-output (TCP, Unix socket) because
//! there are currently only two implementations; if a third
//! persistent-conn sink lands (e.g. TLS), promote to a registry.
//!
//! Kept `pub(crate)` — internal implementation detail of the output
//! layer, not part of the module contract.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::Mutex;

use crate::metrics::OutputMetrics;

/// Policy trait implemented by each persistent-connection output. The
/// concrete stream type and the framed-write routine stay in the
/// output module — this trait only asks each output to surface them so
/// `write_with_reconnect` can own the cached-stream + reconnect dance.
///
/// Using a trait with `async_trait` (instead of higher-ranked async
/// closures) keeps lifetime bookkeeping local: each output implements
/// `connect` and `write_frame` with their own `&self` borrow and no
/// HRTB gymnastics leak into callers.
#[async_trait::async_trait]
pub(crate) trait PersistentConn: Sync {
    type Stream: Send;

    /// Dial a fresh stream. Called on first write and after a broken-
    /// pipe detection.
    async fn connect(&self) -> Result<Self::Stream>;

    /// Write one framed message over a live stream. The caller has
    /// already extracted the egress bytes from the rendered payload —
    /// this is the boundary where sink-specific framing wraps the
    /// payload bytes for the wire.
    async fn write_frame(&self, stream: &mut Self::Stream, payload: &Bytes) -> Result<()>;
}

/// Write `event` through a persistent stream, reconnecting once if the
/// cached stream is stale. Bumps `events_written` on success.
///
/// On the fast path (cached stream, write succeeds) `connect` is never
/// invoked. A single failed write triggers one reconnect attempt; if
/// that also fails the error is returned to the caller.
pub(crate) async fn write_with_reconnect<P>(
    policy: &P,
    conn: &Mutex<Option<P::Stream>>,
    metrics: &Arc<OutputMetrics>,
    payload: &Bytes,
) -> Result<()>
where
    P: PersistentConn + ?Sized,
{
    let mut guard = conn.lock().await;

    // Fast path: reuse an existing connection.
    if guard.is_some() {
        match policy.write_frame(guard.as_mut().unwrap(), payload).await {
            Ok(()) => {
                metrics.events_written.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            Err(_) => {
                // Broken pipe / reset — drop and reconnect below.
                *guard = None;
            }
        }
    }

    // (Re)connect and write once.
    let stream = policy.connect().await?;
    *guard = Some(stream);
    policy.write_frame(guard.as_mut().unwrap(), payload).await?;
    metrics.events_written.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

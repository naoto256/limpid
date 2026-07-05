//! Shared persistent-connection orchestration for stream-oriented outputs.
//!
//! Outputs with a single persistent stream can hold a
//! `Mutex<Option<Stream>>` and follow the same dance on every `write()`:
//!
//! 1. Lock the slot.
//! 2. If a stream is cached, try the framed write. On success bump the
//!    metric and return.
//! 3. On any write error, drop the stream so the next step reconnects.
//! 4. If the slot is empty (initial call, or just-dropped broken conn),
//!    dial a fresh connection, cache it, and write once more.
//!
//! The framing and the concrete stream type live in the caller — this
//! helper only owns the reconnect + metric-increment loop for outputs
//! that use one cached connection rather than a peer list.
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

/// Write `payload` through a persistent stream, reconnecting once if
/// the cached stream is stale. Bumps `events_written` on success.
///
/// On the fast path (cached stream, write succeeds) `connect` is never
/// invoked. A single failed write triggers one reconnect attempt; if
/// that also fails the error is returned to the caller.
///
/// Not shutdown-aware — cancelling the returned future while the
/// interior `write_frame` is in flight leaves the transport in a
/// partially-sent state. Only kept for the shutdown-drain code path
/// (`consume_shutdown`), where the caller wraps the call in
/// `tokio::time::timeout(SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, ...)` and
/// deliberately accepts the partial-write trade-off inside the
/// drain deadline. Steady-state `consume` uses
/// [`write_with_reconnect_shutdown_aware`] instead, which surfaces
/// the pre-send / send phase split explicitly.
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

/// Result of a single shutdown-aware persistent-connection write.
///
/// The `PreSendShutdown` arm exists to distinguish "shutdown fired
/// before any wire-visible side effect" (honest DLQ `Recovered`)
/// from "the write itself failed" (`Err` — the caller's retry loop
/// decides the disposition). A missing arm here would collapse
/// them into a single `Err`, and callers would either lie about
/// `Recovered` on shutdown or spin an extra retry attempt after
/// the runtime has already asked them to stop.
pub(crate) enum WriteReconnectOutcome {
    /// Payload was written and `events_written` bumped.
    Delivered,
    /// A pre-send phase (connect, or a mutex-guard scoped shutdown
    /// check that fires before `write_frame` runs) observed the
    /// shutdown signal. Nothing was written to the wire on this
    /// call; the caller can safely DLQ-route as `Recovered`.
    PreSendShutdown,
    /// A send-phase error surfaced. Cached stream is invalidated;
    /// caller's retry loop chooses whether to retry or route to
    /// DLQ.
    Err(anyhow::Error),
}

/// Shutdown-aware variant of [`write_with_reconnect`] for the
/// steady-state `consume` path.
///
/// Split into a pre-send phase (mutex lock + connect + pre-write
/// shutdown check) and a send phase (`write_frame`). Only the
/// pre-send phase is racedagainst the shutdown signal:
///
/// - Mutex acquisition is pre-send (short, no side effect).
/// - Fast-path shutdown check runs before `write_frame` on a
///   cached stream. If the runtime already asked for shutdown,
///   return `PreSendShutdown` immediately — no bytes leave the
///   process.
/// - Reconnect (when the cache is empty or the last write
///   failed) is wrapped in `pre_send_or_shutdown`: connect is a
///   TCP handshake with no wire-visible payload, so cancelling it
///   is honest.
/// - `write_frame` runs unwrapped. Shutdown fired during this
///   phase does **not** cancel the write; the caller's per-write
///   timeout (if any) is the only cancellation source. If the
///   runtime task is aborted at the shutdown deadline while
///   `write_frame` is still in flight, the ack handle drops as
///   `Dropped`, and — on a disk queue — that Dropped position
///   holds the cursor for replay on next start (Branch B C2's
///   fail-stop contract).
///
/// This restructures the "one failed write triggers one
/// reconnect" pattern into a two-shot flow: the fast-path failure
/// falls through to the slow path, which itself pre-send-races the
/// connect. That preserves the previous "single reconnect per
/// call" invariant.
pub(crate) async fn write_with_reconnect_shutdown_aware<P>(
    policy: &P,
    conn: &Mutex<Option<P::Stream>>,
    metrics: &Arc<OutputMetrics>,
    payload: &Bytes,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> WriteReconnectOutcome
where
    P: PersistentConn + ?Sized,
{
    let mut guard = conn.lock().await;

    // Fast path: reuse an existing connection.
    if guard.is_some() {
        // Pre-send shutdown check: if the runtime already asked
        // for shutdown, don't begin a write we can't cancel. This
        // check is best-effort — a shutdown flip between the
        // borrow and the `write_frame` await is possible and
        // acceptable; the contract's teeth are in the *absence* of
        // a shutdown race around `write_frame` itself.
        if *shutdown.borrow() {
            return WriteReconnectOutcome::PreSendShutdown;
        }
        match policy.write_frame(guard.as_mut().unwrap(), payload).await {
            Ok(()) => {
                metrics.events_written.fetch_add(1, Ordering::Relaxed);
                return WriteReconnectOutcome::Delivered;
            }
            Err(_) => {
                // Broken pipe / reset — drop the cached stream and
                // fall through to the reconnect path below. Some
                // bytes may already be on the wire; the retry
                // caller's at-least-once contract acknowledges the
                // duplicate risk on reconnect.
                *guard = None;
            }
        }
    }

    // Slow path: (re)connect + write. Connect is a wire-preparation
    // phase with no payload bytes — safe to race against shutdown.
    let stream = match crate::modules::pre_send_or_shutdown(shutdown, policy.connect()).await {
        Some(Ok(s)) => s,
        Some(Err(e)) => return WriteReconnectOutcome::Err(e),
        None => return WriteReconnectOutcome::PreSendShutdown,
    };
    *guard = Some(stream);

    // Second-chance pre-send check: shutdown may have flipped
    // between `connect` completing and here. Same best-effort
    // semantics as above.
    if *shutdown.borrow() {
        return WriteReconnectOutcome::PreSendShutdown;
    }
    match policy.write_frame(guard.as_mut().unwrap(), payload).await {
        Ok(()) => {
            metrics.events_written.fetch_add(1, Ordering::Relaxed);
            WriteReconnectOutcome::Delivered
        }
        Err(e) => {
            *guard = None;
            WriteReconnectOutcome::Err(e)
        }
    }
}

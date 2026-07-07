//! Shared actor skeleton for the batched network outputs (`http`,
//! `otlp_http`, `otlp_grpc`). The three sinks differ only in how an
//! event becomes a payload, how a drained batch becomes one wire
//! request, and how a single send attempt is performed — everything
//! else (buffering, the flusher actor, the retry loop, the shutdown
//! lifecycle, ack-handle resolution, DLQ routing, metrics) is the
//! same protocol and lives here so a lifecycle fix lands once instead
//! of as three parallel patches.
//!
//! ### Actor lifecycle contract
//!
//! The long-lived flusher actor owns every send — both batched
//! flushes and singleton (`batch_size <= 1`). The queue consumer's
//! task is never blocked in a transport `await`; that separation is
//! what makes the actor's `wait_until_shutdown` race effective:
//! shutdown can be observed independently of whatever the consumer is
//! doing. (An earlier implementation performed an inline flush on the
//! consumer's task and blocked the consumer from reaching its own
//! shutdown observation.)
//!
//! - **Normal operation (`shutdown()`)**: NEVER aborts the actor. It
//!   signals cooperative shutdown via `is_shutting_down` +
//!   `flush_notify.notify_waiters()` (to wake the actor loop) and
//!   `shutdown_notify.notify_waiters()` (to wake retry backoff /
//!   `wait_until_shutdown` without conflating them with threshold
//!   flushes), then joins the actor bounded by
//!   `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT`. The actor resolves every
//!   stack-local handle (Delivered / Recovered via DLQ) before
//!   exiting.
//! - **`Drop` fallback (last-resort)**: sync `Drop` cannot `.await`
//!   the actor, so it signals first then calls `abort()` as a last
//!   resort. This path is for teardown scenarios where `shutdown()`
//!   was not invoked (e.g. config reload in tests). The signal gives
//!   the actor a chance to exit cleanly before the abort lands; in
//!   practice the runtime calls `shutdown()` before drop in
//!   production.
//!
//! Earlier versions of these outputs spawned a per-flush *timer* task
//! that held the stack-local batch across `flush_events.await`; when
//! `shutdown()` abort()-ed that task every parked `QueueAckHandle`
//! dropped unresolved (debug `debug_assert!(resolved)` panic, release
//! silent `Dropped` loss). The structural fix moves the abort surface
//! to this single actor handle and keeps `abort()` off the
//! `shutdown()` path entirely.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, watch};

use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::queue::{QueueAckHandle, RetryConfig};

/// One parked event in the batched sink's buffer: the event, the ack
/// handle whose disposition it drives, and an optional bounded-ownership
/// permit.
///
/// The permit is `Some` for events accepted by the steady-state
/// `consume()` path — it holds one slot on the shared
/// [`SinkShared::permits`] semaphore and drops (releases the slot)
/// when the tuple itself drops after the handle resolves. That is
/// what caps how many events the sink can own at once: without it,
/// `consume()` returned `Ok` after a `Vec::push` regardless of
/// flusher state, so a stalled peer let the queue backpressure
/// dissolve into an unbounded in-memory `Vec` (and, on disk-backed
/// queues, into a shutdown-window RAM peak that scaled with the
/// durable backlog).
///
/// The permit is `None` for two entries: `consume_shutdown()` (queue
/// consumer's post-shutdown drain — memory queue capacity + close-
/// drain already caps peak RAM, and disk queues no longer surface
/// unread WAL entries during shutdown, so the semaphore is not
/// load-bearing here) and `consume()` entries whose permit
/// acquisition raced the runtime shutdown watch and bypassed
/// (permit-wait must not prevent the actor from observing shutdown
/// — see `BatchedSink::consume`).
pub(crate) type ParkedEvent = (Event, QueueAckHandle, Option<OwnedSemaphorePermit>);

/// Drop the semaphore permits out of a `Vec<ParkedEvent>` before
/// handing it to a helper whose signature does not (and should not)
/// know about batched-sink internals. Consumes the input to make
/// the permit release visible in the call site — the DLQ route
/// resolves handles but does not need to keep permits alive across
/// the resolve.
fn strip_permits(events: Vec<ParkedEvent>) -> Vec<(Event, QueueAckHandle)> {
    events
        .into_iter()
        .map(|(ev, ack, _permit)| (ev, ack))
        .collect()
}

/// Transport-success outcome from a single export call.
///
/// `rejected` is the number of records the receiver acknowledged as
/// not-stored (OTLP's `partial_success.rejected_log_records`). The
/// HTTP 2xx / gRPC OK is still a transport success — the receiver
/// processed the request, it just refused some records (typically
/// quota / schema / size violations). limpid does not retry rejected
/// records (selective re-send is queued for a later release); the
/// counter split lets `events_failed` reflect the data loss so
/// operator dashboards stay accurate. Transports without a
/// partial-success concept (plain HTTP) report `rejected: 0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SendOutcome {
    pub rejected: u64,
}

/// The per-transport surface of a batched output. Everything the
/// skeleton cannot know: payload rendering, batch preparation, and
/// one send attempt (including peer selection + cooldown, typically
/// via `syslog_peers::RotatingPeers`).
#[async_trait::async_trait]
pub(crate) trait BatchSinkPolicy: Send + Sync + 'static {
    /// Per-event payload buffered until flush.
    type Payload: Send + 'static;
    /// Wire-ready form of one drained batch. Built once per flush
    /// (before the retry loop) and shared by every attempt.
    type Prepared: Send + Sync;

    /// Output kind label used in log / error wording
    /// (e.g. `"http output"`, `"otlp_http output"`).
    fn kind(&self) -> &'static str;

    /// Event → payload. Failures route the offending event to the
    /// DLQ on its own without dropping the rest of the batch.
    fn render(&self, event: &Event) -> Result<Self::Payload>;

    /// Batch payloads → sendable form (e.g. join + optional gzip, or
    /// proto decode + batch-level merge). Runs once per flush, before
    /// the retry loop; a failure routes the whole shippable batch to
    /// the DLQ without consuming the retry budget (the transformation
    /// is deterministic, so re-attempting it cannot succeed).
    fn prepare(&self, payloads: Vec<Self::Payload>) -> Result<Self::Prepared>;

    /// One send attempt. Retry pacing, shutdown races, and disposition
    /// handling belong to the skeleton — implementations just pick a
    /// peer, ship, and record peer cooldown state.
    async fn send(&self, prepared: &Self::Prepared) -> Result<SendOutcome>;
}

/// Batched-output skeleton: buffer + flusher actor + shutdown
/// lifecycle. Output modules embed one of these and delegate their
/// `Output` trait methods to it.
pub(crate) struct BatchedSink<P: BatchSinkPolicy> {
    pub(crate) inner: Arc<SinkShared<P>>,
    pub(crate) batch_size: usize,
    /// Long-lived flusher actor handle; see the module docs for the
    /// full lifecycle contract (`shutdown()` joins, only `Drop`
    /// aborts). `None` only when constructed outside a Tokio runtime
    /// (parsing-only unit tests).
    pub(crate) actor_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// State shared between the consume side (queue consumer hand-off
/// into the buffer) and the flusher actor task.
pub(crate) struct SinkShared<P: BatchSinkPolicy> {
    pub(crate) policy: P,
    /// Operator-facing instance name; surfaced on shutdown-flush
    /// recovery and render-failure records.
    pub(crate) name: String,
    batch_timeout: Duration,
    /// Per-flush retry policy. Without an internal retry, one
    /// transient ship failure would lose the whole drained batch
    /// (the queue layer cannot re-push a buffered batch — its cursor
    /// only advances when each event's ack handle resolves). The
    /// retry budget for batched outputs lives entirely here.
    pub(crate) retry: RetryConfig,
    /// Buffered events awaiting flush, paired with their queue ack
    /// handles and their bounded-ownership permits. Render happens at
    /// flush time so per-event render failures can be routed to DLQ
    /// on their own without dropping the rest of the batch; the ack
    /// handle resolves when the event's disposition is decided
    /// (delivered on flush success, recovered on DLQ landing). See
    /// [`ParkedEvent`] for the permit semantics.
    pub(crate) batch: Mutex<Vec<ParkedEvent>>,
    /// Bounded-ownership permit pool. Each event accepted by
    /// steady-state `consume()` holds one permit for its full
    /// lifetime in the sink (park → flush → handle resolve), then
    /// releases the slot when the tuple drops. Capacity is
    /// `batch_size.saturating_mul(2).max(1)` — one batch worth of
    /// parked events plus one batch worth of in-flight refill room,
    /// which is the smallest bound that does not starve
    /// threshold-driven flushes (a bound below `batch_size` would
    /// block `consume()` before the threshold notify fires, wedging
    /// the sink).
    pub(crate) permits: Arc<Semaphore>,
    /// Runtime shutdown watch, cloned from the `BuildContext` at
    /// construction. Steady-state `consume()` races the permit
    /// acquisition against this receiver so a `consume()` blocked on
    /// a saturated `permits` (peer stalled, retry burning through
    /// its budget) can still observe shutdown and let the actor
    /// exit — otherwise `run_queue_consumer` would be trapped
    /// mid-`consume().await` and never reach its own shutdown drain
    /// (see queue/mod.rs shutdown arm). RetryConfig only accepts a
    /// finite `max_attempts`, so the deadlock window is bounded even
    /// without this race; the plumbing narrows shutdown latency and
    /// keeps the sink well-behaved if future config exposes an
    /// unbounded retry mode.
    pub(crate) shutdown_signal: watch::Receiver<bool>,
    /// `error_log` writer injected at construction time by the
    /// runtime via `BuildContext`. Used by the flush path to route
    /// per-event render failures and shutdown-flush leftovers into
    /// the DLQ. `None` when the operator did not configure
    /// `control { error_log "..." }` — the flush path then falls
    /// back to a `tracing` log line.
    pub(crate) error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    /// Operator-selected confidentiality policy for the tracing-side
    /// fallback line — see [`crate::error_log::ErrorLogFallback`].
    /// Passed through to `route_event_to_dlq` /
    /// `route_shutdown_batch_to_dlq` /
    /// `route_shutdown_batch_ambiguous_to_dlq` from every emission
    /// site so all three ladder states surface consistently.
    pub(crate) error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    /// Threshold-driven flush trigger. `consume()` calls
    /// `notify_one()` when the buffer crosses `batch_size`; the
    /// flusher actor's `select!` wakes and drains. **Never** used to
    /// wake the retry backoff — see `shutdown_notify` for that.
    flush_notify: Notify,
    /// Shutdown broadcast. `shutdown()` calls `notify_waiters()`
    /// after setting `is_shutting_down`. Kept distinct from
    /// `flush_notify` so that a threshold flush can't short-circuit a
    /// retry backoff: if the retry sleep raced against `flush_notify`,
    /// a new event arriving mid-backoff would wake the sleep and
    /// re-fire the failing send immediately, ignoring the configured
    /// backoff. Retry sleeps race only against this notify (and the
    /// `is_shutting_down` check that follows).
    shutdown_notify: Notify,
    /// Co-operative cancel flag. `shutdown()` sets this to true
    /// before notifying the actor; both the actor loop and the
    /// in-flight `flush_events` retry loop check it so they exit
    /// without burning the full retry budget.
    is_shutting_down: AtomicBool,
}

impl<P: BatchSinkPolicy> BatchedSink<P> {
    /// Build the sink and spawn the flusher actor. Spawn is skipped
    /// when there is no Tokio runtime (= parsing-only unit tests
    /// outside `#[tokio::test]`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        policy: P,
        name: &str,
        batch_size: usize,
        batch_timeout: Duration,
        retry: RetryConfig,
        error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
        error_log_fallback: crate::error_log::ErrorLogFallback,
        metrics: Arc<OutputMetrics>,
        shutdown_signal: watch::Receiver<bool>,
    ) -> Self {
        // See `SinkShared::permits` for why the bound is
        // `batch_size * 2` clamped away from zero.
        let permit_capacity = batch_size.saturating_mul(2).max(1);
        let inner = Arc::new(SinkShared {
            policy,
            name: name.to_string(),
            batch_timeout,
            retry,
            batch: Mutex::new(Vec::with_capacity(batch_size.max(1))),
            permits: Arc::new(Semaphore::new(permit_capacity)),
            shutdown_signal,
            error_log,
            error_log_fallback,
            metrics,
            flush_notify: Notify::new(),
            shutdown_notify: Notify::new(),
            is_shutting_down: AtomicBool::new(false),
        });
        let actor_handle = if tokio::runtime::Handle::try_current().is_ok() {
            let actor_inner = Arc::clone(&inner);
            Some(tokio::spawn(async move {
                flusher_actor_loop(actor_inner).await;
            }))
        } else {
            None
        };
        Self {
            inner,
            batch_size,
            actor_handle: Mutex::new(actor_handle),
        }
    }

    /// Batched-buffer consume: park the `(Event, ack)` pair in the
    /// in-memory buffer and arm/run the flush. The ack handle stays
    /// with the event and resolves at flush time (delivered or
    /// recovered) — not now. The actor drains the buffer on
    /// `flush_notify`, on `batch_timeout`, or on `is_shutting_down`;
    /// this works for both batched (`batch_size > 1`) and singleton
    /// (`batch_size <= 1`) modes — for the latter the actor flushes a
    /// one-element batch on each notify.
    pub(crate) async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        // Bounded ownership: hold one permit for the event's full
        // lifetime in the sink. The permit drops (releases its slot)
        // when the tuple drops after the handle resolves. Race the
        // permit acquire with the runtime shutdown watch and the
        // cooperative `is_shutting_down` flag so a saturated
        // semaphore does not trap this task past the moment shutdown
        // fires — a `consume().await` stuck here would prevent the
        // queue consumer from reaching its own drain arm.
        let permit: Option<OwnedSemaphorePermit> = if self
            .inner
            .is_shutting_down
            .load(Ordering::Acquire)
            || *self.inner.shutdown_signal.borrow()
        {
            None
        } else {
            let mut shutdown_rx = self.inner.shutdown_signal.clone();
            let permits = Arc::clone(&self.inner.permits);
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => None,
                acquired = permits.acquire_owned() => {
                    // `acquire_owned` only errors when the
                    // semaphore is `close()`d; we never close
                    // this permit pool, so treat the Err as a
                    // bug rather than silently bypassing.
                    Some(acquired.expect("batched-sink permit semaphore was closed unexpectedly"))
                }
            }
        };
        let should_flush = {
            let mut buf = self.inner.batch.lock().await;
            buf.push((event.clone(), ack, permit));
            buf.len() >= self.batch_size
        };
        if should_flush {
            self.inner.flush_notify.notify_one();
        }
        // The actor's `select!` already races the `batch_timeout`
        // sleep — no separate timer to arm.
        Ok(())
    }

    /// Drain-time per-event entry: park the `(event, ack, permit)`
    /// tuple in the buffer that the post-loop `shutdown()` call will
    /// drain bounded. Deliberately does NOT trigger a flush from
    /// here — that would re-enter the steady-state retry path
    /// (`flush_events`' exponential backoff loop) which the shutdown
    /// contract forbids. The buffer holds the handle until
    /// `shutdown()` resolves it via `flush_events_at_shutdown`'s
    /// bounded single attempt + DLQ route.
    ///
    /// Permit acquisition is bypassed here (permit = `None`): the
    /// queue consumer's shutdown drain already bounds peak RAM
    /// (memory queues are capped by channel capacity + close-drain;
    /// disk queues no longer surface unread WAL entries during
    /// shutdown at all — see the queue/mod.rs shutdown arm), so the
    /// semaphore is not load-bearing on this path and adding it
    /// would open a subtle deadlock: `consume_shutdown` is called
    /// from inside the same task that would eventually drop the
    /// permits when tuples resolve, so a saturated semaphore here
    /// would block the drain until an in-flight batch happened to
    /// resolve.
    pub(crate) async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut buf = self.inner.batch.lock().await;
        buf.push((event.clone(), ack, None));
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        // 1. Signal cooperative shutdown. `is_shutting_down`
        //    propagates to the actor's outer `select!`, to the
        //    retry sleep race, and (via the transport-cancel race
        //    in `flush_events`) to the in-flight send call itself.
        //    Two notifies: `flush_notify` wakes the actor loop's
        //    `select!`, and `shutdown_notify` wakes anything else
        //    waiting on the shutdown signal (retry backoff sleep,
        //    `wait_until_shutdown`). Keeping them separate lets a
        //    threshold flush arrive without collapsing the retry
        //    backoff — see the `shutdown_notify` field doc.
        self.inner.is_shutting_down.store(true, Ordering::Release);
        self.inner.flush_notify.notify_waiters();
        self.inner.shutdown_notify.notify_waiters();

        // 2. Bounded join of the flusher actor. The actor's invariant
        //    is that EVERY stack-local `(Event, QueueAckHandle)` it
        //    has taken is resolved (Delivered on success, Recovered on
        //    transport cancel / retry exhaustion / DLQ recovery)
        //    before `flush_events` returns and before the actor exits.
        //    The cooperative cancels above guarantee the actor reaches
        //    that exit promptly even when a peer stalls — so a healthy
        //    actor finishes well inside `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT`.
        //    The timeout here is a defensive guard against a future
        //    bug (e.g. a new transport path that forgets the cancel
        //    race); it is NOT a contract that allows unresolved
        //    handles. We deliberately do NOT `abort()` — abort would
        //    re-open the unresolved-handle leak.
        let handle_opt = self.actor_handle.lock().await.take();
        if let Some(h) = handle_opt {
            let _ = tokio::time::timeout(crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, h).await;
        }

        // 3. Final drain. The buffer holds only events pushed via
        //    `consume_shutdown` (the queue consumer's drain path
        //    after shutdown signal) plus anything that arrived too
        //    late for the actor's last iteration. The actor's own
        //    in-flight batch was already resolved by `flush_events`
        //    before the actor exited. `flush_events_at_shutdown`
        //    does one bounded send attempt then routes the rest to
        //    DLQ + Recovered.
        let leftover = std::mem::take(&mut *self.inner.batch.lock().await);
        self.inner.flush_events_at_shutdown(leftover).await;
        Ok(())
    }

    /// Wedge-exit resolve. Same signal + join preamble as
    /// [`BatchedSink::shutdown`], but the final drain routes every
    /// parked handle through [`crate::modules::route_shutdown_batch_ambiguous_to_dlq`]
    /// with a synthesized wedge error rather than through
    /// `flush_events_at_shutdown` — no `policy.send` call is ever
    /// entered. See the [`crate::modules::Output::shutdown_wedged`]
    /// trait doc for the full lifecycle contract.
    ///
    /// The signal + join preamble is still needed so the long-lived
    /// flusher actor exits cleanly: signalling `is_shutting_down`
    /// makes the actor's own in-flight `flush_events` observe the
    /// shutdown notify and resolve its stack-local batch via the
    /// steady-state ambiguous-DLQ path. Skipping the signal would
    /// let the actor keep running past this method's return, leaving
    /// its parked handles unresolved.
    pub(crate) async fn shutdown_wedged(&self) -> Result<()> {
        self.inner.is_shutting_down.store(true, Ordering::Release);
        self.inner.flush_notify.notify_waiters();
        self.inner.shutdown_notify.notify_waiters();

        let handle_opt = self.actor_handle.lock().await.take();
        if let Some(h) = handle_opt {
            let _ = tokio::time::timeout(crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT, h).await;
        }

        let leftover = std::mem::take(&mut *self.inner.batch.lock().await);
        if !leftover.is_empty() {
            let wedge_err = anyhow::anyhow!(
                "output '{}' wedged: skipping shutdown flush; parked buffer resolved via ambiguous DLQ (no send attempted)",
                self.inner.name
            );
            let events = strip_permits(leftover);
            crate::modules::route_shutdown_batch_ambiguous_to_dlq(
                self.inner.error_log.as_ref(),
                self.inner.error_log_fallback,
                &self.inner.metrics,
                &self.inner.name,
                events,
                &wedge_err,
            )
            .await;
        }
        Ok(())
    }
}

impl<P: BatchSinkPolicy> Drop for BatchedSink<P> {
    fn drop(&mut self) {
        // Signal the actor cooperatively before falling back to
        // abort — `shutdown()` should have already done this and
        // joined, but Drop is the last-resort path (Output dropped
        // without an explicit `shutdown()` call, e.g. config
        // teardown in tests). The actor's stack-local batch on a
        // bare abort would drop handles unresolved; setting
        // `is_shutting_down` and both notifies here gives it a
        // chance to exit cleanly first, even though we cannot await
        // its completion from a sync Drop.
        self.inner.is_shutting_down.store(true, Ordering::Release);
        self.inner.flush_notify.notify_waiters();
        self.inner.shutdown_notify.notify_waiters();
        if let Some(h) = self.actor_handle.get_mut().take() {
            h.abort();
        }
        // Best-effort warn on leaked buffered events. Awaiting the
        // lock here would block the warn under contention; try_lock
        // is the right behaviour for a Drop path. Each leftover
        // handle's own Drop impl fires `Dropped` back at the queue
        // consumer, which bumps `events_failed` but writes no DLQ
        // record — on a disk queue the fail-stop wedge holds the
        // cursor for replay on next start; on a memory queue the
        // event is unrecoverable. See the Disposition contract in
        // `docs/src/operations/error-log.md`.
        if let Ok(buf) = self.inner.batch.try_lock()
            && !buf.is_empty()
        {
            tracing::warn!(
                "{}: {} events in buffer at shutdown (Dropped — counted in events_failed, no DLQ replay)",
                self.inner.policy.kind(),
                buf.len()
            );
        }
    }
}

/// Long-lived flusher actor. Drains the buffer on either of three
/// triggers: threshold-driven `flush_notify`, timer-driven
/// `batch_timeout` sleep, or shutdown via `is_shutting_down`. See the
/// module docs for the lifecycle contract (`shutdown()` never aborts
/// this actor).
async fn flusher_actor_loop<P: BatchSinkPolicy>(inner: Arc<SinkShared<P>>) {
    loop {
        if inner.is_shutting_down.load(Ordering::Acquire) {
            break;
        }
        // Race the triggers. `biased` makes the notify arm the
        // priority one so a `notify_waiters()` race with a
        // threshold notify still exits cleanly on the follow-up
        // `is_shutting_down` check.
        let sleep_fut = tokio::time::sleep(inner.batch_timeout);
        tokio::pin!(sleep_fut);
        tokio::select! {
            biased;
            _ = inner.flush_notify.notified() => {}
            _ = &mut sleep_fut => {}
        }
        if inner.is_shutting_down.load(Ordering::Acquire) {
            break;
        }
        let batch = {
            let mut buf = inner.batch.lock().await;
            std::mem::take(&mut *buf)
        };
        if !batch.is_empty() {
            // `flush_events` resolves every handle (Delivered on
            // success, Recovered on retry-exhausted DLQ). It checks
            // `is_shutting_down` between retry attempts so a
            // shutdown signal collapses the retry budget to one
            // attempt instead of burning the full backoff window.
            inner.flush_events(batch).await;
        }
    }
}

impl<P: BatchSinkPolicy> SinkShared<P> {
    /// Yield when the cooperative shutdown signal is observed. Used
    /// in `tokio::select!` to race the transport send future so a
    /// stalled peer cannot trap the actor task with the batch on its
    /// stack past the runtime shutdown deadline.
    ///
    /// The double-check around `notified()` closes the lost-wake race:
    /// if `is_shutting_down` flips between our load and the call to
    /// `notified()`, the re-check catches it; otherwise the next
    /// `notify_waiters()` (from `shutdown()`) wakes us.
    async fn wait_until_shutdown(&self) {
        loop {
            if self.is_shutting_down.load(Ordering::Acquire) {
                return;
            }
            let notified = self.shutdown_notify.notified();
            if self.is_shutting_down.load(Ordering::Acquire) {
                return;
            }
            notified.await;
            // After the notify wake, the next iteration's `load`
            // decides whether to return (shutdown observed) or loop
            // (spurious wake from a threshold-driven flush nudge).
        }
    }

    /// Render every buffered event, routing per-event render failures
    /// to the DLQ (Recovered) on their own so they don't drop the
    /// rest of the batch. `site` distinguishes the steady-state and
    /// shutdown flush wordings in DLQ records.
    async fn render_batch(
        &self,
        batch: Vec<ParkedEvent>,
        site: &str,
    ) -> (Vec<P::Payload>, Vec<ParkedEvent>) {
        let mut payloads: Vec<P::Payload> = Vec::with_capacity(batch.len());
        let mut shippable: Vec<ParkedEvent> = Vec::with_capacity(batch.len());
        let mut render_failures: Vec<(Event, QueueAckHandle, anyhow::Error)> = Vec::new();
        for (ev, ack, permit) in batch {
            match self.policy.render(&ev) {
                Ok(p) => {
                    payloads.push(p);
                    shippable.push((ev, ack, permit));
                }
                // Drop the permit as the render failure is fanned
                // out; the DLQ route below owns disposition without
                // needing the semaphore slot.
                Err(e) => {
                    drop(permit);
                    render_failures.push((ev, ack, e));
                }
            }
        }
        for (ev, ack, err) in render_failures {
            let reason = format!("render failed during {}: {}", site, err);
            let __dlq_outcome = crate::modules::route_event_to_dlq(
                self.error_log.as_ref(),
                self.error_log_fallback,
                &self.metrics,
                &self.name,
                &ev,
                ack.position(),
                &reason,
            )
            .await;
            crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
        }
        (payloads, shippable)
    }

    /// Commit a transport-success outcome: split the batch between
    /// `events_written` (accepted) and `events_failed` (rejected via
    /// partial success) and resolve every handle. The receiver does
    /// not identify *which* records were rejected, so we approximate
    /// by routing the trailing `rejected` entries to the DLQ and
    /// resolving the rest as Delivered — metric totals are accurate
    /// either way. `rejected: 0` (the plain-HTTP case) resolves the
    /// whole batch as Delivered.
    async fn resolve_send_success(&self, shippable: Vec<ParkedEvent>, outcome: SendOutcome) {
        let count = shippable.len() as u64;
        let rejected = outcome.rejected.min(count);
        let written = count - rejected;
        if written > 0 {
            self.metrics
                .events_written
                .fetch_add(written, Ordering::Relaxed);
        }
        let split = (count - rejected) as usize;
        let mut iter = shippable.into_iter();
        for (_, ack, _permit) in iter.by_ref().take(split) {
            ack.resolve_delivered();
        }
        // Per-event DLQ routing for the trailing `rejected` entries.
        // `events_failed` is bumped by `resolve_ack_from_dlq_outcome`
        // (memory + Recovered-on-disk arms) or by
        // `handle_ack_disposition(Dropped)` on the ack side
        // (Dropped-on-disk arm); the aggregate across all rejected
        // handles equals `rejected`.
        for (ev, ack, _permit) in iter {
            let reason = "collector reported partial_success rejection".to_string();
            let __dlq_outcome = crate::modules::route_event_to_dlq(
                self.error_log.as_ref(),
                self.error_log_fallback,
                &self.metrics,
                &self.name,
                &ev,
                ack.position(),
                &reason,
            )
            .await;
            crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
        }
    }

    /// Drain + ship one batch, resolving each handle to its final
    /// disposition. Infallible from the caller's POV: every entry
    /// has its disposition committed before this returns. Render
    /// failures route the offending event to DLQ on its own
    /// (resolve_recovered); the rest proceed to the send loop.
    /// Transport failures consume the per-flush retry budget; on
    /// exhaust the whole shippable subset is routed to DLQ.
    async fn flush_events(&self, batch: Vec<ParkedEvent>) {
        if batch.is_empty() {
            return;
        }
        let (payloads, shippable) = self.render_batch(batch, "batch flush").await;
        if payloads.is_empty() {
            return;
        }
        let prepared = match self.policy.prepare(payloads) {
            Ok(p) => p,
            Err(e) => {
                // Deterministic transformation failure (encode /
                // decode / compress): re-attempting cannot succeed,
                // so skip the retry budget and route straight to DLQ.
                let reason = format!("flush failed: {}", e);
                for (ev, ack, _permit) in shippable {
                    let __dlq_outcome = crate::modules::route_event_to_dlq(
                        self.error_log.as_ref(),
                        self.error_log_fallback,
                        &self.metrics,
                        &self.name,
                        &ev,
                        ack.position(),
                        &reason,
                    )
                    .await;
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
                }
                return;
            }
        };
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        // Distinguishes "retry budget exhausted, transport reported
        // Err" (safe to route Recovered — no wire commit was
        // observed, or the transport's own Err semantics rule it
        // out) from "shutdown cancelled the in-flight send" (the
        // future was dropped while the request may already have
        // been serialised into the kernel socket buffer). Only the
        // latter needs the ambiguous-wire discipline: `Recovered`
        // in that case would fabricate an at-least-once guarantee
        // that lets a partial-wire success be replayed and
        // double-delivered.
        let mut shutdown_cancelled_send = false;
        let final_err = loop {
            // Race the transport against the shutdown signal. A
            // stalled peer (= TCP accepted but never responds) holds
            // the send future past the runtime shutdown deadline —
            // without the race the actor task gets aborted with the
            // stack-local shippable Vec, dropping every parked
            // QueueAckHandle unresolved (the unresolved-handle
            // regression). With the race, a `shutdown()` mid-flight
            // cancels the send future (the transport client cancels
            // the connection on drop). The dropped future's wire
            // state is ambiguous — the ambiguous DLQ route below
            // forces Dropped rather than Recovered.
            let send_outcome = tokio::select! {
                biased;
                res = self.policy.send(&prepared) => Some(res),
                _ = self.wait_until_shutdown() => None,
            };
            match send_outcome {
                Some(Ok(outcome)) => {
                    self.resolve_send_success(shippable, outcome).await;
                    return;
                }
                None => {
                    shutdown_cancelled_send = true;
                    break anyhow::anyhow!(
                        "shutdown cancelled in-flight send (collapsed retry budget)"
                    );
                }
                Some(Err(e)) => {
                    attempt += 1;
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.retry.max_attempts {
                        break e;
                    }
                    // Cooperative shutdown cancel: if `shutdown()`
                    // signalled mid-retry, abandon the budget and
                    // route the shippable batch straight to DLQ.
                    // Burning the full backoff window would outlast
                    // the runtime's 10s shutdown timeout and leak
                    // the stack-local handles when the task is
                    // aborted by the runtime.
                    if self.is_shutting_down.load(Ordering::Acquire) {
                        break e;
                    }
                    tracing::warn!(
                        "{} '{}': flush attempt {}/{} failed: {} — retrying in {:?}",
                        self.policy.kind(),
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    // Race the sleep against a shutdown wake. The
                    // bare check before the sleep only catches a
                    // shutdown that arrived between attempts; a
                    // shutdown that fires *during* the sleep would
                    // otherwise be stuck until the sleep elapses
                    // (5s+ in practice, longer than the runtime
                    // shutdown budget).
                    //
                    // Use `wait_until_shutdown()` (not a bare
                    // `shutdown_notify.notified()`) so the lost-wake
                    // window is closed: `shutdown()` first sets
                    // `is_shutting_down` and *then* calls
                    // `notify_waiters`. A raw `notified()` registered
                    // between those two steps would miss the wake and
                    // sleep the full backoff; `wait_until_shutdown`
                    // does the load-notified-recheck dance so either
                    // ordering wakes it. See `wait_until_shutdown`'s
                    // docs for the argument.
                    //
                    // NOT `flush_notify`: a threshold-driven flush wake
                    // arriving mid-backoff would otherwise cut the
                    // backoff short and re-fire the failing send
                    // instantly, hammering a failing collector and
                    // ignoring the configured retry pacing. Retry
                    // pacing survives incoming traffic; only shutdown
                    // is allowed to short-circuit it.
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.wait_until_shutdown() => {}
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        };
        // Terminal disposition: split by why we left the loop.
        //
        // - Shutdown cancelled the in-flight send → the wire state
        //   is ambiguous (HTTP / OTLP transports flush request bytes
        //   into the kernel socket buffer well before the send
        //   future resolves, so a mid-flight drop can happen after
        //   the peer has already read part or all of the batch).
        //   `route_shutdown_batch_ambiguous_to_dlq` writes the DLQ
        //   record for the operator's audit trail but forces
        //   `Dropped` — on a disk queue the fail-stop wedge holds
        //   the cursor for next-start reconciliation instead of
        //   advancing past a batch that may already have shipped.
        //
        // - Retry exhausted from transport `Err` → `Recovered` via
        //   the plain per-event DLQ route. Retry-exhausted `Err` is
        //   the transport's own signal that no wire commit is
        //   pending: HTTP returned a status / connection error,
        //   OTLP returned a gRPC status, the send future is done.
        //   The disk-queue cursor advances past this batch (the DLQ
        //   record is now the load-bearing recovery).
        if shutdown_cancelled_send {
            crate::modules::route_shutdown_batch_ambiguous_to_dlq(
                self.error_log.as_ref(),
                self.error_log_fallback,
                &self.metrics,
                &self.name,
                strip_permits(shippable),
                &final_err,
            )
            .await;
        } else {
            let reason = format!("flush failed after {} attempts: {}", attempt, final_err);
            for (ev, ack, _permit) in shippable {
                let __dlq_outcome = crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    self.error_log_fallback,
                    &self.metrics,
                    &self.name,
                    &ev,
                    ack.position(),
                    &reason,
                )
                .await;
                crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
            }
        }
    }

    /// Shutdown single-attempt flush; never uses the steady-state retry
    /// budget. Render failures route per-event to DLQ as before,
    /// partial successes are honoured (the rejected tail goes to DLQ
    /// with the `partial_success` reason — distinct from transport
    /// failure), and the shippable subset gets one send attempt
    /// wrapped in `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT`. The `shippable`
    /// vector is held in this frame across the `timeout()` boundary so
    /// an `Elapsed` outcome does NOT drop it — otherwise the inner
    /// handles would fire `QueueAckHandle::Drop` and be counted as
    /// silent loss.
    async fn flush_events_at_shutdown(&self, batch: Vec<ParkedEvent>) {
        if batch.is_empty() {
            return;
        }
        let (payloads, shippable) = self.render_batch(batch, "shutdown flush").await;
        if payloads.is_empty() {
            return;
        }
        let prepared = match self.policy.prepare(payloads) {
            Ok(p) => p,
            Err(e) => {
                let err = anyhow::anyhow!("transport error: {}", e);
                crate::modules::route_shutdown_batch_to_dlq(
                    self.error_log.as_ref(),
                    self.error_log_fallback,
                    &self.metrics,
                    &self.name,
                    strip_permits(shippable),
                    &err,
                )
                .await;
                return;
            }
        };
        let send_outcome = tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            self.policy.send(&prepared),
        )
        .await;
        match send_outcome {
            Ok(Ok(outcome)) => {
                self.resolve_send_success(shippable, outcome).await;
            }
            Ok(Err(send_err)) => {
                let err = anyhow::anyhow!("transport error: {}", send_err);
                crate::modules::route_shutdown_batch_to_dlq(
                    self.error_log.as_ref(),
                    self.error_log_fallback,
                    &self.metrics,
                    &self.name,
                    strip_permits(shippable),
                    &err,
                )
                .await;
            }
            Err(_elapsed) => {
                // The 3 s attempt timeout fired after `policy.send`
                // was already running — the same partial-wire
                // ambiguity as the steady-state shutdown-cancel arm.
                // Use the ambiguous DLQ route so a partially-shipped
                // batch does not double-deliver on next-start replay.
                let err = anyhow::anyhow!(
                    "deadline exceeded after {:?} during shutdown flush",
                    crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
                );
                crate::modules::route_shutdown_batch_ambiguous_to_dlq(
                    self.error_log.as_ref(),
                    self.error_log_fallback,
                    &self.metrics,
                    &self.name,
                    strip_permits(shippable),
                    &err,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Structural pin: `flush_events`' shutdown-cancel arm and
    /// `flush_events_at_shutdown`'s `Elapsed` arm both route through
    /// `route_shutdown_batch_ambiguous_to_dlq`, not the plain
    /// `route_shutdown_batch_to_dlq`.
    ///
    /// The batched transports (HTTP / OTLP-HTTP / OTLP-gRPC) push
    /// request bytes into the kernel socket buffer well before
    /// `policy.send(...)` resolves, so both cancelling the future
    /// mid-flight (`wait_until_shutdown` wins the select) and
    /// timing it out after `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` leave
    /// the wire state ambiguous. Honest `Recovered` on a disk queue
    /// would advance the cursor past a batch the downstream may
    /// have already received; forcing `Dropped` wedges the cursor
    /// for next-start reconciliation instead. The plain helper is
    /// still used for retry-exhausted `Err` (retry-exhausted `Err`
    /// is the transport's own no-commit signal, so `Recovered` is
    /// honest there) and for the deterministic prepare-failure arm.
    #[test]
    fn shutdown_ambiguous_arms_use_ambiguous_dlq_helper() {
        let src = include_str!("batched.rs");

        // `flush_events` steady-state shutdown arm: check the
        // terminal-disposition split. The `shutdown_cancelled_send`
        // branch must call the ambiguous helper; the `else` branch
        // (retry exhausted) keeps `route_event_to_dlq` per event.
        let fe_start = src
            .find("async fn flush_events(&self, batch: Vec<ParkedEvent>)")
            .expect("flush_events fn must exist");
        let fe_end = src[fe_start..]
            .find("async fn flush_events_at_shutdown(")
            .expect("flush_events must precede flush_events_at_shutdown");
        let fe_body = &src[fe_start..fe_start + fe_end];
        assert!(
            fe_body.contains("if shutdown_cancelled_send {"),
            "flush_events must split its terminal disposition on shutdown_cancelled_send"
        );
        assert!(
            fe_body.contains("route_shutdown_batch_ambiguous_to_dlq("),
            "flush_events shutdown-cancel arm must call the ambiguous DLQ helper"
        );

        // `flush_events_at_shutdown` Elapsed arm: same check. The
        // Err(_elapsed) branch must call the ambiguous helper.
        let fs_start = src
            .find("async fn flush_events_at_shutdown(&self, batch: Vec<ParkedEvent>)")
            .expect("flush_events_at_shutdown fn must exist");
        // Take the rest of the file — the fn is the last thing here.
        let fs_body = &src[fs_start..];
        // Locate the `Err(_elapsed)` arm and check its body block
        // reaches the ambiguous helper before the next top-level
        // `match` arm boundary.
        let elapsed_arm = fs_body
            .find("Err(_elapsed) => {")
            .expect("Elapsed arm must exist");
        let elapsed_tail = &fs_body[elapsed_arm..];
        // Take a generous window up to the closing brace of the
        // whole match — the ambiguous call must appear inside.
        assert!(
            elapsed_tail
                .split("\n            }")
                .next()
                .unwrap_or("")
                .contains("route_shutdown_batch_ambiguous_to_dlq("),
            "flush_events_at_shutdown Elapsed arm must call the ambiguous DLQ helper"
        );
    }
}

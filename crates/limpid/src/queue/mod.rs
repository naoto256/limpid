//! Output queue: async FIFO between pipeline and output modules.
//!
//! Two implementations:
//! - **Memory queue** (default): fast, events lost on process restart
//! - **Disk queue**: WAL-based, survives restarts, configurable max size

pub mod disk;

use std::sync::Arc;

use tracing::{info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
#[cfg(test)]
use crate::event::Event;
use crate::event::QueuedEvent;

// ---------------------------------------------------------------------------
// Declarative schema for the per-output `queue { ... }` sub-block
// ---------------------------------------------------------------------------
//
// Every output supports the same `queue { type / path / max_size /
// capacity }` block, parsed by `QueueConfig::from_output_properties`
// below. We declare its shape once so every Module's
// `property_schema()` can splice in `QUEUE_PROPERTY_SPEC` instead of
// repeating the same four entries. Keeping the schema co-located with
// the parsing code is intentional — if a queue option is renamed
// here, both surfaces update in the same edit.

const QUEUE_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "type",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["memory", "disk"]),
    },
    PropertySpec {
        name: "path",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "max_size",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Size,
    },
    PropertySpec {
        name: "capacity",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

/// `queue { ... }` sub-block specification shared by every output
/// Module's property schema. Spread into a Module schema as a single
/// `PropertySpec` value:
///
/// ```ignore
/// const SYSLOG_TCP_OUTPUT_SCHEMA: &[PropertySpec] = &[
///     PropertySpec { name: "address", ... },
///     crate::queue::QUEUE_PROPERTY_SPEC,
/// ];
/// ```
pub const QUEUE_PROPERTY_SPEC: PropertySpec = PropertySpec {
    name: "queue",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::Block(QUEUE_BLOCK_PROPERTIES),
};

// ---------------------------------------------------------------------------
// Declarative schema for the per-output `retry { ... }` sub-block
// ---------------------------------------------------------------------------
//
// Honored by `RetryConfig::from_output_properties` (below) for *every*
// output the queue consumer drives. Historically only the two OTLP
// outputs declared `retry` in their `property_schema()`, so non-OTLP
// configs that set `retry { ... }` got hard-failed at `--check` time
// as "unknown property" even though the runtime would happily accept
// them. Hoisting the schema here and splicing the const into every
// output's schema closes that gap without touching the runtime —
// schema acceptance now matches the runtime's existing intent.
//
// Sub-property names mirror the keys read by
// `RetryConfig::from_output_properties`; renames stay in lock-step
// with the parser because both definitions live in this file.

const RETRY_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "max_attempts",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "initial_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "max_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "backoff",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["fixed", "exponential"]),
    },
];

/// `retry { ... }` sub-block specification shared by every output
/// Module's property schema. Splice alongside [`QUEUE_PROPERTY_SPEC`].
pub const RETRY_PROPERTY_SPEC: PropertySpec = PropertySpec {
    name: "retry",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::Block(RETRY_BLOCK_PROPERTIES),
};

// ---------------------------------------------------------------------------
// Queue item — every queue (memory + disk) now carries `Event` end-to-end
// ---------------------------------------------------------------------------
//
// Before this change the queue distinguished between a pre-rendered sink
// payload (memory hot path) and an `OwnedEvent` (disk persist / inject).
// After this change the queue uniformly carries `Event`; rendering moves from
// the pipeline-side (enqueue time) to the consumer-side (send time,
// inside each sink's `Output::consume`). Memory and disk queues are
// distinguished only by the `SenderInner` variant — there is no
// `QueueKind` tag because the pipeline never branches on it.

/// Configuration for an output queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub queue_type: QueueType,
    /// Maximum number of events for the memory queue. Ignored by the disk
    /// queue, which is sized in bytes via `QueueType::Disk { max_size }`.
    pub capacity: usize,
}

/// Why a queue enqueue failed. Each variant corresponds to a code
/// path inside `QueueSender::send` / `DiskQueueSender::send`.
///
/// `#[non_exhaustive]` so future recovery-routing work can add
/// variants without breaking external matches; downstream code should
/// use `Result` patterns that accept new variants forward-compatibly.
/// Current callers in-tree match exhaustively.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueueSendError {
    /// Memory queue's receiving end was dropped (consumer task gone,
    /// daemon usually shutting down). Comes from
    /// `tokio::sync::mpsc::Sender::send().await.is_err()`.
    #[error("queue receiver dropped (channel closed)")]
    ChannelClosed,

    /// Disk queue failed to serialise the event to JSON before
    /// writing. The underlying error is preserved for logging.
    #[error("disk queue: failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),

    /// `tokio::task::spawn_blocking` returned a `JoinError` while the
    /// disk queue was performing the synchronous segment write.
    #[error("disk queue: write task failed: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// Disk queue segment write failed — covers open/append/flush
    /// errors inside `write_to_segment`. The helper currently
    /// collapses the specific I/O error onto its own error log; this
    /// variant signals only that the write didn't land.
    #[error("disk queue: segment write failed")]
    DiskWrite,
}

#[derive(Debug, Clone)]
pub enum QueueType {
    Memory,
    Disk {
        path: String,
        max_size: u64, // bytes
    },
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            queue_type: QueueType::Memory,
            capacity: 65536,
        }
    }
}

impl QueueConfig {
    /// Parse from an output definition's `queue { ... }` block.
    pub fn from_output_properties(
        output_name: &str,
        output_props: &[Property],
    ) -> anyhow::Result<Self> {
        if let Some(block) = props::get_block(output_props, "queue") {
            let queue_type = match props::get_ident(block, "type").as_deref() {
                Some("disk") => {
                    let path = props::get_string(block, "path")
                        .unwrap_or_else(|| format!("/var/lib/limpid/queues/{}", output_name));
                    let max_size = match props::get_string(block, "max_size") {
                        Some(s) => props::parse_size(&s)?,
                        None => 0, // 0 = unlimited
                    };
                    QueueType::Disk { path, max_size }
                }
                _ => QueueType::Memory,
            };
            let capacity = props::get_positive_int(block, "capacity")?.unwrap_or(65536) as usize;
            Ok(QueueConfig {
                queue_type,
                capacity,
            })
        } else {
            Ok(QueueConfig::default())
        }
    }
}

/// Handle for sending events into a queue. Cheaply cloneable.
#[derive(Clone)]
pub struct QueueSender {
    inner: SenderInner,
    // 0.8 metrics will surface queue-level counters (depth, enqueue
    // pressure) that need a label distinct from the output name.
    // Holding the Arc<String> avoids re-plumbing it then; the per-queue
    // allocation is trivial.
    #[allow(dead_code)]
    name: Arc<String>,
    /// Optional metrics — if set, `send()` increments `events_received` on success.
    /// Set by the runtime after the output module's metrics handle is available.
    metrics: Option<Arc<crate::metrics::OutputMetrics>>,
}

#[derive(Clone)]
enum SenderInner {
    Memory(tokio::sync::mpsc::Sender<QueuedEvent>),
    Disk(disk::DiskQueueSender),
}

impl QueueSender {
    /// Send an `Event` into the queue.
    ///
    /// Both queue kinds carry `Event` end-to-end after this change: memory
    /// queues forward the event to the consumer where the configured
    /// output renders it inside `Output::consume`, and disk queues
    /// serialise the same `Event` to JSON for replay. There is no
    /// longer a `Rendered`-vs-`Owned` discriminator at this level
    /// because the queue does not see sink-specific payloads anymore.
    pub async fn send(&self, event: QueuedEvent) -> Result<(), QueueSendError> {
        let result: Result<(), QueueSendError> = match &self.inner {
            SenderInner::Memory(tx) => tx
                .send(event)
                .await
                .map_err(|_| QueueSendError::ChannelClosed),
            SenderInner::Disk(tx) => tx.send(event).await,
        };
        if let Some(m) = &self.metrics {
            if result.is_ok() {
                m.events_received.inc();
            } else {
                // Enqueue failure: memory-queue receiver dropped (=
                // consumer task gone, daemon usually shutting down),
                // or disk-queue serialise/write error. From this
                // sender's POV the event never made it into the
                // queue and can't be retried by the consumer (the
                // consumer never sees it). Bump `events_failed` so
                // the per-output enqueue failure is visible in
                // metrics; the pipeline-side caller additionally
                // routes the lost event through the dead-letter
                // path.
                m.events_failed.inc();
            }
        }
        result
    }

    /// Attach output metrics so subsequent `send()` calls count `events_received`.
    pub fn attach_metrics(&mut self, metrics: Arc<crate::metrics::OutputMetrics>) {
        self.metrics = Some(metrics);
    }

    /// Access the attached metrics (e.g. to increment `events_injected` on inject).
    pub fn metrics(&self) -> Option<&Arc<crate::metrics::OutputMetrics>> {
        self.metrics.as_ref()
    }
}

/// Position of an event in its queue, captured at `recv()` time and
/// carried through the event's lifetime on a [`QueueAckHandle`]. The
/// consumer feeds it back to the receiver via `ack_to`, which uses it
/// to advance the on-disk cursor only through the contiguous prefix of
/// acked positions — so out-of-order acks from a batched output do not
/// silently advance past still-in-flight events.
///
/// Memory queues have no persistent cursor, so the [`AckPosition::Memory`]
/// variant carries no data — it exists only so the handle's type does
/// not need to know which backend produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AckPosition {
    Memory,
    Disk { seq: u64, offset: u64 },
}

/// Which backend implements a given [`QueueReceiver`].
///
/// The shutdown drain in `run_queue_consumer` (and the pipeline
/// worker's mirror in `runtime.rs`) is the only site that currently
/// needs this distinction: memory queues must be closed and drained
/// to `None` so an outstanding-permit send from the pipeline side
/// still lands on the consumer, whereas disk queues must skip the
/// drain entirely so unread WAL entries survive to the next-start
/// replay path rather than being pulled into shutdown-window RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueBackendKind {
    Memory,
    Disk,
}

/// Maximum spin iterations the [`SpinController`] budget will ever
/// reach. `try_recv` + `std::hint::spin_loop` per iteration puts the
/// worst-case spin duration in the low-microsecond range; that ceiling
/// is what keeps a busy consumer from monopolising its tokio worker
/// against cooperative scheduling. This is the only [`SpinController`]
/// tuning constant a human chose from a latency argument rather than
/// from measurement, and is chosen deliberately over a per-output or
/// runtime-configurable knob.
const SPIN_CAP: u32 = 128;

/// Number of consecutive spin misses (or their pseudo-hit equivalents)
/// before the arrival-evidence held on the [`SpinController`] halves.
/// The staleness rule is what lets the evidence floor decay to zero at
/// idle — without it R2's floor would pin the budget at a stale
/// arrival mode forever, keeping the daemon spinning long after
/// arrivals had stopped. Sixteen misses is fast enough that a
/// sustained load drop stops costing spin within milliseconds and slow
/// enough that a single anomalous long gap does not erase the floor
/// that the steady-state regime depends on.
const EVIDENCE_STALE_AFTER: u32 = 16;

/// Threshold below which a park wake is treated as evidence that
/// spinning would have caught the arrival ("pseudo-hit"). Sits at
/// roughly 2–5× the worst-case spin duration, so wake events near the
/// spin-time budget are treated as growth signals. Falsely triggering
/// (a park at 9 µs that spin could not actually have covered) only
/// costs budget growth that R2's decay will walk back — the rule errs
/// toward re-engagement because the cost function is asymmetric
/// (short-park misclassification: nanoseconds of extra spin; missed
/// short-park signal: microseconds of extra park round trips).
const PSEUDO_HIT_PARK_NS: u64 = 10_000;

/// Adaptive spin-before-park controller for [`QueueReceiver::recv_many`].
///
/// Bounded queue consumers on tokio pay a full park/wake round trip
/// (scheduler wake, timer bookkeeping, context switch) every time the
/// producer's inter-arrival gap crosses the park boundary — even when
/// the next event is only nanoseconds away. This controller lets the
/// consumer spin a short adaptive budget of `try_recv` polls before
/// parking, converting the "arrival just after park" case into a
/// synchronous `try_recv` hit at a fraction of the cost.
///
/// Under a steady load the budget rises to the arrival mode and stays
/// there; under an idle load it decays to zero and the receiver
/// behaves exactly as it would without the controller (byte-identical
/// on the park path, including its scheduler footprint).
///
/// The five rules (`R1` growth on hit / `R2` evidence-floored decay
/// on miss / `R3` staleness / `R4` pseudo-hit escape from park /
/// `R5` zero-budget short-circuit) each exist to close a specific
/// failure mode the plain versions of the machine have. See the
/// per-method docstrings for which mode each rule closes; simplifying
/// any of them re-opens the mode it exists to prevent.
///
/// Total state is three `u32`s and the state machine has no `.await`,
/// no clock read (the caller feeds a park duration to
/// [`Self::record_park`]), and no allocation, so it can be composed
/// into any receive path without changing its cost profile.
#[derive(Debug, Clone, Copy)]
struct SpinController {
    /// Current spin iterations budget. `0` means "skip spin, park
    /// immediately" (R5). Grows on hit, decays with an evidence floor
    /// on miss.
    budget: u32,
    /// Arrival-mode evidence: the deepest spin iteration index at
    /// which a hit landed (subject to staleness). Serves as the floor
    /// under R2 decay so that a single anomalous long gap cannot pin
    /// the budget below the arrival point where hits are actually
    /// happening.
    evidence: u32,
    /// Spin misses (or the pseudo-hit equivalents that R4 folds into
    /// the same counter) since evidence was last refreshed. Drives R3
    /// staleness so evidence eventually decays back to zero at idle.
    evidence_age: u32,
}

impl SpinController {
    const fn new() -> Self {
        Self {
            budget: 0,
            evidence: 0,
            evidence_age: 0,
        }
    }

    /// R5: return `Some(budget)` when the caller should enter the spin
    /// phase and `None` when the caller should park immediately.
    ///
    /// Zero-budget short-circuit is what keeps the idle daemon
    /// byte-identical to the pre-controller receive path. When budget
    /// has decayed to zero (cold start, or R3 staleness from a
    /// sustained idle) the spin phase is skipped entirely — no
    /// `try_recv`, no `spin_loop`, no clock read on the spin side.
    fn should_spin(&self) -> Option<u32> {
        (self.budget > 0).then_some(self.budget)
    }

    /// R1: called when a `try_recv` inside the spin phase returned
    /// `Some`. `iterations_needed` is the 1-based count of `try_recv`
    /// calls made through and including the successful one.
    ///
    /// Growth is aggressive (`budget * 2`, saturating at `SPIN_CAP`)
    /// because the cost function is asymmetric — an oversized budget
    /// costs nanoseconds on true miss iterations (the spin exits at
    /// the hit and never walks the surplus), an undersized budget
    /// costs the full park/wake round trip in microseconds. Err on
    /// the high side, fast.
    fn record_hit(&mut self, iterations_needed: u32) {
        self.evidence = self.evidence.max(iterations_needed);
        self.evidence_age = 0;
        self.budget = self.budget.max(1).saturating_mul(2).min(SPIN_CAP);
    }

    /// R2 (evidence-floored decay on miss) composed with R3 (evidence
    /// staleness). Called after the caller has spun through the full
    /// budget without a hit — right before the caller parks.
    ///
    /// The evidence floor is what makes this controller work under
    /// steady load. Blind halving would create an absorbing zero
    /// state: under a regular arrival mode, one anomalous long gap
    /// halves the budget below the arrival point, after which hits
    /// become impossible, R1's growth signal disappears, and the
    /// budget monotonically decays to zero — every subsequent event
    /// paying full park cost, forever. R2's floor forbids decaying
    /// below the demonstrated arrival mode, so a single tail event
    /// cannot pin the controller into that trap.
    ///
    /// R3 makes the floor age out. Without staleness the floor
    /// eventually pins the budget at a mode that has stopped being
    /// the current arrival regime — including at idle, where the
    /// daemon would keep spinning `evidence` iterations per wake
    /// despite arrivals having stopped. Aging lets evidence (and so
    /// the budget floor, and so the budget itself) decay back to zero
    /// at idle. R2 without R3 breaks the idle-safety property.
    fn record_spin_miss(&mut self) {
        self.evidence_age = self.evidence_age.saturating_add(1);
        if self.evidence_age >= EVIDENCE_STALE_AFTER {
            self.evidence /= 2;
            self.evidence_age = 0;
        }
        let proposed = self.budget / 2;
        self.budget = if proposed < self.evidence {
            self.evidence.min(self.budget)
        } else {
            proposed
        };
    }

    /// R4 (pseudo-hit escape from park). Called after every park's
    /// wake with the time actually spent parked.
    ///
    /// A short park is the direct observation "spinning would have
    /// caught this event" and is the only growth signal available
    /// below the arrival mode. R1's spin-hit growth requires
    /// `budget >= arrival mode`; from a zero budget (cold start, or
    /// after idle decay through R3) hits are structurally impossible
    /// and the controller would have an absorbing low state without
    /// this rule. A short park lets the controller notice that load
    /// has returned and grow the budget back into the useful range.
    ///
    /// Long parks (past `PSEUDO_HIT_PARK_NS`) are no-ops on state so
    /// the idle-safety property from R3+R5 is preserved verbatim.
    fn record_park(&mut self, parked_ns: u64) {
        if parked_ns < PSEUDO_HIT_PARK_NS {
            self.budget = self.budget.max(1).saturating_mul(2).min(SPIN_CAP);
            self.evidence = self.evidence.max(self.budget);
            self.evidence_age = 0;
        }
    }
}

/// Handle for receiving events from a queue.
pub struct QueueReceiver {
    inner: ReceiverInner,
    name: Arc<String>,
    /// Adaptive spin-before-park controller applied inside
    /// [`Self::recv_many`]. Local to the receiver (not shared across
    /// receivers) because the arrival mode is a property of the
    /// producer feeding this specific channel — noisy neighbours on a
    /// different queue must not perturb this one's arrival estimate.
    spin_ctrl: SpinController,
}

enum ReceiverInner {
    Memory(tokio::sync::mpsc::Receiver<QueuedEvent>),
    Disk(disk::DiskQueueReceiver),
}

impl QueueReceiver {
    pub(crate) fn depth(&self) -> u64 {
        match &self.inner {
            ReceiverInner::Memory(rx) => rx.len() as u64,
            ReceiverInner::Disk(rx) => rx.depth(),
        }
    }

    /// Which backend is behind this receiver. See [`QueueBackendKind`].
    pub fn backend_kind(&self) -> QueueBackendKind {
        match &self.inner {
            ReceiverInner::Memory(_) => QueueBackendKind::Memory,
            ReceiverInner::Disk(_) => QueueBackendKind::Disk,
        }
    }

    /// Refuse further sends on the underlying channel so any
    /// outstanding-permit-holding sender wakes up with `Err`, and any
    /// event whose send had already committed a slot still becomes
    /// visible to `recv()` before the final `None`.
    ///
    /// Load-bearing for shutdown correctness on the memory backend:
    /// the previous `try_recv()` snapshot drain would race with a
    /// pipeline-side send that had reserved a permit but not yet
    /// written the value, silently losing the event once the consumer
    /// exited. Combining `close()` with `recv().await`-until-`None`
    /// consumes every value already in the channel plus every value
    /// still being written by an outstanding permit-holder, then
    /// terminates deterministically.
    ///
    /// No-op on the disk backend — unread WAL entries are handled by
    /// the shutdown drain skipping them entirely, so they survive to
    /// the next-start replay cursor.
    pub fn close(&mut self) {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.close(),
            ReceiverInner::Disk(_) => {}
        }
    }

    /// Receive the next event, paired with the position that must be
    /// fed back via `ack_to` once the event reaches a terminal
    /// disposition. The position is captured at the moment of read,
    /// not at ack time — that distinction is what makes the disk
    /// cursor correct under batched, out-of-order acks.
    pub async fn recv(&mut self) -> Option<(QueuedEvent, AckPosition)> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.recv().await.map(|e| (e, AckPosition::Memory)),
            ReceiverInner::Disk(rx) => rx.recv().await,
        }
    }

    /// Non-blocking peek at the next event. Returns `None` on empty
    /// (regardless of closure state); a subsequent `recv().await` is
    /// what observes queue closure.
    ///
    /// Safe here as the greedy-drain step of [`Self::recv_many`]
    /// because the outer consumer loop always re-enters through
    /// `recv().await` after each batch — so a permit-holding sender
    /// whose write races the `try_recv()` will still be picked up on
    /// the next iteration. This differs from the shutdown drain,
    /// which exits the loop after one snapshot and therefore MUST
    /// use `close() + recv().await`-until-`None` to avoid dropping
    /// mid-write permit-holder sends. Do not lift `try_recv()` into
    /// any exit path.
    pub fn try_recv(&mut self) -> Option<(QueuedEvent, AckPosition)> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.try_recv().ok().map(|e| (e, AckPosition::Memory)),
            ReceiverInner::Disk(rx) => rx.try_recv(),
        }
    }

    /// Return the current spin budget only when the spin phase is
    /// worth running on this receiver's backend.
    ///
    /// The `SpinController` state machine is per-receiver and applies
    /// uniformly, but the *cost model* of the spin phase's `try_recv`
    /// calls does not: memory `try_recv` is nanoseconds (one atomic
    /// mpsc peek), disk `try_recv` opens a segment file and seeks per
    /// call (multiple microseconds). The spin phase is meant to
    /// convert a park/wake round trip (a few microseconds) into a
    /// handful of quick `try_recv` calls, and only earns its keep on
    /// the memory backend where each poll is cheap. On the disk
    /// backend the same 128-iteration budget would cost more than
    /// the park it is meant to skip. Gating the phase here is the
    /// smallest change that respects both sides of the balance.
    fn spin_budget_for_backend(&self) -> Option<u32> {
        match &self.inner {
            ReceiverInner::Memory(_) => self.spin_ctrl.should_spin(),
            ReceiverInner::Disk(_) => None,
        }
    }

    /// Wait for at least one event, then greedily drain up to `max`
    /// events total from the queue into `buf`. Returns the number of
    /// events appended (0 only when the queue is closed and empty).
    ///
    /// Cancel-safe: the only `.await` is the eventual `recv()` call,
    /// which is cancel-safe by contract on both backends. Everything
    /// else is synchronous — the initial fast-path drain, the
    /// adaptive spin phase (bounded by [`SpinController`]) and every
    /// greedy `try_recv` follow-up. If a `tokio::select!` branch
    /// other than this one fires before the `recv()` returns Ready,
    /// no events have been consumed; if `recv()` has already
    /// returned an event, the whole batch runs to completion.
    ///
    /// Flow:
    /// 1. **Fast-path drain**: pop already-queued events synchronously.
    ///    On a healthy backlog regime this is the only step that runs.
    /// 2. **Adaptive spin** (`SpinController::should_spin`, gated to
    ///    the memory backend via [`Self::spin_budget_for_backend`]): if
    ///    the controller has a non-zero budget and the receiver's
    ///    backend is memory, spin `try_recv` / `spin_loop` up to that
    ///    budget. On a hit inside the spin window we skip the park
    ///    entirely and record the arrival depth via `record_hit`; on
    ///    budget exhaustion we call `record_spin_miss` right before
    ///    parking.
    /// 3. **Timed park**: `Instant::now()` before `recv().await`,
    ///    `elapsed()` after — the duration is fed to `record_park`
    ///    so the controller can grow the budget when it sees a park
    ///    that spinning could plausibly have caught.
    pub async fn recv_many(
        &mut self,
        buf: &mut Vec<(QueuedEvent, AckPosition)>,
        max: usize,
    ) -> usize {
        if max == 0 {
            return 0;
        }
        let start = buf.len();

        // (1) Fast-path drain: on backlog we take events without ever
        // touching spin or park. Cancel-safe because there is no
        // await — if we push anything we exit through the return
        // below, and if we push nothing we haven't consumed anything.
        while buf.len() - start < max {
            match self.try_recv() {
                Some(pair) => buf.push(pair),
                None => break,
            }
        }
        if buf.len() > start {
            return buf.len() - start;
        }

        // (2) Adaptive spin phase. Gated to the memory backend: on
        // the disk backend every `try_recv` performs a fresh segment
        // file open + seek (multi-microseconds), so a spin budget of
        // up to `SPIN_CAP = 128` iterations against an empty
        // disk-backed queue would cost hundreds of microseconds per
        // idle poll — comparable to or worse than the single park
        // round trip the spin phase is meant to save. The controller
        // still records park durations on the disk backend below
        // (the state is cheap and preserves R4's pseudo-hit escape
        // shape) but the spin phase itself is a memory-only path.
        // When the budget is 0 (cold start / decayed at idle) or
        // the backend is disk, `spin_budget_for_backend` returns
        // `None` and we go straight to park — that is the
        // "idle byte-identical" path.
        if let Some(budget) = self.spin_budget_for_backend() {
            let mut iterations: u32 = 0;
            while iterations < budget {
                iterations = iterations.saturating_add(1);
                if let Some(pair) = self.try_recv() {
                    self.spin_ctrl.record_hit(iterations);
                    buf.push(pair);
                    while buf.len() - start < max {
                        match self.try_recv() {
                            Some(p) => buf.push(p),
                            None => break,
                        }
                    }
                    return buf.len() - start;
                }
                std::hint::spin_loop();
            }
            self.spin_ctrl.record_spin_miss();
        }

        // (3) Park path. Time the wait so a short park (indistinguishable
        // from a spin hit that we just barely missed) can lift the
        // controller out of the absorbing zero-budget state via R4.
        // Clock reads only appear here — never on the spin side.
        let park_start = std::time::Instant::now();
        let Some(first) = self.recv().await else {
            return 0;
        };
        let parked_ns: u64 = park_start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.spin_ctrl.record_park(parked_ns);
        buf.push(first);
        while buf.len() - start < max {
            match self.try_recv() {
                Some(pair) => buf.push(pair),
                None => break,
            }
        }
        buf.len() - start
    }

    /// Commit a specific event's position as processed. For the disk
    /// backend this records the ack against the in-flight position
    /// queue and, when the position is the front of the queue (or
    /// completes a contiguous acked prefix from the front), advances
    /// the persisted cursor through that prefix and reclaims
    /// fully-consumed segments. For the memory backend this is a
    /// no-op (mpsc removes events on `recv()`; there is no separate
    /// persistent cursor to advance).
    ///
    /// The consumer is expected to call `ack_to(position)` after
    /// every event's final disposition is decided — delivered, routed
    /// to recovery, or dropped. Calling out of order is safe: the
    /// disk receiver only persists through positions that are
    /// contiguously acked from the front, so a late-arriving early
    /// ack will still hold the cursor back correctly.
    pub fn ack_to(&mut self, position: AckPosition) {
        match (&mut self.inner, position) {
            (ReceiverInner::Memory(_), AckPosition::Memory) => {}
            (ReceiverInner::Disk(rx), AckPosition::Disk { seq, offset }) => {
                rx.ack_to(seq, offset);
            }
            (ReceiverInner::Memory(_), AckPosition::Disk { .. }) => {
                debug_assert!(
                    false,
                    "ack_to: Disk position fed to memory receiver — backend mismatch",
                );
                warn!("queue: ack_to received Disk position on a memory receiver (ignored)");
            }
            (ReceiverInner::Disk(_), AckPosition::Memory) => {
                debug_assert!(
                    false,
                    "ack_to: Memory position fed to disk receiver — backend mismatch",
                );
                warn!("queue: ack_to received Memory position on a disk receiver (ignored)");
            }
        }
    }
}

/// Create a sender/receiver pair.
pub fn create_queue(
    name: String,
    config: QueueConfig,
) -> anyhow::Result<(QueueSender, QueueReceiver)> {
    let name = Arc::new(name);

    match config.queue_type {
        QueueType::Memory => {
            let (tx, rx) = tokio::sync::mpsc::channel(config.capacity);
            Ok((
                QueueSender {
                    inner: SenderInner::Memory(tx),
                    name: Arc::clone(&name),
                    metrics: None,
                },
                QueueReceiver {
                    inner: ReceiverInner::Memory(rx),
                    name: Arc::clone(&name),
                    spin_ctrl: SpinController::new(),
                },
            ))
        }
        QueueType::Disk { ref path, max_size } => {
            let (tx, rx) = disk::create_disk_queue(path, max_size)?;
            Ok((
                QueueSender {
                    inner: SenderInner::Disk(tx),
                    name: Arc::clone(&name),
                    metrics: None,
                },
                QueueReceiver {
                    inner: ReceiverInner::Disk(rx),
                    name: Arc::clone(&name),
                    spin_ctrl: SpinController::new(),
                },
            ))
        }
    }
}

/// Retry configuration for output writes.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_wait: std::time::Duration,
    pub max_wait: std::time::Duration,
    pub backoff: BackoffStrategy,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_wait: std::time::Duration::from_secs(1),
            max_wait: std::time::Duration::from_secs(60),
            backoff: BackoffStrategy::Exponential,
        }
    }
}

impl RetryConfig {
    /// Compute the next sleep duration given the current one, applying
    /// the configured backoff. Shared by every output's internal retry
    /// loop so the doubling-then-clamp policy is defined once.
    pub fn next_wait(&self, current: std::time::Duration) -> std::time::Duration {
        match self.backoff {
            BackoffStrategy::Exponential => current.saturating_mul(2).min(self.max_wait),
            BackoffStrategy::Fixed => current,
        }
    }

    /// Parse from an output definition's properties (retry block).
    pub fn from_output_properties(output_props: &[Property]) -> anyhow::Result<Self> {
        let mut config = Self::default();

        if let Some(block) = props::get_block(output_props, "retry") {
            if let Some(n) = props::get_positive_int(block, "max_attempts")? {
                config.max_attempts = n.min(u32::MAX as u64) as u32;
            }
            if let Some(s) = props::get_string(block, "initial_wait") {
                config.initial_wait = props::parse_duration(&s)?;
            }
            if let Some(s) = props::get_string(block, "max_wait") {
                config.max_wait = props::parse_duration(&s)?;
            }
            match props::get_ident(block, "backoff").as_deref() {
                Some("fixed") => config.backoff = BackoffStrategy::Fixed,
                _ => config.backoff = BackoffStrategy::Exponential,
            }
        }

        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffStrategy {
    Exponential,
    Fixed,
}

// ---------------------------------------------------------------------------
// Ack lifecycle
// ---------------------------------------------------------------------------
//
// Previously the queue consumer advanced the disk-queue cursor (`ack`)
// immediately after `Output::consume` returned `Ok` — which for batched
// outputs only means "event accepted into the in-memory buffer", not
// "event shipped". A daemon crash between accept and flush silently
// lost every event in the batch (the cursor said "delivered", the
// buffer was gone).
//
// The current flow gives each event a `QueueAckHandle`. Outputs hold
// the handle until the event's final disposition is known and call
// `resolve_delivered()` or `resolve_recovered()` (= "routed to DLQ").
// The handle's destruction sends one [`AckDisposition`] back to the
// consumer, which then advances the queue cursor. A handle that drops
// without an explicit resolve sends `Dropped` and (in debug builds)
// fires a `debug_assert!` — that path is reserved for bugs / panics /
// shutdown fall-through, never the steady-state contract.

/// Final disposition of an event as far as the queue is concerned.
/// Reported back from the output via [`QueueAckHandle`] so the consumer
/// can advance the cursor and update per-disposition metrics breakdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDisposition {
    /// Event was durably shipped to its destination.
    Delivered,
    /// Event was routed to the DLQ (`error_log`).
    Recovered,
    /// Event was dropped without explicit disposition (bug / panic /
    /// shutdown fallthrough). Should not occur on healthy paths.
    Dropped,
}

/// Handle handed to an [`crate::modules::Output`] alongside each event.
/// The output is responsible for calling `resolve_delivered` or
/// `resolve_recovered` once the event's final disposition is decided —
/// including across batched flushes that may resolve a handle long
/// after `consume` returned `Ok`. The handle's `Drop` impl falls back
/// to [`AckDisposition::Dropped`] for the unhealthy paths, and
/// `debug_assert!`s the contract was met.
pub struct QueueAckHandle {
    tx: Option<tokio::sync::mpsc::UnboundedSender<(AckPosition, AckDisposition)>>,
    /// Position the handle's event occupied in the source queue,
    /// captured at `recv()` time. Pre-fix the disk receiver advanced
    /// its cursor to `self.read_*` at ack time, which under batched
    /// outputs meant "all in-flight events", not "this event" — a
    /// single ack from anywhere in the batch could advance the cursor
    /// past still-in-flight events and silently lose them on crash.
    /// Carrying the position on the handle decouples cursor commit
    /// from in-flight order.
    position: AckPosition,
    /// True once an explicit `resolve_*` ran. Read in `Drop` to
    /// distinguish "resolved cleanly" (silence the debug_assert) from
    /// "dropped without resolve" (fire the assert + send `Dropped`).
    resolved: bool,
    emitted_ns: crate::time::UnixNanos,
    metrics: Arc<crate::metrics::OutputMetrics>,
}

impl QueueAckHandle {
    pub fn new(
        tx: tokio::sync::mpsc::UnboundedSender<(AckPosition, AckDisposition)>,
        position: AckPosition,
        emitted_ns: crate::time::UnixNanos,
        metrics: Arc<crate::metrics::OutputMetrics>,
    ) -> Self {
        Self {
            tx: Some(tx),
            position,
            resolved: false,
            emitted_ns,
            metrics,
        }
    }

    /// The position this handle's event occupies in the source queue.
    /// Test-only accessor — no production caller needs it today (the
    /// queue consumer reads `position` off the resolved
    /// `(AckPosition, AckDisposition)` tuple instead). Now exposed
    /// crate-wide for the DLQ-outcome dispatcher —
    /// `resolve_ack_from_dlq_outcome` (in `crates/limpid/src/modules/mod.rs`)
    /// consults the queue kind to decide between `resolve_recovered`
    /// (memory) and `resolve_dropped` (disk wedge) on a Dropped
    /// route outcome.
    pub fn position(&self) -> AckPosition {
        self.position
    }

    /// Signal that the event was durably delivered. Consumes the handle.
    pub fn resolve_delivered(self) {
        self.resolve_delivered_at(crate::time::UnixNanos::now());
    }

    /// Signal delivery using a caller-sampled wall-clock boundary.
    pub(crate) fn resolve_delivered_at(mut self, delivered_at: crate::time::UnixNanos) {
        self.metrics.observe_delivery(self.emitted_ns, delivered_at);
        self.resolved = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Delivered));
        }
    }

    /// Signal that the event was routed to the DLQ (retry exhausted,
    /// render error, shutdown leftover). Consumes the handle.
    pub fn resolve_recovered(mut self) {
        self.resolved = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Recovered));
        }
    }

    /// Signal an **intentional** `Dropped` disposition: the caller
    /// has decided the event cannot be delivered *and* cannot be
    /// routed to the DLQ (e.g. DLQ-write failed with the outer
    /// route_event_to_dlq contract). On a disk queue this holds
    /// the cursor at the event's position so a subsequent daemon
    /// start replays it; on a memory queue it is functionally
    /// identical to the implicit drop (memory queues have no
    /// cursor to hold and cannot replay).
    ///
    /// Distinct from the `Drop` impl's `Dropped` send: `Drop`
    /// firing signals a **bug** (the output failed to resolve
    /// explicitly), guarded by `debug_assert!(self.resolved,
    /// ...)`. This method is the honest way to arrive at the same
    /// disposition without tripping the assertion, and the two
    /// paths are indistinguishable to the queue consumer — the
    /// wedge contract applies to both because the underlying
    /// disposition is `Dropped` either way.
    ///
    /// Currently only exercised by test-side mocks that need to
    /// signal `Dropped` without tripping the debug_assert;
    /// in-daemon callers (DLQ-write-failure path) reach it
    /// through the DLQ-outcome dispatcher.
    #[allow(dead_code)]
    pub fn resolve_dropped(mut self) {
        self.resolved = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Dropped));
        }
    }

    /// Test-only constructor returning the handle and the receiving
    /// half of its ack channel, so tests can assert on the disposition.
    #[cfg(test)]
    pub fn for_test() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(AckPosition, AckDisposition)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self::new(
                tx,
                AckPosition::Memory,
                crate::time::UnixNanos::now(),
                crate::metrics::OutputMetrics::for_testing(),
            ),
            rx,
        )
    }

    /// Test-only constructor that also sets the carried position.
    #[cfg(test)]
    pub fn for_test_with_position(
        position: AckPosition,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(AckPosition, AckDisposition)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self::new(
                tx,
                position,
                crate::time::UnixNanos::now(),
                crate::metrics::OutputMetrics::for_testing(),
            ),
            rx,
        )
    }
}

impl Drop for QueueAckHandle {
    fn drop(&mut self) {
        debug_assert!(
            self.resolved,
            "QueueAckHandle dropped without explicit resolve_delivered / resolve_recovered \
             — bug: outputs MUST explicitly resolve their disposition"
        );
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Dropped));
        }
    }
}

/// Maximum number of events drained per [`QueueReceiver::recv_many`]
/// call inside the consumer loop. Bounds the number of events
/// processed between two select-loop re-entries, which sets the
/// worst-case latency for observing shutdown and ack arrivals — at
/// this size (64 × sub-microsecond per-event `consume` calls in
/// steady state) the blackout stays sub-millisecond, well inside the
/// shutdown budget. Amortization of per-wake scheduler cost is a
/// 1/k curve, so raising this further has diminishing returns while
/// the blackout grows linearly; 64 sits past the knee of the curve.
const RECV_BATCH_MAX: usize = 64;

/// Run a queue consumer that drains events and writes them to an output.
///
/// The consumer does not own retry / DLQ logic — each `Output` runs
/// its own per-event retry loop and resolves a [`QueueAckHandle`]
/// when the event's final disposition is decided. The consumer's job
/// is purely "hand each event + handle to the output, then advance
/// the queue cursor when its disposition comes back". The cursor
/// only advances after the output's handle resolves, which for
/// batched outputs happens at flush time, not at buffer-accept time.
#[allow(clippy::too_many_arguments)]
pub async fn run_queue_consumer(
    mut receiver: QueueReceiver,
    writer: Arc<dyn crate::modules::Output>,
    tap: Option<crate::tap::TapRegistry>,
    metrics: Arc<crate::metrics::OutputMetrics>,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let name = Arc::clone(&receiver.name);
    info!("output '{}': queue consumer started", name);
    let (ack_tx, mut ack_rx) =
        tokio::sync::mpsc::unbounded_channel::<(AckPosition, AckDisposition)>();
    let mut in_flight: usize = 0;
    let mut accepting = true;
    // `wedged` distinguishes fail-stop wedge from natural queue
    // closure. Both drive `accepting=false`, but they differ in
    // exit reason (logged), whether shutdown drain runs (skipped
    // when wedged — feeding more events into a bug-path output
    // just piles up doomed handles), and whether the exit message
    // is `info` (natural) or `error` (wedge — operator action
    // required). Once wedged, in-flight is drained but new
    // events stay in the queue for the next run's replay.
    let mut wedged = false;
    // Reused batch buffer for `recv_many`. Allocated once with the
    // batch cap so the steady-state hot path never reallocates. Owned
    // by the loop (not the arm) so its lifetime is independent of any
    // single `select!` future's cancellation — since our `recv_many`
    // is cancel-safe up to its first pushed event, and the drain that
    // follows the first push is synchronous, the only state that can
    // survive across select re-entries is an empty buffer.
    let mut batch: Vec<(QueuedEvent, AckPosition)> = Vec::with_capacity(RECV_BATCH_MAX);
    metrics.queue_depth.set(receiver.depth());

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed(), if accepting || wedged => {
                if *shutdown.borrow() {
                    // Wedged path: skip the drain and break immediately.
                    // The consumer is not accepting new events (that is
                    // the wedge contract), and any handles still parked
                    // inside a batched sink's buffer at this point can
                    // only be resolved by the post-loop
                    // `writer.shutdown_wedged()` — waiting for
                    // `in_flight == 0` here would deadlock exactly the
                    // case the wedge-aware resolve was added to fix.
                    if wedged {
                        info!(
                            "output '{}': shutting down while wedged; parked buffers resolve via shutdown_wedged",
                            name
                        );
                        break;
                    }
                    info!("output '{}': shutting down, draining queue", name);
                    // Backend-aware drain.
                    //
                    // Memory backend: close the receiver first, then
                    // consume with `recv().await` until `None`. Closing
                    // makes any outstanding-permit sender wake with
                    // `Err`, while values whose send had already
                    // committed a slot still become visible before
                    // `None`. The previous `try_recv()` snapshot loop
                    // raced with permit-holding sends and could silently
                    // exit before a mid-flight `send` completed its
                    // write — dropping that event even though the
                    // sender saw `Ok`. Bounded because the mpsc capacity
                    // caps how much can be in flight when we start.
                    //
                    // Disk backend: skip the drain entirely. The WAL
                    // owns unread durable state; pulling it into
                    // `consume_shutdown` here would pin the whole
                    // unread backlog into shutdown-window RAM (and
                    // stretch the flush deadline over WAL read time).
                    // Unread entries stay on disk and replay on the
                    // next start; only handles already owned by the
                    // output resolve via the post-loop `writer.shutdown()`
                    // and the ack drain that follows.
                    match receiver.backend_kind() {
                        QueueBackendKind::Memory => {
                            receiver.close();
                            while let Some((event, position)) = receiver.recv().await {
                                metrics.queue_depth.set(receiver.depth());
                                if let Some(tap) = &tap {
                                    tap.emit(&format!("output {}", name), &event).await;
                                }
                                let handle = QueueAckHandle::new(
                                    ack_tx.clone(),
                                    position,
                                    event.emitted_ns(),
                                    Arc::clone(&metrics),
                                );
                                in_flight += 1;
                                // `consume_shutdown` (not `consume`) — the
                                // shutdown contract forbids the steady-state
                                // retry path. Unbatched outputs ship once
                                // bounded then DLQ; batched outputs buffer
                                // only and let the post-loop `writer.shutdown()`
                                // drain bounded. See `Output::consume_shutdown`.
                                if let Err(e) = writer.consume_shutdown(&event, handle).await {
                                    // Bug path: `consume_shutdown` returned
                                    // Err without taking ownership of the
                                    // handle. The handle's Drop impl fires
                                    // `Dropped` through the ack channel, and
                                    // `handle_ack_disposition(Dropped)` is
                                    // the single site that bumps
                                    // `events_failed` for bug-path drops —
                                    // do NOT bump here as well.
                                    tracing::error!(
                                        "output '{}': consume_shutdown returned Err during drain: {} \
                                         (bug — disposition signalled via handle)",
                                        name,
                                        e
                                    );
                                }
                            }
                        }
                        QueueBackendKind::Disk => {
                            info!(
                                "output '{}': disk backend — leaving unread backlog for next-start replay",
                                name
                            );
                        }
                    }
                    break;
                }
            }

            Some((position, disposition)) = ack_rx.recv() => {
                handle_ack_disposition(disposition, &name, &metrics);
                // Dropped disposition on a *disk* queue is the
                // fail-stop wedge. `receiver.ack_to` would advance
                // the cursor past a position we cannot honestly
                // confirm — silently losing the event on a
                // durable queue. On the first Dropped-on-disk we
                // record the wedge transition (helper: log +
                // `events_wedged++`, guarded so subsequent Dropped
                // dispositions after the wedge do not re-emit),
                // stop accepting new events (further `consume`
                // calls on a bug-path output would just
                // accumulate more doomed handles behind the
                // wedged front), and skip `ack_to` so the
                // disk-side in-flight bookkeeping keeps the
                // wedged position at the front, blocking the
                // cursor from advancing past it until replay.
                // Subsequent Dropped dispositions after the wedge
                // are drained through the normal `ack_to` path
                // for their positions — the wedged front already
                // holds the cursor.
                // Operator intervention (fix the bug / restart
                // the daemon so the disk queue replays from the
                // wedge point) is the recovery contract. Memory
                // queues cannot replay on restart, so wedging
                // would only cause loss without a recovery path
                // — they keep the continue-and-count behavior.
                let was_wedged = wedged;
                record_wedge_transition_if_first(
                    position,
                    disposition,
                    &mut wedged,
                    &name,
                    &metrics,
                );
                if !was_wedged && wedged {
                    accepting = false;
                    // Skip ack_to on this position — the wedge
                    // was just recorded and its position must
                    // stay at the in-flight front.
                } else {
                    receiver.ack_to(position);
                    metrics.queue_depth.set(receiver.depth());
                }
                in_flight = in_flight.saturating_sub(1);
                // Natural queue-closure or wedge exit: the
                // consumer stopped accepting (queue drained or
                // fail-stop wedged) and the last in-flight
                // handle just resolved. Shutdown does NOT exit
                // here — it breaks straight out of the select
                // arm above so the post-loop `writer.shutdown()`
                // can drain batched buffers (which is the only
                // thing that can resolve their parked handles).
                if !accepting && in_flight == 0 {
                    break;
                }
            }

            n = receiver.recv_many(&mut batch, RECV_BATCH_MAX), if accepting => {
                metrics.queue_depth.set(receiver.depth());
                if n == 0 {
                    // `recv_many` returned 0 ⇔ `recv().await` observed
                    // `None` ⇔ queue closed and empty. Same semantics as
                    // the pre-batch single-event `input = None` arm.
                    info!("output '{}': queue closed", name);
                    accepting = false;
                    if in_flight == 0 {
                        break;
                    }
                } else {
                    for (event, position) in batch.drain(..) {
                        if let Some(tap) = &tap {
                            tap.emit(&format!("output {}", name), &event).await;
                        }
                        let handle = QueueAckHandle::new(
                            ack_tx.clone(),
                            position,
                            event.emitted_ns(),
                            Arc::clone(&metrics),
                        );
                        in_flight += 1;
                        if let Err(e) = writer.consume(&event, handle).await {
                            // Reaching here means the output returned an
                            // error from `consume` itself — by the
                            // ack-handle contract that signals a bug
                            // (the output failed to take ownership of
                            // the lifecycle). The handle's Drop impl
                            // fires `Dropped` via the channel, and
                            // `handle_ack_disposition(Dropped)` is the
                            // single site that bumps `events_failed`
                            // for bug-path drops — do NOT bump here
                            // as well (double count).
                            tracing::error!(
                                "output '{}': consume returned Err: {} \
                                 (bug — disposition signalled via handle)",
                                name,
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    // Shutdown phase. Ordering matters: call `writer.shutdown()` FIRST
    // so batched outputs drain their internal `(Event, QueueAckHandle)`
    // buffers (final flush + per-event DLQ routing as needed),
    // resolving every still-held handle. Only then drain the ack
    // channel for cursor advancement. The reversed ordering (wait for
    // in_flight == 0 before calling shutdown) deadlocks: the buffered
    // handles are exactly what `writer.shutdown()` resolves, so the
    // wait can never make progress on its own.
    //
    // Wedge-exit takes the separate `shutdown_wedged()` path
    // instead of `shutdown()`: the wedge contract says "no new
    // work through a bug-path output", so this variant resolves
    // internally parked handles without entering any transport
    // send. Buffered events go through
    // `route_shutdown_batch_ambiguous_to_dlq` — on a disk queue
    // the wedged cursor stays put for replay, on a memory queue
    // the ambiguous helper folds to `Recovered` so the ack drain
    // below does not hang on messages that will never arrive.
    // Unbatched sinks hold no buffer, so their default no-op
    // impl is correct — every handle they took has already been
    // resolved on the steady-state path by the time the wedge
    // signal reaches this arm.
    if wedged {
        if let Err(e) = writer.shutdown_wedged(error_log.as_ref()).await {
            warn!("output '{}': wedge shutdown resolve failed: {}", name, e);
        }
    } else if let Err(e) = writer.shutdown(error_log.as_ref()).await {
        warn!("output '{}': shutdown flush failed: {}", name, e);
    }
    drop(ack_tx);
    while let Some((position, disposition)) = ack_rx.recv().await {
        handle_ack_disposition(disposition, &name, &metrics);
        // Wedge held the cursor when it fired in the steady-state
        // arm above; subsequent ack drain still advances via
        // `ack_to` on delivered / recovered positions but keeps
        // holding on Dropped-on-disk. If the FIRST Dropped-on-disk
        // arrives here (never happened in steady-state — e.g. the
        // wedge originates from the post-loop `writer.shutdown()`
        // drain), the helper records the wedge transition once so
        // the operator alarm (`events_wedged` + wedge log line)
        // still fires. `accepting` is not touched — the loop above
        // has already exited its accept phase.
        record_wedge_transition_if_first(position, disposition, &mut wedged, &name, &metrics);
        let is_disk_position = matches!(position, AckPosition::Disk { .. });
        let is_dropped_on_disk = matches!(disposition, AckDisposition::Dropped) && is_disk_position;
        if !is_dropped_on_disk {
            receiver.ack_to(position);
            metrics.queue_depth.set(receiver.depth());
        }
        in_flight = in_flight.saturating_sub(1);
    }
    if in_flight != 0 {
        tracing::error!(
            "output '{}': consumer exiting with {} unresolved handle(s) — bug",
            name,
            in_flight,
        );
    }
    metrics.queue_depth.set(0);

    if wedged {
        tracing::error!(
            "output '{}': queue consumer stopped after wedge — cursor held at the Dropped \
             position for the next daemon start's replay",
            name
        );
    } else {
        info!("output '{}': queue consumer stopped", name);
    }
}

fn handle_ack_disposition(
    disposition: AckDisposition,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
) {
    match disposition {
        AckDisposition::Delivered => {
            // The output bumped `events_written` itself on the success
            // path; nothing to do here. Kept explicit so the
            // per-disposition metrics breakdown has an obvious hook
            // when 0.8 lands.
        }
        AckDisposition::Recovered => {
            // `events_failed` for the Recovered path is bumped by
            // `resolve_ack_from_dlq_outcome` when it commits the
            // recovery disposition. Nothing to do here — kept
            // explicit so the per-disposition metrics breakdown has
            // an obvious hook when 0.8 lands.
        }
        AckDisposition::Dropped => {
            // Dropped reaches this arm from two shapes:
            //
            // - **Bug path**: an output's `consume` returned
            //   without an explicit `resolve_*` call, and the
            //   handle's `Drop` fired `Dropped` through the
            //   ack channel (guarded in debug builds by the
            //   `debug_assert!(self.resolved, ...)` in
            //   `QueueAckHandle::drop`).
            // - **Intentional path**: `resolve_dropped()` was
            //   called explicitly — currently the only in-daemon
            //   caller is `resolve_ack_from_dlq_outcome` on a
            //   disk-backed queue whose DLQ-write failure was
            //   just observed, so the fail-stop wedge holds the
            //   cursor for replay.
            //
            // Both shapes count exactly once here: this is the
            // single site that bumps `events_failed` for a
            // Dropped disposition. Callers that observe a bug
            // (`consume` / `consume_shutdown` returning `Err`)
            // must NOT bump `events_failed` themselves — the
            // handle's Drop will route through here and the
            // aggregate would double-count otherwise.
            // Callers that produce an intentional Dropped via
            // `resolve_ack_from_dlq_outcome` on a disk queue also
            // must NOT bump — the helper delegates the count to
            // this arm on purpose. Operators reading this should
            // cross-reference `events_errored_unwritable` (bumped
            // on the DLQ-write failure path) and the daemon's
            // `panicked at` lines (bug path).
            tracing::error!(
                "output '{}': event dropped — no explicit disposition (bug) or intentional \
                 Dropped from a DLQ-write failure (check events_errored_unwritable and daemon \
                 panic logs)",
                name
            );
            metrics.events_failed.inc();
        }
    }
}

/// Record the fail-stop wedge transition on the first Dropped
/// disposition observed on a disk-backed position. Idempotent: the
/// `wedged` flag guards against re-recording on subsequent Dropped
/// dispositions after the wedge already fired. Non-disk positions and
/// non-Dropped dispositions are no-ops.
///
/// The wedge log line and the `events_wedged` counter bump are the
/// operator-facing signal that a disk queue has stopped accepting
/// new events and will replay from the wedged cursor on next
/// daemon start.
///
/// Callers are responsible for the cursor decision (whether to skip
/// `receiver.ack_to`) and, in the steady-state select arm, for
/// stopping acceptance (`accepting = false`). Those decisions are
/// outside the wedge-recording contract because their reachability
/// differs per call site — the post-loop ack drain has no
/// `accepting` state to mutate and always holds the cursor on
/// Dropped-on-disk.
fn record_wedge_transition_if_first(
    position: AckPosition,
    disposition: AckDisposition,
    wedged: &mut bool,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
) {
    let is_disk_position = matches!(position, AckPosition::Disk { .. });
    let is_dropped_on_disk = matches!(disposition, AckDisposition::Dropped) && is_disk_position;
    if is_dropped_on_disk && !*wedged {
        *wedged = true;
        metrics.events_wedged.inc();
        tracing::error!(
            "output '{}': disk queue wedged after AckDisposition::Dropped at position {:?} — \
             the consumer will drain in-flight events and stop accepting new ones. Fix the \
             underlying bug / DLQ-write failure and restart the daemon so the disk queue \
             replays from the wedge point. See docs/src/operations/error-log.md for the \
             manual intervention runbook.",
            name,
            position,
        );
    }
}

#[cfg(test)]
mod wedge_transition_helper_tests {
    use super::*;
    use crate::metrics::OutputMetrics;
    use std::sync::atomic::Ordering;

    fn disk_pos(seq: u64) -> AckPosition {
        AckPosition::Disk { seq, offset: 0 }
    }

    #[test]
    fn first_dropped_on_disk_sets_wedge_and_bumps() {
        let mut wedged = false;
        let metrics = crate::metrics::OutputMetrics::for_testing();
        record_wedge_transition_if_first(
            disk_pos(1),
            AckDisposition::Dropped,
            &mut wedged,
            "test",
            &metrics,
        );
        assert!(wedged);
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn subsequent_dropped_on_disk_is_idempotent() {
        let mut wedged = true;
        let metrics = OutputMetrics::for_testing();
        record_wedge_transition_if_first(
            disk_pos(2),
            AckDisposition::Dropped,
            &mut wedged,
            "test",
            &metrics,
        );
        assert!(wedged);
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dropped_on_memory_is_noop() {
        let mut wedged = false;
        let metrics = OutputMetrics::for_testing();
        record_wedge_transition_if_first(
            AckPosition::Memory,
            AckDisposition::Dropped,
            &mut wedged,
            "test",
            &metrics,
        );
        assert!(!wedged);
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn recovered_on_disk_is_noop() {
        let mut wedged = false;
        let metrics = OutputMetrics::for_testing();
        record_wedge_transition_if_first(
            disk_pos(3),
            AckDisposition::Recovered,
            &mut wedged,
            "test",
            &metrics,
        );
        assert!(!wedged);
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn delivered_on_disk_is_noop() {
        let mut wedged = false;
        let metrics = OutputMetrics::for_testing();
        record_wedge_transition_if_first(
            disk_pos(4),
            AckDisposition::Delivered,
            &mut wedged,
            "test",
            &metrics,
        );
        assert!(!wedged);
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod consumer_lifecycle_tests {
    use super::*;
    use crate::modules::{HasMetrics, Output};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    fn owned_event() -> QueuedEvent {
        QueuedEvent::new(
            Event::new(
                Bytes::from_static(b"x"),
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ),
            crate::time::UnixNanos::now(),
        )
    }

    /// Programmable mock: each call to `consume` pops the next outcome
    /// from `script` and resolves the handle accordingly. The script
    /// vocabulary mirrors the per-event ack-lifecycle a real output
    /// reaches: `Delivered` resolves the handle as delivered, and
    /// `Bug` returns Err after explicitly resolving as `Dropped`
    /// (the honest way to signal a bug path; the alternative of
    /// dropping the handle without any resolve is guarded by
    /// `debug_assert!(self.resolved, ...)` and would panic in
    /// test builds).
    #[derive(Clone, Copy)]
    enum Outcome {
        Delivered,
        Bug,
    }

    struct ScriptedWriter {
        script: Mutex<Vec<Outcome>>,
        calls: std::sync::atomic::AtomicUsize,
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    impl ScriptedWriter {
        fn new(script: Vec<Outcome>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: std::sync::atomic::AtomicUsize::new(0),
                metrics: crate::metrics::OutputMetrics::for_testing(),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl HasMetrics for ScriptedWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for ScriptedWriter {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let next = {
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    Outcome::Delivered
                } else {
                    s.remove(0)
                }
            };
            match next {
                Outcome::Delivered => {
                    ack.resolve_delivered();
                    Ok(())
                }
                Outcome::Bug => {
                    // Honest Dropped: `resolve_dropped()` marks the
                    // handle resolved (satisfies the debug_assert
                    // in `Drop`) and sends `Dropped` on the ack
                    // channel — same disposition a real output's
                    // bug path (panic / DLQ-write failure) would
                    // deliver, but without tripping the assertion
                    // on the way. Returning Err signals the
                    // consumer that the caller considered this a
                    // failure path.
                    ack.resolve_dropped();
                    Err(anyhow::anyhow!("scripted bug"))
                }
            }
        }

        async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.consume(event, ack).await
        }
    }

    struct StalledWriter {
        metrics: Arc<crate::metrics::OutputMetrics>,
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Notify,
    }

    impl HasMetrics for StalledWriter {
        type Stats = crate::metrics::OutputMetrics;

        fn metrics(&self) -> Arc<Self::Stats> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for StalledWriter {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            let entered = self.entered.lock().unwrap().take();
            if let Some(entered) = entered {
                let _ = entered.send(());
                self.release.notified().await;
            }
            ack.resolve_delivered();
            Ok(())
        }

        async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.consume(event, ack).await
        }
    }

    // ---- queue boundary error tests ----

    #[tokio::test]
    async fn queue_sender_send_returns_channel_closed_when_receiver_dropped() {
        let (tx, rx) = create_queue(
            "mem".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        drop(rx);
        let err = tx
            .send(owned_event())
            .await
            .expect_err("send to closed memory channel must fail");
        assert!(
            matches!(err, QueueSendError::ChannelClosed),
            "expected ChannelClosed, got {:?}",
            err
        );
    }

    #[tokio::test]
    async fn memory_queue_depth_is_the_current_receiver_length() {
        let (sender, mut receiver) = create_queue(
            "depth".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();

        assert_eq!(receiver.depth(), 0);
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        assert_eq!(receiver.depth(), 2);
        receiver.recv().await.expect("first queued event");
        assert_eq!(receiver.depth(), 1);
        receiver.recv().await.expect("second queued event");
        assert_eq!(receiver.depth(), 0);
    }

    #[tokio::test]
    async fn consumer_publishes_memory_backlog_and_clears_depth_on_close() {
        let event_count = RECV_BATCH_MAX + 6;
        let (sender, receiver) = create_queue(
            "depth-consumer".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: event_count + 4,
            },
        )
        .unwrap();
        for _ in 0..event_count {
            sender.send(owned_event()).await.unwrap();
        }

        let metrics = crate::metrics::OutputMetrics::for_testing();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let writer = Arc::new(StalledWriter {
            metrics: Arc::clone(&metrics),
            entered: Mutex::new(Some(entered_tx)),
            release: tokio::sync::Notify::new(),
        });
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_queue_consumer(
            receiver,
            writer.clone(),
            None,
            Arc::clone(&metrics),
            None,
            shutdown_rx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), entered_rx)
            .await
            .expect("consumer must reach stalled writer")
            .expect("stalled writer readiness sender");
        assert_eq!(metrics.queue_depth.load(Ordering::Relaxed), 6);

        writer.release.notify_one();
        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("consumer must drain and close")
            .expect("consumer task must not panic");
        assert_eq!(metrics.queue_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_consumer_publishes_receiver_depth_through_the_output_handle() {
        let source = include_str!("mod.rs");
        let start = source
            .find("pub async fn run_queue_consumer(")
            .expect("queue consumer must exist");
        let body = &source[start..];
        assert!(
            body.contains("metrics.queue_depth.set") && body.contains("receiver.depth()"),
            "queue consumer must publish backend depth through its pre-resolved gauge"
        );
    }

    // ---- QueueAckHandle unit tests ----

    #[tokio::test]
    async fn ack_handle_resolve_delivered_sends_delivered() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        handle.resolve_delivered();
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Delivered))
        );
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn ack_handle_resolve_recovered_sends_recovered() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        handle.resolve_recovered();
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Recovered))
        );
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn delivery_latency_is_observed_only_for_delivered_dispositions() {
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let emitted_at = crate::time::UnixNanos::new(1_000_000_000);

        let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
        QueueAckHandle::new(
            delivered_tx,
            AckPosition::Memory,
            emitted_at,
            Arc::clone(&metrics),
        )
        .resolve_delivered_at(crate::time::UnixNanos::new(3_000_000_000));
        assert_eq!(
            delivered_rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Delivered))
        );
        assert_eq!(metrics.delivery_seconds.count(), 1);
        assert!(metrics.delivery_seconds.sum() >= 2.0);

        let (recovered_tx, mut recovered_rx) = tokio::sync::mpsc::unbounded_channel();
        QueueAckHandle::new(
            recovered_tx,
            AckPosition::Memory,
            emitted_at,
            Arc::clone(&metrics),
        )
        .resolve_recovered();
        assert_eq!(
            recovered_rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Recovered))
        );
        assert_eq!(metrics.delivery_seconds.count(), 1);

        let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        QueueAckHandle::new(
            dropped_tx,
            AckPosition::Memory,
            emitted_at,
            Arc::clone(&metrics),
        )
        .resolve_dropped();
        assert_eq!(
            dropped_rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Dropped))
        );
        assert_eq!(metrics.delivery_seconds.count(), 1);
    }

    #[tokio::test]
    async fn accepted_batch_can_resolve_multiple_acks_at_one_delivery_boundary() {
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let delivered_at = crate::time::UnixNanos::new(10_000_000_000);
        for emitted_at in [
            crate::time::UnixNanos::new(7_000_000_000),
            crate::time::UnixNanos::new(8_000_000_000),
        ] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            QueueAckHandle::new(tx, AckPosition::Memory, emitted_at, Arc::clone(&metrics))
                .resolve_delivered_at(delivered_at);
            assert_eq!(
                rx.recv().await,
                Some((AckPosition::Memory, AckDisposition::Delivered))
            );
        }
        assert_eq!(metrics.delivery_seconds.count(), 2);
        assert_eq!(metrics.delivery_seconds.sum(), 5.0);
    }

    #[tokio::test]
    async fn ack_handle_carries_position_through_resolve() {
        // Positions captured at recv time must travel back unchanged
        // through `resolve_delivered` — this is the core of the
        // positional-ack fix.
        let position = AckPosition::Disk {
            seq: 7,
            offset: 4242,
        };
        let (handle, mut rx) = QueueAckHandle::for_test_with_position(position);
        assert_eq!(handle.position(), position);
        handle.resolve_delivered();
        assert_eq!(rx.recv().await, Some((position, AckDisposition::Delivered)));
    }

    /// Dropping a handle without resolving sends `Dropped` so the
    /// consumer can still advance the cursor (avoiding queue stall on
    /// a buggy output) and bumps `events_failed`. The debug_assert
    /// path is exercised by `debug_assert_panics_on_handle_drop_without_resolve`.
    #[cfg(not(debug_assertions))]
    #[tokio::test]
    async fn ack_handle_drop_without_resolve_sends_dropped_in_release() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        drop(handle);
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Dropped))
        );
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "QueueAckHandle dropped without explicit resolve")]
    async fn debug_assert_panics_on_handle_drop_without_resolve() {
        let (handle, _rx) = QueueAckHandle::for_test();
        drop(handle);
    }

    // ---- consumer loop ack-bookkeeping ----

    /// Build a queue + spawn the consumer driving `writer`. Returns the
    /// sender, a shutdown signal, and the join handle.
    async fn spawn_consumer(
        writer: Arc<dyn Output>,
        metrics: Arc<crate::metrics::OutputMetrics>,
    ) -> (
        QueueSender,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = create_queue(
            "test".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 16,
            },
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_queue_consumer(
            receiver,
            writer,
            None,
            metrics,
            None,
            shutdown_rx,
        ));
        (sender, shutdown_tx, handle)
    }

    #[tokio::test]
    async fn consumer_acks_each_delivered_event() {
        let writer = Arc::new(ScriptedWriter::new(vec![
            Outcome::Delivered,
            Outcome::Delivered,
            Outcome::Delivered,
        ]));
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..3 {
            sender.send(owned_event()).await.unwrap();
        }
        // Tickle shutdown so the consumer wraps up; it must still
        // process the events already on the queue.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let _ = shutdown.send(true);
        handle.await.unwrap();
        assert_eq!(writer.calls(), 3);
        // Delivered dispositions do not bump `events_failed`.
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn consumer_handles_bug_path_via_drop_fallthrough() {
        // The bug path drops the handle without resolving — Drop sends
        // `Dropped`, and Drop's debug_assert fires on debug builds. We
        // only run this assertion in release builds (panics in debug
        // would make the test fail). The path still exists and is
        // counted; we just don't materialise the panic here.
        if cfg!(debug_assertions) {
            return;
        }
        let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug]));
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        sender.send(owned_event()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let _ = shutdown.send(true);
        handle.await.unwrap();
        assert!(metrics.events_failed.load(Ordering::Relaxed) >= 1);
    }

    // ---- batched-output shutdown ordering ----

    /// A batched-output mock: `consume` parks the ack handle in an
    /// internal buffer without resolving it (= the steady-state
    /// contract for a real batched output below `batch_size`). Only
    /// `shutdown()` drains the buffer and resolves every handle. This
    /// is exactly the shape that deadlocked the consumer when the
    /// previous ordering waited for `in_flight == 0` BEFORE calling
    /// `shutdown()`.
    struct BatchedMockWriter {
        buffer: tokio::sync::Mutex<Vec<QueueAckHandle>>,
        shutdown_mode: ShutdownMode,
        shutdown_called: std::sync::atomic::AtomicBool,
        consume_calls: std::sync::atomic::AtomicUsize,
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    #[derive(Clone, Copy)]
    enum ShutdownMode {
        /// Final flush succeeds: resolve every parked handle as Delivered.
        DeliverAll,
        /// Final flush fails: route every parked event to DLQ — but we
        /// only have handles here (no Event captured), so we resolve as
        /// Recovered to mirror the real shutdown DLQ path.
        FailAll,
    }

    impl BatchedMockWriter {
        fn new(mode: ShutdownMode) -> Self {
            Self {
                buffer: tokio::sync::Mutex::new(Vec::new()),
                shutdown_mode: mode,
                shutdown_called: std::sync::atomic::AtomicBool::new(false),
                consume_calls: std::sync::atomic::AtomicUsize::new(0),
                metrics: crate::metrics::OutputMetrics::for_testing(),
            }
        }
    }

    impl HasMetrics for BatchedMockWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for BatchedMockWriter {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.consume_calls.fetch_add(1, Ordering::Relaxed);
            self.buffer.lock().await.push(ack);
            Ok(())
        }

        async fn consume_shutdown(
            &self,
            _event: &Event,
            ack: QueueAckHandle,
        ) -> anyhow::Result<()> {
            // Batched mock: park into the same buffer that shutdown()
            // will drain. No flush trigger — mirrors the real batched
            // outputs' shutdown contract.
            self.consume_calls.fetch_add(1, Ordering::Relaxed);
            self.buffer.lock().await.push(ack);
            Ok(())
        }

        async fn shutdown(
            &self,
            _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
        ) -> anyhow::Result<()> {
            self.shutdown_called.store(true, Ordering::Relaxed);
            let leftovers = std::mem::take(&mut *self.buffer.lock().await);
            match self.shutdown_mode {
                ShutdownMode::DeliverAll => {
                    for ack in leftovers {
                        ack.resolve_delivered();
                    }
                }
                ShutdownMode::FailAll => {
                    for ack in leftovers {
                        // Mirror the real shutdown DLQ recovery path:
                        // failed final flush → resolve Recovered after
                        // routing each event to error_log.
                        self.metrics.events_failed.inc();
                        ack.resolve_recovered();
                    }
                }
            }
            Ok(())
        }
    }

    /// The ordering fix: outputs that hold handles inside an internal
    /// buffer until `shutdown()` runs must see `writer.shutdown()`
    /// called BEFORE the consumer waits for in-flight to drain. The
    /// old code reversed this and would have hung on a batched output.
    /// Here we verify the consumer exits cleanly and every event's ack
    /// disposition was reported (= the `Delivered` count on the
    /// metrics path is bumped by the output itself, but the consumer
    /// must have processed each disposition in the post-loop drain).
    #[tokio::test]
    async fn shutdown_drains_batched_buffer_before_consumer_exits() {
        let writer = Arc::new(BatchedMockWriter::new(ShutdownMode::DeliverAll));
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..5 {
            sender.send(owned_event()).await.unwrap();
        }
        // Give the consumer time to pull every event into the
        // writer's internal buffer (no flush — `consume` parks the
        // handles unresolved).
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(writer.consume_calls.load(Ordering::Relaxed), 5);

        let _ = shutdown.send(true);

        // The consumer must terminate without external help. If the
        // ordering reverts to "wait for in_flight == 0 before calling
        // shutdown()", this await blocks forever and the test times
        // out — that is the deadlock the fix prevents.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s after shutdown")
            .expect("consumer task panicked");

        assert!(
            writer.shutdown_called.load(Ordering::Relaxed),
            "writer.shutdown() must have been called"
        );
        // Delivered dispositions: no failures bumped by the consumer.
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    /// Shutdown DLQ routing: when the writer's `shutdown()` body fails
    /// its final flush and routes each buffered event to the error
    /// log, every handle resolves as `Recovered`. The consumer must
    /// drain those dispositions and exit cleanly.
    #[tokio::test]
    async fn shutdown_routes_failed_batch_to_dlq() {
        let writer = Arc::new(BatchedMockWriter::new(ShutdownMode::FailAll));
        let metrics = crate::metrics::OutputMetrics::for_testing();
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..3 {
            sender.send(owned_event()).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(writer.consume_calls.load(Ordering::Relaxed), 3);

        let _ = shutdown.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s")
            .expect("consumer task panicked");

        assert!(writer.shutdown_called.load(Ordering::Relaxed));
        // Each event's failure is bumped on the writer's metrics
        // (where real outputs track it). The consumer's per-event
        // metrics object is the same struct, but bumped on a
        // different path: `Recovered` does NOT touch the consumer's
        // counter (see `handle_ack_disposition`). So the consumer's
        // metrics stay at 0 and the writer's is at 3.
        assert_eq!(writer.metrics.events_failed.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    /// `ack_to(AckPosition::Memory)` on a memory-backed receiver is a
    /// silent no-op: the mpsc channel already consumed the event on
    /// recv, there is no persistent cursor to advance. This is the
    /// contract every memory-queue consumer relies on; a regression
    /// that, e.g., panicked here would break every non-disk output
    /// the moment the consumer loop tried to commit an ack.
    #[tokio::test]
    async fn memory_queue_ack_to_is_noop() {
        let (sender, mut receiver) = create_queue(
            "mem".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        sender.send(owned_event()).await.unwrap();
        let (_e, position) = receiver.recv().await.unwrap();
        assert_eq!(position, AckPosition::Memory);
        receiver.ack_to(position);
        // A second ack on the same position is still a clean no-op.
        receiver.ack_to(AckPosition::Memory);
    }

    // ---- disk queue wedge (the disk-queue fail-stop wedge) ----
    //
    // Fail-stop wedge: a disk-queue consumer that observes
    // `AckDisposition::Dropped` cannot honestly advance the cursor
    // past that position (bug / panic / DLQ-write failure paths
    // leave the event un-DLQ'd), so it stops accepting new events
    // and holds the cursor. The tests below pin the four legs of
    // that contract: cursor hold, accepting flip, memory-queue
    // opt-out, and replay-on-reopen.

    /// Build a queue + spawn consumer helper, generic over queue
    /// type so the disk-wedge tests below can reuse the same
    /// scaffolding as the memory-queue tests above.
    async fn spawn_consumer_with_queue(
        queue_type: QueueType,
        writer: Arc<dyn Output>,
        metrics: Arc<crate::metrics::OutputMetrics>,
    ) -> (
        QueueSender,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = create_queue(
            "wedge_test".into(),
            QueueConfig {
                queue_type,
                capacity: 16,
            },
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_queue_consumer(
            receiver,
            writer,
            None,
            metrics,
            None,
            shutdown_rx,
        ));
        (sender, shutdown_tx, handle)
    }

    async fn wait_for_wedge(metrics: &Arc<crate::metrics::OutputMetrics>) {
        for _ in 0..500 {
            if metrics.events_wedged.load(Ordering::Relaxed) >= 1 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        panic!(
            "wedge did not fire within 1s — events_wedged={}",
            metrics.events_wedged.load(Ordering::Relaxed),
        );
    }

    /// A disk-queue consumer that observes `Dropped` on an event
    /// stops accepting new events (accepting = false via the
    /// fail-stop wedge). Pin this so a regression that let the
    /// consumer keep pulling from the receiver would grow the
    /// disk in-flight bookkeeping unboundedly behind an
    /// un-advanceable cursor front.
    #[tokio::test]
    async fn disk_queue_wedges_on_dropped_and_stops_accepting() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug]));
        let metrics = Arc::clone(&writer.metrics);
        let (sender, _shutdown_tx, handle) = spawn_consumer_with_queue(
            QueueType::Disk {
                path: tmp.path().display().to_string(),
                max_size: 4 * 1024 * 1024,
            },
            writer.clone(),
            metrics.clone(),
        )
        .await;

        sender.send(owned_event()).await.unwrap();
        wait_for_wedge(&metrics).await;
        assert_eq!(
            writer.calls(),
            1,
            "exactly one consume should have run before the wedge fired; calls={}",
            writer.calls(),
        );

        // Subsequent send lands in the queue (producer side keeps
        // accepting) but the consumer must NOT dequeue it — we
        // pin this by waiting a beat and observing that
        // `writer.calls()` did not tick past 1.
        sender.send(owned_event()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            writer.calls(),
            1,
            "consumer must not pull a second event after wedge; calls={}",
            writer.calls(),
        );
        assert_eq!(metrics.events_wedged.load(Ordering::Relaxed), 1);
        // `events_failed` may be bumped multiple times per bug
        // event under the pre-existing shape (one from the ack
        // disposition arm, one from the outer `consume` Err
        // fallthrough), so pin the lower bound rather than an
        // exact count — the wedge-adjacent invariant we care
        // about is "at least one failure recorded", not the
        // exact bump multiplicity.
        assert!(
            metrics.events_failed.load(Ordering::Relaxed) >= 1,
            "at least one events_failed bump expected",
        );

        // Dropping the sender closes the producer channel; the
        // consumer should exit cleanly with in_flight == 0.
        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit after wedge + producer drop")
            .expect("consumer task must not panic");
    }

    /// A memory-queue consumer does NOT wedge on Dropped: a
    /// memory queue has no persistent cursor to hold and cannot
    /// replay on restart, so wedging would just cause loss with
    /// no recovery path. Memory queues retain a continue-and-count
    /// behavior on Dropped instead of the fail-stop wedge that
    /// disk queues use.
    #[tokio::test]
    async fn memory_queue_does_not_wedge_on_dropped() {
        let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug, Outcome::Delivered]));
        let metrics = Arc::clone(&writer.metrics);
        let (sender, _shutdown_tx, handle) =
            spawn_consumer_with_queue(QueueType::Memory, writer.clone(), metrics.clone()).await;

        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();

        // Give the consumer time to consume both events.
        for _ in 0..500 {
            if writer.calls() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(writer.calls(), 2, "memory queue must not wedge");
        assert_eq!(
            metrics.events_wedged.load(Ordering::Relaxed),
            0,
            "memory queue must never bump events_wedged",
        );

        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit after producer drop")
            .expect("consumer task must not panic");
    }

    /// A disk-queue consumer that wedges and then observes
    /// Delivered acks for subsequent events must still hold the
    /// disk cursor at the wedged (front) position — the contiguous-
    /// prefix ack in `DiskQueueReceiver::ack_to` naturally
    /// enforces this, but here we pin the behaviour end-to-end:
    /// even if a later Delivered ack somehow reaches
    /// `ack_to`, it must not advance the persisted cursor past
    /// the wedged front.
    ///
    /// Regression pin: if a future refactor collapsed the disk /
    /// memory branch above and started calling `ack_to` on the
    /// Dropped position, this test's disk cursor would advance
    /// and the reopen would find nothing to replay.
    #[tokio::test]
    async fn disk_queue_wedge_reopen_replays_from_wedged_front() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = QueueType::Disk {
            path: tmp.path().display().to_string(),
            max_size: 4 * 1024 * 1024,
        };

        // First run: send one event, script drops it → wedge.
        {
            let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug]));
            let metrics = Arc::clone(&writer.metrics);
            let (sender, _shutdown_tx, handle) =
                spawn_consumer_with_queue(disk.clone(), writer.clone(), metrics.clone()).await;
            sender.send(owned_event()).await.unwrap();
            wait_for_wedge(&metrics).await;
            drop(sender);
            tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .unwrap()
                .unwrap();
        }

        // Second run: reopen the same disk path. The wedged
        // event must replay — script says Delivered this time
        // (bug fixed) and the consumer drains cleanly.
        {
            let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Delivered]));
            let metrics = Arc::clone(&writer.metrics);
            let (sender, _shutdown_tx, handle) =
                spawn_consumer_with_queue(disk, writer.clone(), metrics.clone()).await;
            for _ in 0..500 {
                if writer.calls() >= 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            assert_eq!(
                writer.calls(),
                1,
                "reopen must replay the wedged event; calls={}",
                writer.calls(),
            );
            assert_eq!(
                metrics.events_wedged.load(Ordering::Relaxed),
                0,
                "clean-second-run must not wedge again",
            );
            drop(sender);
            tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .unwrap()
                .unwrap();
        }
    }

    /// Batched-sink wedge exit calls `shutdown_wedged`, not
    /// `shutdown`. The wedge contract is "no new work through a
    /// bug-path output" — the queue consumer must NOT enter the
    /// normal shutdown path (which does one more bounded send)
    /// when a Dropped-on-disk disposition fired the wedge. This
    /// mirror pin catches a regression where a refactor collapsed
    /// the `wedged` / else arms.
    #[test]
    fn queue_consumer_wedge_arm_calls_shutdown_wedged_not_shutdown() {
        let src = include_str!("mod.rs");
        let consumer_body = src
            .find("pub(crate) async fn run_queue_consumer")
            .expect("run_queue_consumer must exist");
        // Look inside the consumer body for the two branches.
        let window = &src[consumer_body..consumer_body + 20_000];
        assert!(
            window.contains("if wedged {")
                && window.contains("writer.shutdown_wedged(error_log.as_ref()).await"),
            "run_queue_consumer must drive `writer.shutdown_wedged()` on the wedged branch"
        );
        assert!(
            window.contains("} else if let Err(e) = writer.shutdown(error_log.as_ref()).await"),
            "the non-wedged branch must still drive `writer.shutdown()`"
        );
    }

    /// Batched wedge scenario end-to-end: the flusher actor's
    /// analogue (a background task inside the mock) fires a
    /// Dropped ack for one already-consumed event, which trips
    /// the disk-queue wedge while other handles are still parked
    /// in the mock's buffer. The queue consumer must take the
    /// wedge-exit `shutdown_wedged` path — resolving the parked
    /// handles WITHOUT attempting a further send — so the ack
    /// drain does not hang on messages that will never arrive.
    ///
    /// Regression pin: the previous shape skipped both `shutdown`
    /// and `shutdown_wedged` on wedge exit, leaving parked
    /// handles unresolved and forcing the runtime's 10 s wall-
    /// clock timeout to unblock the drain.
    #[tokio::test]
    async fn batched_wedge_resolves_parked_buffer_without_send() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicUsize;

        struct WedgeMockWriter {
            buffer: tokio::sync::Mutex<Vec<QueueAckHandle>>,
            consume_calls: AtomicUsize,
            send_calls: AtomicUsize,
            shutdown_called: AtomicBool,
            shutdown_wedged_called: AtomicBool,
            metrics: Arc<crate::metrics::OutputMetrics>,
        }

        impl HasMetrics for WedgeMockWriter {
            type Stats = crate::metrics::OutputMetrics;
            fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
                Arc::clone(&self.metrics)
            }
        }

        #[async_trait::async_trait]
        impl Output for WedgeMockWriter {
            async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
                self.consume_calls.fetch_add(1, Ordering::Relaxed);
                self.buffer.lock().await.push(ack);
                Ok(())
            }

            async fn consume_shutdown(
                &self,
                _event: &Event,
                ack: QueueAckHandle,
            ) -> anyhow::Result<()> {
                self.buffer.lock().await.push(ack);
                Ok(())
            }

            async fn shutdown(
                &self,
                _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
            ) -> anyhow::Result<()> {
                self.shutdown_called.store(true, Ordering::Relaxed);
                // Normal shutdown pretends to do one bounded send.
                // The wedge path must NEVER reach here.
                self.send_calls.fetch_add(1, Ordering::Relaxed);
                let leftover = std::mem::take(&mut *self.buffer.lock().await);
                for ack in leftover {
                    ack.resolve_recovered();
                }
                Ok(())
            }

            async fn shutdown_wedged(
                &self,
                _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
            ) -> anyhow::Result<()> {
                self.shutdown_wedged_called.store(true, Ordering::Relaxed);
                // Wedge contract: no send. Resolve every parked
                // handle as Recovered (mirrors
                // `route_shutdown_batch_ambiguous_to_dlq` folding
                // on the ack channel — on a real disk queue this
                // path forces Dropped; the memory-queue fold happens
                // inside `resolve_ack_from_dlq_outcome`, but here we
                // just need every handle resolved so the drain
                // exits).
                let leftover = std::mem::take(&mut *self.buffer.lock().await);
                for ack in leftover {
                    ack.resolve_recovered();
                }
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WedgeMockWriter {
            buffer: tokio::sync::Mutex::new(Vec::new()),
            consume_calls: AtomicUsize::new(0),
            send_calls: AtomicUsize::new(0),
            shutdown_called: AtomicBool::new(false),
            shutdown_wedged_called: AtomicBool::new(false),
            metrics: crate::metrics::OutputMetrics::for_testing(),
        });
        let metrics = Arc::clone(&writer.metrics);
        let (sender, shutdown_tx, handle) = spawn_consumer_with_queue(
            QueueType::Disk {
                path: tmp.path().display().to_string(),
                max_size: 4 * 1024 * 1024,
            },
            writer.clone(),
            metrics.clone(),
        )
        .await;

        // Dispatch 4 events into the batched buffer.
        for _ in 0..4 {
            sender.send(owned_event()).await.unwrap();
        }
        for _ in 0..500 {
            if writer.consume_calls.load(Ordering::Relaxed) >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(writer.consume_calls.load(Ordering::Relaxed), 4);

        // Simulate the flusher actor firing a Dropped ack for the
        // first parked handle — the disposition that trips the
        // fail-stop wedge on a disk queue. The remaining 3 handles
        // stay parked in the buffer.
        {
            let mut buf = writer.buffer.lock().await;
            let first = buf.remove(0);
            first.resolve_dropped();
        }
        wait_for_wedge(&metrics).await;

        // Trigger graceful shutdown. The consumer must take the
        // wedge-exit path — `shutdown_wedged` on the mock — and
        // exit within the timeout without hanging on the 3
        // still-parked handles.
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s on wedge shutdown")
            .expect("consumer task must not panic");

        assert!(
            writer.shutdown_wedged_called.load(Ordering::Relaxed),
            "wedge exit must drive shutdown_wedged()"
        );
        assert!(
            !writer.shutdown_called.load(Ordering::Relaxed),
            "wedge exit must NOT drive shutdown() (that would attempt a further send)"
        );
        assert_eq!(
            writer.send_calls.load(Ordering::Relaxed),
            0,
            "wedge contract: no send attempted on the wedge-exit path"
        );
    }

    /// Wedge fires exactly once per consumer lifetime — a second
    /// Dropped after the first wedge (in-flight tail scenario)
    /// must not double-count `events_wedged`.
    #[tokio::test]
    async fn disk_queue_wedge_fires_at_most_once() {
        let tmp = tempfile::tempdir().unwrap();
        // Two Bug outcomes in a row: only the first should flip
        // the wedge; the second (if it dequeued somehow) must
        // not tick the counter again.
        let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug, Outcome::Bug]));
        let metrics = Arc::clone(&writer.metrics);
        let (sender, _shutdown_tx, handle) = spawn_consumer_with_queue(
            QueueType::Disk {
                path: tmp.path().display().to_string(),
                max_size: 4 * 1024 * 1024,
            },
            writer.clone(),
            metrics.clone(),
        )
        .await;

        sender.send(owned_event()).await.unwrap();
        wait_for_wedge(&metrics).await;
        // Additional sends after wedge should NOT be dequeued.
        sender.send(owned_event()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            metrics.events_wedged.load(Ordering::Relaxed),
            1,
            "wedge counter must fire exactly once per consumer lifetime",
        );

        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    // ---- recv_many batch drain ----

    /// Batch drain preserves in-order delivery: N events pushed in
    /// sender order arrive at the output in the same order. The disk
    /// cursor's contiguous-prefix ack advances rely on this — a
    /// mechanical refactor that pulled events out of `recv_many` in a
    /// reordered way would silently break at-least-once.
    #[tokio::test]
    async fn recv_many_preserves_order_across_batch() {
        let (sender, mut receiver) = create_queue(
            "batch_order".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 16,
            },
        )
        .unwrap();
        for i in 0..8u32 {
            let e = Event::new(
                Bytes::copy_from_slice(&i.to_le_bytes()),
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            );
            sender
                .send(QueuedEvent::new(e, crate::time::UnixNanos::now()))
                .await
                .unwrap();
        }
        // Give the sends time to land so recv_many drains the whole
        // backlog in one call (deterministic single-batch scenario).
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let mut buf: Vec<(QueuedEvent, AckPosition)> = Vec::with_capacity(16);
        let n = receiver.recv_many(&mut buf, 16).await;
        assert_eq!(n, 8, "single-batch drain must collect all 8 events");
        for (i, (event, _)) in buf.iter().enumerate() {
            let mut expected = [0u8; 4];
            expected.copy_from_slice(event.ingress.as_ref());
            assert_eq!(
                u32::from_le_bytes(expected),
                i as u32,
                "recv_many must preserve sender order at index {}",
                i
            );
        }
    }

    /// Every event in a batch gets its own `QueueAckHandle` and its own
    /// position feedback — batching must not coalesce acks. The
    /// consumer path constructs one handle per drained event; this
    /// test verifies at the receiver level that positions are distinct
    /// across a batch.
    #[tokio::test]
    async fn recv_many_produces_distinct_positions_on_disk_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let (sender, mut receiver) = create_queue(
            "batch_positions".into(),
            QueueConfig {
                queue_type: QueueType::Disk {
                    path: tmp.path().display().to_string(),
                    max_size: 4 * 1024 * 1024,
                },
                capacity: 16,
            },
        )
        .unwrap();
        for _ in 0..5 {
            sender.send(owned_event()).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let mut buf: Vec<(QueuedEvent, AckPosition)> = Vec::with_capacity(8);
        let n = receiver.recv_many(&mut buf, 8).await;
        assert_eq!(n, 5);
        let mut seen: std::collections::HashSet<AckPosition> = std::collections::HashSet::new();
        for (_, pos) in &buf {
            assert!(
                matches!(pos, AckPosition::Disk { .. }),
                "disk backend must produce Disk positions"
            );
            assert!(
                seen.insert(*pos),
                "each event in a batch must carry a unique position; duplicate = {:?}",
                pos
            );
        }
    }

    /// Natural close mid-drain: the sender is dropped while the
    /// receiver still holds buffered events. `recv_many` must deliver
    /// the remaining events on the batch that observes them, and the
    /// next call must return 0 (queue closed). The consumer loop's
    /// `n == 0` branch depends on this equivalence with the old
    /// `recv() -> None`.
    #[tokio::test]
    async fn recv_many_returns_remainder_then_zero_after_sender_drop() {
        let (sender, mut receiver) = create_queue(
            "close_batch".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
            },
        )
        .unwrap();
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        drop(sender);

        let mut buf: Vec<(QueuedEvent, AckPosition)> = Vec::with_capacity(8);
        let n = receiver.recv_many(&mut buf, 8).await;
        assert_eq!(n, 3, "closing after 3 sends must still yield those 3");
        buf.clear();
        let n2 = receiver.recv_many(&mut buf, 8).await;
        assert_eq!(n2, 0, "subsequent call on closed empty queue must return 0");
    }

    /// Structural pin: the steady-state arm of `run_queue_consumer`
    /// uses `recv_many` against the reused batch buffer, and returns
    /// through the queue-closed branch when `n == 0`. Prevents a
    /// refactor from silently reverting to per-event `recv()` (which
    /// would restore the wake amplification that this phase exists to
    /// mitigate) or from moving the batch buffer inside the select
    /// arm (where cancellation could strand pushed events).
    #[test]
    fn steady_state_arm_uses_recv_many_with_reused_buffer() {
        let src = include_str!("mod.rs");
        // The batch buffer must be declared before the outer `loop`,
        // not inside the select arm — otherwise its lifetime would
        // not span cancellations. We only assert the declaration
        // shape; a rename of `batch` would only fail this pin (not
        // the correctness of the code) and force a deliberate update.
        // Assemble the declaration so this test cannot satisfy its own
        // source-text assertion when the production declaration drifts.
        let batch_declaration = concat!(
            "let mut batch: Vec<(Queued",
            "Event, AckPosition)> = Vec::with_capacity(RECV_BATCH_MAX);"
        );
        assert!(
            src.contains(batch_declaration),
            "batch buffer must be declared once outside the select loop"
        );
        // The steady-state arm must dispatch through `recv_many` and
        // handle both `n == 0` (close) and drain-and-process paths.
        assert!(
            src.contains("receiver.recv_many(&mut batch, RECV_BATCH_MAX)"),
            "steady-state arm must call recv_many with the reused batch buffer"
        );
        assert!(
            src.contains("if n == 0 {"),
            "steady-state arm must observe queue closure through n == 0"
        );
        // Guardrail: the old per-event single-recv shape must not survive
        // alongside the batch shape. Assemble the forbidden literal at
        // runtime so this test file itself does not contain it (the
        // pattern-match would otherwise fail on the pin itself).
        let forbidden = concat!("input = receiver.", "recv(), if accepting =>");
        assert!(
            !src.contains(forbidden),
            "steady-state arm must not use the pre-batch per-event recv() shape"
        );
    }

    // ---- adaptive spin-before-park controller ----

    /// R1: recording a spin hit doubles the budget (saturating at
    /// `SPIN_CAP`), lifts evidence to at least the hit depth, and
    /// zeroes the staleness counter. From a zero budget, growth
    /// still fires because `max(1) * 2 = 2`.
    #[test]
    fn spin_controller_hit_grows_budget_and_records_evidence() {
        let mut ctrl = SpinController::new();
        assert_eq!(ctrl.budget, 0);
        ctrl.record_hit(5);
        assert_eq!(ctrl.budget, 2, "growth from zero uses max(budget, 1) * 2");
        assert_eq!(ctrl.evidence, 5, "evidence tracks the deepest hit position");
        assert_eq!(ctrl.evidence_age, 0);

        // Repeated hits keep doubling until SPIN_CAP.
        for _ in 0..10 {
            ctrl.record_hit(3);
        }
        assert_eq!(ctrl.budget, SPIN_CAP, "budget saturates at SPIN_CAP");
        assert_eq!(
            ctrl.evidence, 5,
            "evidence stays at the max seen, not the current hit"
        );
    }

    /// R2: after a hit at position K, a spin-miss decay refuses to
    /// drop the budget below K. This is what stops one anomalous
    /// long-gap event from collapsing the controller into a
    /// permanently-low absorbing state under a steady arrival regime
    /// (where the spin-miss would otherwise halve the budget below
    /// the arrival mode and never hit again).
    #[test]
    fn spin_controller_evidence_floor_prevents_decay_below_arrival_mode() {
        let mut ctrl = SpinController::new();
        // Force budget up to SPIN_CAP with evidence pinned at 40.
        ctrl.record_hit(40);
        while ctrl.budget < SPIN_CAP {
            ctrl.record_hit(40);
        }
        assert_eq!(ctrl.evidence, 40);

        // A single miss halves budget but must not drop below evidence.
        for _ in 0..10 {
            ctrl.record_spin_miss();
            assert!(
                ctrl.budget >= ctrl.evidence,
                "budget {} decayed below evidence {}",
                ctrl.budget,
                ctrl.evidence
            );
        }
    }

    /// R3: after `EVIDENCE_STALE_AFTER` consecutive misses, evidence
    /// halves and the staleness counter resets. With enough misses
    /// both evidence and budget decay to zero — this is what
    /// re-enables the R5 idle-safety property after a load drop.
    #[test]
    fn spin_controller_staleness_lets_evidence_and_budget_decay_to_zero() {
        let mut ctrl = SpinController::new();
        ctrl.record_hit(64);
        while ctrl.budget < SPIN_CAP {
            ctrl.record_hit(64);
        }
        assert_eq!(ctrl.evidence, 64);

        // Enough misses to walk evidence all the way down.
        // Each round of EVIDENCE_STALE_AFTER misses halves evidence.
        let miss_rounds = 20; // 20 * 16 = 320 misses, plenty of room to hit zero.
        for _ in 0..(miss_rounds * EVIDENCE_STALE_AFTER) {
            ctrl.record_spin_miss();
        }
        assert_eq!(
            ctrl.evidence, 0,
            "evidence must decay to zero after sustained misses (R3)"
        );
        assert_eq!(
            ctrl.budget, 0,
            "budget must decay to zero after evidence has been walked out"
        );
    }

    /// R4: from an absorbing zero budget (cold start or post-idle
    /// decay), a short park wake grows the budget. Without this
    /// escape the controller would be permanently disabled once
    /// budget hit zero — the only growth signal, spin-hit, cannot
    /// fire from budget = 0.
    #[test]
    fn spin_controller_pseudo_hit_escapes_zero_budget() {
        let mut ctrl = SpinController::new();
        assert_eq!(ctrl.budget, 0);

        ctrl.record_park(PSEUDO_HIT_PARK_NS / 2);
        assert_eq!(ctrl.budget, 2, "short park grows from zero via max(1)*2");
        assert_eq!(ctrl.evidence, 2, "evidence tracks the new budget");
        assert_eq!(ctrl.evidence_age, 0);

        // Repeated short parks keep growing.
        for _ in 0..10 {
            ctrl.record_park(1_000);
        }
        assert_eq!(ctrl.budget, SPIN_CAP);
    }

    /// R5: with `budget == 0` the caller must skip the spin phase
    /// entirely — no `try_recv`, no `spin_loop`. Idle daemons are
    /// byte-identical to the pre-controller receive path when the
    /// budget is zero.
    #[test]
    fn spin_controller_zero_budget_short_circuits_spin() {
        let ctrl = SpinController::new();
        assert_eq!(ctrl.budget, 0);
        assert!(
            ctrl.should_spin().is_none(),
            "zero budget must short-circuit the spin phase"
        );
    }

    /// A long park (past `PSEUDO_HIT_PARK_NS`) from an idle state
    /// leaves the controller in place. This preserves the
    /// idle-safety property: a genuinely idle daemon accumulates no
    /// budget through repeated long parks.
    #[test]
    fn spin_controller_long_park_from_zero_budget_is_no_op() {
        let mut ctrl = SpinController::new();
        for _ in 0..100 {
            ctrl.record_park(PSEUDO_HIT_PARK_NS * 100);
        }
        assert_eq!(ctrl.budget, 0);
        assert_eq!(ctrl.evidence, 0);
        assert_eq!(ctrl.evidence_age, 0);
    }

    /// Integration pin: under a trickle load (arrivals slower than
    /// the pseudo-hit threshold) the spin budget must not grow. The
    /// receiver should behave exactly like the pre-controller path
    /// under such loads, and idle CPU must not rise. This test
    /// exercises the receiver end-to-end, not the state machine in
    /// isolation.
    #[tokio::test]
    async fn trickle_load_keeps_spin_budget_at_zero() {
        let (sender, mut receiver) = create_queue(
            "trickle".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();

        // Warm-up round: prove that Some path is exercised.
        sender.send(owned_event()).await.unwrap();
        let mut buf: Vec<(QueuedEvent, AckPosition)> = Vec::with_capacity(4);
        assert_eq!(receiver.recv_many(&mut buf, 4).await, 1);
        buf.clear();

        // The spin budget must stay at zero: each recv_many parks,
        // and the park duration exceeds the pseudo-hit threshold by
        // orders of magnitude.
        for _ in 0..3 {
            let send_delay = std::time::Duration::from_millis(50);
            let sender = sender.clone();
            tokio::spawn(async move {
                tokio::time::sleep(send_delay).await;
                let _ = sender.send(owned_event()).await;
            });
            assert_eq!(receiver.recv_many(&mut buf, 4).await, 1);
            buf.clear();
            assert_eq!(
                receiver.spin_ctrl.budget, 0,
                "trickle load must leave the spin budget at zero (R5 idle path)"
            );
        }
    }

    /// Structural pin: `recv_many` must consult the spin controller
    /// via `should_spin`, must time the park duration and feed it to
    /// `record_park`, and must call `record_hit` on spin success and
    /// `record_spin_miss` on budget exhaustion. Prevents a mechanical
    /// simplification from dropping the controller integration while
    /// keeping the batch-drain shape — the runtime symptom would be a
    /// throughput regression on the batch-drain measurement gate that
    /// no unit test can produce.
    #[test]
    fn recv_many_uses_spin_controller_and_times_park() {
        let src = include_str!("mod.rs");
        // Locate the recv_many body between the pub-async signature
        // and the trailing brace at the end of the fn.
        let sig_marker = "pub async fn recv_many(\n        &mut self,\n        buf: &mut Vec<(QueuedEvent, AckPosition)>,\n        max: usize,\n    ) -> usize {";
        let start = src
            .find(sig_marker)
            .expect("recv_many signature marker must exist");
        // The body ends at the first blank-line `}` after the sig.
        let body_tail = &src[start..];
        // Take a generous slice — the body is a couple of dozen lines.
        let slice_end = body_tail
            .find("    /// Commit a specific event's position as processed.")
            .expect("recv_many must be followed by ack_to doc");
        let body = &body_tail[..slice_end];

        assert!(
            body.contains("self.spin_budget_for_backend()"),
            "recv_many must gate the spin phase through spin_budget_for_backend \
             (which composes SpinController::should_spin with the backend check)"
        );
        assert!(
            body.contains("self.spin_ctrl.record_hit("),
            "recv_many must call record_hit on spin success"
        );
        assert!(
            body.contains("self.spin_ctrl.record_spin_miss()"),
            "recv_many must call record_spin_miss on budget exhaustion"
        );
        assert!(
            body.contains("std::time::Instant::now()"),
            "recv_many must time the park duration for record_park"
        );
        assert!(
            body.contains("self.spin_ctrl.record_park("),
            "recv_many must feed the park duration to record_park"
        );
        assert!(
            body.contains("std::hint::spin_loop()"),
            "recv_many must use std::hint::spin_loop between spin iterations"
        );
    }

    /// Backend-aware spin gating: on the memory backend the helper
    /// forwards the controller's current budget (nanosecond-scale
    /// `try_recv` per iteration is worth spinning); on the disk
    /// backend the helper returns `None` regardless of controller
    /// state (multi-microsecond `try_recv` per iteration would cost
    /// more than the park round trip the spin phase is meant to
    /// skip). Prevents a mechanical simplification from lifting the
    /// spin phase back onto the disk backend, which no unit test
    /// timing check would catch on its own.
    #[tokio::test]
    async fn spin_budget_for_backend_returns_none_on_disk_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let (_sender, mut receiver) = create_queue(
            "spin_budget_disk".into(),
            QueueConfig {
                queue_type: QueueType::Disk {
                    path: tmp.path().display().to_string(),
                    max_size: 4 * 1024 * 1024,
                },
                capacity: 16,
            },
        )
        .unwrap();

        // Warm the receiver's own controller — a copy would only pin
        // the SpinController state machine, not the backend gate the
        // helper on the receiver is supposed to apply on top of it.
        receiver.spin_ctrl.record_hit(SPIN_CAP);
        assert!(
            receiver.spin_ctrl.should_spin().is_some(),
            "sanity: receiver's controller is warm — the assertion below is checking the gate, not the controller"
        );

        // With the receiver's own controller genuinely warm, the
        // helper must still return None because the backend is disk.
        assert!(
            receiver.spin_budget_for_backend().is_none(),
            "disk backend must never enter the spin phase, even when the receiver's controller is warm"
        );
    }

    #[tokio::test]
    async fn spin_budget_for_backend_forwards_controller_on_memory_backend() {
        let (_sender, mut receiver) = create_queue(
            "spin_budget_memory".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 16,
            },
        )
        .unwrap();

        // Cold controller: helper returns None (matches R5).
        assert!(receiver.spin_budget_for_backend().is_none());

        // Warm the controller and confirm the helper now forwards it.
        receiver.spin_ctrl.record_hit(4);
        assert!(
            receiver.spin_budget_for_backend().is_some(),
            "memory backend with a warm controller must return Some(budget)"
        );
    }

    // ---- backend-aware shutdown drain ----

    /// The load-bearing tokio-mpsc guarantee that `close()` +
    /// `recv-until-None` builds on: a `Receiver::close()` mid-flight
    /// does NOT cancel an outstanding permit's write — the value
    /// still becomes visible before `recv()` returns `None`.
    ///
    /// This is the exact scenario the previous `try_recv()` snapshot
    /// drain could not observe: a sender that had reserved a permit
    /// but not yet written the value would complete after the
    /// consumer's snapshot loop exited, silently dropping the event.
    /// The new shutdown drain relies on this tokio behavior; if a
    /// future tokio update ever broke it, the drain would need to
    /// change too — this test pins the assumption at the mpsc layer
    /// so that failure would show up here first, not as a lost event
    /// in the queue-level test.
    #[tokio::test]
    async fn tokio_mpsc_close_then_permit_send_still_visible() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
        let permit = tx.reserve().await.expect("reserve must succeed");
        rx.close();
        permit.send(42);
        drop(tx);
        assert_eq!(rx.recv().await, Some(42));
        assert_eq!(rx.recv().await, None);
    }

    /// `QueueReceiver::close()` on a memory backend refuses further
    /// sends, but events already buffered remain readable until the
    /// receiver observes `None`. This is what the shutdown drain
    /// depends on: after `close()`, drain with `recv().await` until
    /// `None` to catch every value that was in-flight when shutdown
    /// fired.
    #[tokio::test]
    async fn queue_receiver_close_then_recv_drains_buffered_before_none() {
        let (sender, mut receiver) = create_queue(
            "close_test".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        receiver.close();

        // Post-close sends are refused.
        let err = sender
            .send(owned_event())
            .await
            .expect_err("send after close must fail");
        assert!(matches!(err, QueueSendError::ChannelClosed));

        // Buffered events are still readable.
        assert!(receiver.recv().await.is_some(), "first buffered event");
        assert!(receiver.recv().await.is_some(), "second buffered event");
        // Drop the sender so `recv` can observe termination.
        drop(sender);
        assert!(receiver.recv().await.is_none(), "final None after drain");
    }

    /// A sender blocked on a full memory channel must wake with
    /// `ChannelClosed` when `receiver.close()` fires, not hang.
    /// Otherwise the shutdown drain could deadlock a pipeline worker
    /// that is mid-`send().await` on a saturated output queue.
    #[tokio::test]
    async fn blocked_sender_wakes_with_err_after_receiver_close() {
        let (sender, mut receiver) = create_queue(
            "block_test".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 1,
            },
        )
        .unwrap();
        sender.send(owned_event()).await.unwrap();

        let sender_clone = sender.clone();
        let blocked = tokio::spawn(async move { sender_clone.send(owned_event()).await });
        // Give the spawned task time to reach the permit wait.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        receiver.close();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .expect("blocked send must wake within 1s of receiver.close()")
            .expect("task must not panic");
        assert!(
            matches!(result, Err(QueueSendError::ChannelClosed)),
            "expected ChannelClosed, got {:?}",
            result
        );
    }

    /// End-to-end shutdown drain on a memory queue: events already
    /// buffered when shutdown fires reach `consume_shutdown`. The
    /// close-then-recv-until-None pattern is what guarantees this
    /// under the older `try_recv()` snapshot semantics; pins the
    /// visible behavior so a regression that stripped `close()` back
    /// out would fail here rather than at a customer.
    #[tokio::test]
    async fn memory_shutdown_drain_delivers_buffered_events_to_consume_shutdown() {
        let writer = Arc::new(ScriptedWriter::new(vec![]));
        let metrics = Arc::clone(&writer.metrics);
        let (sender, shutdown_tx, handle) =
            spawn_consumer_with_queue(QueueType::Memory, writer.clone(), metrics.clone()).await;

        // Buffer three events without letting the consumer touch
        // them by holding the runtime yield until after the sends.
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();

        shutdown_tx.send(true).unwrap();
        drop(sender);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s of shutdown")
            .expect("consumer task must not panic");

        assert_eq!(
            writer.calls(),
            3,
            "memory drain must deliver all buffered events to consume_shutdown",
        );
        assert_eq!(metrics.queue_depth.load(Ordering::Relaxed), 0);
    }

    /// Disk backend: shutdown must NOT drain unread WAL entries
    /// into `consume_shutdown`. Unread entries stay on disk and are
    /// available via next-start replay. The previous `try_recv()`
    /// loop over the WAL would pull the entire durable backlog into
    /// RAM for the shutdown window and slow the flush deadline.
    #[tokio::test]
    async fn disk_shutdown_drain_leaves_unread_backlog_for_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = QueueType::Disk {
            path: tmp.path().display().to_string(),
            max_size: 4 * 1024 * 1024,
        };
        let writer = Arc::new(ScriptedWriter::new(vec![]));
        let metrics = Arc::clone(&writer.metrics);
        let (sender, shutdown_tx, handle) =
            spawn_consumer_with_queue(disk.clone(), writer.clone(), metrics.clone()).await;

        // Signal shutdown before any event lands so the consumer
        // takes the shutdown branch immediately (before draining).
        shutdown_tx.send(true).unwrap();
        // Yield so the consumer observes shutdown.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Now write events to the WAL. The sender writes directly
        // to disk; the consumer is already in the shutdown-skip
        // branch and must not read them.
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        sender.send(owned_event()).await.unwrap();
        drop(sender);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit")
            .expect("consumer task must not panic");
        assert_eq!(metrics.queue_depth.load(Ordering::Relaxed), 0);

        assert_eq!(
            writer.calls(),
            0,
            "disk shutdown drain must not read unread WAL entries into consume_shutdown",
        );

        // Reopen the disk queue at the same path — the three
        // unconsumed events must replay.
        let (_reopen_sender, mut reopen_receiver) = create_queue(
            "wedge_test".into(),
            QueueConfig {
                queue_type: disk,
                capacity: 16,
            },
        )
        .unwrap();
        let mut replayed = 0;
        for _ in 0..3 {
            if reopen_receiver.recv().await.is_some() {
                replayed += 1;
            }
        }
        assert_eq!(
            replayed, 3,
            "disk queue must replay all three unconsumed events on reopen",
        );
    }

    /// Structural pin: the shutdown arm in `run_queue_consumer`
    /// branches on `backend_kind()`, uses `close() + recv().await`
    /// on the memory backend, and does not fall back to any
    /// `try_recv()` snapshot. Prevents a mechanical refactor from
    /// silently reintroducing the C1 permit-holder race.
    #[test]
    fn shutdown_drain_arm_is_backend_aware_and_uses_close_recv_pattern() {
        let src = include_str!("mod.rs");
        let arm_start = src
            .find("_ = shutdown.changed(), if accepting || wedged =>")
            .expect("shutdown arm marker must exist");
        // Isolate the arm body from the wedge-shutdown early-break
        // to the next `break;` — the exit of the memory drain — and
        // take everything between as the arm body. The wedge branch
        // (`if wedged { … break; }`) sits before the backend split,
        // so start the search past its break to reach the drain
        // scaffolding the pins below cover.
        let arm_tail = &src[arm_start..];
        let wedge_break = arm_tail
            .find("if wedged {")
            .expect("shutdown arm must handle wedged early-break");
        let after_wedge_break = wedge_break
            + arm_tail[wedge_break..]
                .find("break;")
                .expect("wedge branch must break out")
            + "break;".len();
        let drain_tail = &arm_tail[after_wedge_break..];
        let body_end = drain_tail
            .find("break;")
            .expect("shutdown arm must break out of the select loop after the drain");
        let body = &drain_tail[..body_end];

        assert!(
            body.contains("receiver.backend_kind()"),
            "shutdown arm must branch on backend_kind()",
        );
        assert!(
            body.contains("QueueBackendKind::Memory"),
            "shutdown arm must have a memory branch",
        );
        assert!(
            body.contains("QueueBackendKind::Disk"),
            "shutdown arm must have a disk branch",
        );
        assert!(
            body.contains("receiver.close()"),
            "memory branch must close the receiver before draining",
        );
        assert!(
            body.contains("receiver.recv().await"),
            "memory branch must drain with recv().await, not try_recv()",
        );
        assert!(
            !body.contains("receiver.try_recv()"),
            "shutdown drain must not use try_recv() — permit-holder race",
        );
    }
}

// ---------------------------------------------------------------------------
// Schema-splice regression tests
// ---------------------------------------------------------------------------
//
// Before the retry-schema splice, only the two OTLP outputs declared
// `retry` in their `property_schema()` even though
// `RetryConfig::from_output_properties` reads it for *every* output.
// The tests below pin the invariant: every non-OTLP output's schema
// accepts a `retry { ... }`
// block without raising `UnknownKey`. Adding a third output schema
// later is then a hard failure here if the splice is forgotten.
#[cfg(test)]
mod schema_splice_tests {
    use super::{BackoffStrategy, RetryConfig};
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::{PropertySpec, SchemaError, SchemaErrorKind, validate};
    use crate::modules::Module;
    use crate::modules::output::file::FileOutput;
    use crate::modules::output::http::HttpOutput;
    use crate::modules::output::otlp::grpc::OtlpGrpcOutput;
    use crate::modules::output::otlp::http::OtlpHttpOutput;
    use crate::modules::output::stdout::StdoutOutput;
    use crate::modules::output::syslog_tcp::SyslogTcpOutput;
    use crate::modules::output::syslog_udp::SyslogUdpOutput;
    use crate::modules::output::unix_socket::UnixSocketOutput;

    #[cfg(feature = "kafka")]
    use crate::modules::output::kafka::KafkaOutput;

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties,
        }
    }

    /// `retry { max_attempts 3 initial_wait "100ms" max_wait "5s" backoff exponential }`
    fn full_retry_block() -> Property {
        block(
            "retry",
            vec![
                kv("max_attempts", ExprKind::IntLit(3)),
                kv("initial_wait", ExprKind::StringLit("100ms".into())),
                kv("max_wait", ExprKind::StringLit("5s".into())),
                kv("backoff", ExprKind::Ident(vec!["exponential".into()])),
            ],
        )
    }

    /// Filter out `UnknownKey` errors for the splice contract — other
    /// errors (e.g. missing required keys we didn't supply for the
    /// schema under test) are off-topic for this test.
    fn unknown_key_errs(errs: &[SchemaError]) -> Vec<&SchemaError> {
        errs.iter()
            .filter(|e| matches!(e.kind, SchemaErrorKind::UnknownKey))
            .collect()
    }

    fn assert_accepts(schema: &[PropertySpec], output_name: &str) {
        let props = vec![full_retry_block()];
        let errs = validate(&props, schema);
        let unknown = unknown_key_errs(&errs);
        assert!(
            unknown.is_empty(),
            "output '{}': retry should be accepted by schema, \
             got UnknownKey errors for: {:?}",
            output_name,
            unknown.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_output_schema_accepts_retry() {
        // The full matrix lives in one test so adding a new output
        // either splices the common const into its schema or fails
        // this test loudly.
        assert_accepts(StdoutOutput::property_schema().unwrap(), "stdout");
        assert_accepts(FileOutput::property_schema().unwrap(), "file");
        assert_accepts(HttpOutput::property_schema().unwrap(), "http");
        assert_accepts(SyslogTcpOutput::property_schema().unwrap(), "syslog_tcp");
        assert_accepts(SyslogUdpOutput::property_schema().unwrap(), "syslog_udp");
        assert_accepts(UnixSocketOutput::property_schema().unwrap(), "unix_socket");
        assert_accepts(OtlpGrpcOutput::property_schema().unwrap(), "otlp_grpc");
        assert_accepts(OtlpHttpOutput::property_schema().unwrap(), "otlp_http");
        #[cfg(feature = "kafka")]
        assert_accepts(KafkaOutput::property_schema().unwrap(), "kafka");
    }

    /// Inside the `retry { ... }` block, the four documented keys must
    /// be the only accepted ones across every output schema —
    /// confirms the hoist into `crate::queue` keeps the OTLP-era block
    /// shape exactly (no key drift).
    #[test]
    fn retry_block_rejects_unknown_inner_key_in_all_outputs() {
        let bad_retry = block("retry", vec![kv("typo", ExprKind::IntLit(1))]);
        let props = vec![bad_retry];
        #[allow(unused_mut)]
        let mut schemas: Vec<(&[PropertySpec], &str)> = vec![
            (StdoutOutput::property_schema().unwrap(), "stdout"),
            (FileOutput::property_schema().unwrap(), "file"),
            (HttpOutput::property_schema().unwrap(), "http"),
            (SyslogTcpOutput::property_schema().unwrap(), "syslog_tcp"),
            (SyslogUdpOutput::property_schema().unwrap(), "syslog_udp"),
            (UnixSocketOutput::property_schema().unwrap(), "unix_socket"),
            (OtlpGrpcOutput::property_schema().unwrap(), "otlp_grpc"),
            (OtlpHttpOutput::property_schema().unwrap(), "otlp_http"),
        ];
        #[cfg(feature = "kafka")]
        schemas.push((KafkaOutput::property_schema().unwrap(), "kafka"));
        for (schema, name) in schemas {
            let errs = validate(&props, schema);
            let typo_err = errs
                .iter()
                .find(|e| matches!(e.kind, SchemaErrorKind::UnknownKey) && e.key == "typo");
            assert!(
                typo_err.is_some(),
                "output '{}': retry block should reject unknown inner key 'typo', got errs={:?}",
                name,
                errs.iter().map(|e| (&e.kind, &e.key)).collect::<Vec<_>>(),
            );
        }
    }

    /// `RetryConfig::from_output_properties` parses retry props on a
    /// non-OTLP output (kafka here) into the same fields the OTLP
    /// outputs have always populated. Anchors the "runtime behavior
    /// already matched, schema just caught up" invariant.
    #[test]
    fn retry_config_parser_reads_same_fields_for_non_otlp_outputs() {
        let props = vec![full_retry_block()];
        let cfg = RetryConfig::from_output_properties(&props)
            .expect("retry block parses for non-OTLP output");
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.initial_wait, std::time::Duration::from_millis(100));
        assert_eq!(cfg.max_wait, std::time::Duration::from_secs(5));
        assert!(matches!(cfg.backoff, BackoffStrategy::Exponential));
    }
}

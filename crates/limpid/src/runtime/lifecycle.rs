use super::*;

pub(super) const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const ABORT_JOIN_RESERVE: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) async fn shutdown_change_is_terminal(shutdown: &mut watch::Receiver<bool>) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CleanupOutcome {
    pub(super) forced_abort: bool,
    pub(super) abort_safe_incomplete: usize,
    pub(super) must_join_exceeded_threshold: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TaskKind {
    AbortSafe,
    MustJoin,
}

pub(super) fn output_task_kind(
    output_type: &str,
    queue_type: &queue::QueueType,
    has_error_log: bool,
    stdout_regular_file: bool,
) -> TaskKind {
    if output_type == "file"
        || (output_type == "stdout" && stdout_regular_file)
        || matches!(queue_type, queue::QueueType::Disk { .. })
        || has_error_log
    {
        TaskKind::MustJoin
    } else {
        TaskKind::AbortSafe
    }
}

pub(super) fn pipeline_task_kind(has_error_log: bool, has_disk_output: bool) -> TaskKind {
    if has_error_log || has_disk_output {
        TaskKind::MustJoin
    } else {
        TaskKind::AbortSafe
    }
}

pub(super) fn input_task_kind(input_type: &str) -> TaskKind {
    if matches!(input_type, "journal" | "windows_event_log") {
        TaskKind::MustJoin
    } else {
        TaskKind::AbortSafe
    }
}

pub(super) struct TrackedTask {
    pub(super) kind: TaskKind,
    pub(super) handle: Option<tokio::task::JoinHandle<()>>,
}

/// Owns every task created during startup until the runtime is fully committed.
/// Any explicit startup error uses [`Self::rollback`]; cancellation or panic is
/// covered by `Drop`, which transfers cleanup to the current Tokio runtime.
pub(super) struct StartupGuard {
    shutdown_tx: Option<watch::Sender<bool>>,
    handles: Vec<TrackedTask>,
    cleanup_executor: tokio::runtime::Handle,
    #[cfg(test)]
    drop_cleanup_observer: Option<Arc<DropCleanupObserver>>,
}

impl StartupGuard {
    pub(super) fn new(shutdown_tx: watch::Sender<bool>) -> Self {
        Self {
            shutdown_tx: Some(shutdown_tx),
            handles: Vec::new(),
            cleanup_executor: tokio::runtime::Handle::current(),
            #[cfg(test)]
            drop_cleanup_observer: DROP_CLEANUP_RESULT.try_with(Arc::clone).ok(),
        }
    }

    pub(super) fn track(&mut self, kind: TaskKind, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(TrackedTask {
            kind,
            handle: Some(handle),
        });
    }

    pub(super) fn extend(
        &mut self,
        kind: TaskKind,
        handles: impl IntoIterator<Item = tokio::task::JoinHandle<()>>,
    ) {
        self.handles
            .extend(handles.into_iter().map(|handle| TrackedTask {
                kind,
                handle: Some(handle),
            }));
    }

    pub(super) async fn rollback(self, original: anyhow::Error) -> anyhow::Error {
        self.rollback_with_timeout(original, SHUTDOWN_TIMEOUT).await
    }

    pub(super) async fn rollback_with_timeout(
        mut self,
        original: anyhow::Error,
        timeout: std::time::Duration,
    ) -> anyhow::Error {
        let shutdown_tx = self.shutdown_tx.take().expect("startup guard sender");
        let mut handles = std::mem::take(&mut self.handles);
        let cleanup = shutdown_tasks_with_timeout(&shutdown_tx, &mut handles, timeout).await;
        drop(shutdown_tx);
        if cleanup.abort_safe_incomplete != 0 {
            original.context(format!(
                "runtime startup rollback reached the hard cleanup deadline; {} abort-safe task(s) remain incomplete after abort",
                cleanup.abort_safe_incomplete
            ))
        } else if cleanup.must_join_exceeded_threshold {
            original.context(
                "runtime startup rollback exceeded the 10s health threshold; must-join resource owners completed before return",
            )
        } else if cleanup.forced_abort {
            original.context(
                "runtime startup rollback exceeded the graceful phase; pending tasks were aborted and joined within the global cleanup deadline",
            )
        } else {
            original.context("runtime startup rolled back all started tasks")
        }
    }

    pub(super) fn commit(mut self) -> (watch::Sender<bool>, Vec<TrackedTask>) {
        let shutdown_tx = self.shutdown_tx.take().expect("startup guard sender");
        let handles = std::mem::take(&mut self.handles);
        (shutdown_tx, handles)
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        let Some(shutdown_tx) = self.shutdown_tx.take() else {
            return;
        };
        let mut handles = std::mem::take(&mut self.handles);
        let _ = shutdown_tx.send(true);
        if handles.is_empty() {
            #[cfg(test)]
            if let Some(observer) = self.drop_cleanup_observer.take() {
                observer.complete(CleanupOutcome::default());
            }
            return;
        }
        #[cfg(test)]
        let observer = self.drop_cleanup_observer.take();
        // The originating runtime must remain alive until this cleanup task
        // completes. Dropping the runtime itself cancels all tasks by Tokio's
        // contract; no handle can extend an executor beyond that boundary.
        self.cleanup_executor.spawn(async move {
            let outcome = shutdown_tasks(&shutdown_tx, &mut handles).await;
            #[cfg(test)]
            if let Some(observer) = observer {
                observer.complete(outcome);
            }
            #[cfg(not(test))]
            let _ = outcome;
        });
    }
}

fn record_join_result(result: std::result::Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && error.is_panic()
    {
        error!("task panicked during shutdown: {error}");
    }
}

pub(super) async fn shutdown_tasks(
    shutdown_tx: &watch::Sender<bool>,
    handles: &mut [TrackedTask],
) -> CleanupOutcome {
    shutdown_tasks_with_timeout(shutdown_tx, handles, SHUTDOWN_TIMEOUT).await
}

pub(super) async fn shutdown_tasks_with_timeout(
    shutdown_tx: &watch::Sender<bool>,
    handles: &mut [TrackedTask],
    total_timeout: std::time::Duration,
) -> CleanupOutcome {
    shutdown_tasks_with_progress(shutdown_tx, handles, total_timeout, || {}).await
}

// Poll every owner on each wake. A slow first owner must not hide another
// owner's completion; completed handles are removed before any later poll.
async fn reap_tasks(
    handles: &mut [TrackedTask],
    until: Option<TaskKind>,
    joined: &mut (impl FnMut() + Send),
) {
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for task in handles.iter_mut() {
            if let Some(handle) = task.handle.as_mut() {
                match std::future::Future::poll(std::pin::Pin::new(handle), cx) {
                    std::task::Poll::Ready(result) => {
                        record_join_result(result);
                        task.handle = None;
                        joined();
                    }
                    std::task::Poll::Pending => {
                        if until.is_none_or(|kind| kind == task.kind) {
                            pending = true;
                        }
                    }
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
}

async fn shutdown_tasks_with_progress(
    shutdown_tx: &watch::Sender<bool>,
    handles: &mut [TrackedTask],
    total_timeout: std::time::Duration,
    mut joined: impl FnMut() + Send,
) -> CleanupOutcome {
    let _ = shutdown_tx.send(true);
    let started = tokio::time::Instant::now();
    let overall_deadline = started + total_timeout;
    let abort_reserve = ABORT_JOIN_RESERVE.min(total_timeout / 2);
    let graceful_deadline = overall_deadline - abort_reserve;
    let _ =
        tokio::time::timeout_at(graceful_deadline, reap_tasks(handles, None, &mut joined)).await;

    let forced_abort = handles
        .iter()
        .any(|task| task.kind == TaskKind::AbortSafe && task.handle.is_some());
    for task in handles.iter() {
        if task.kind == TaskKind::AbortSafe
            && let Some(handle) = &task.handle
        {
            handle.abort();
        }
    }
    // Still reap completed MustJoin owners while waiting for AbortSafe cleanup.
    let _ = tokio::time::timeout_at(
        overall_deadline,
        reap_tasks(handles, Some(TaskKind::AbortSafe), &mut joined),
    )
    .await;

    let abort_safe_incomplete = handles
        .iter()
        .filter(|task| task.kind == TaskKind::AbortSafe && task.handle.is_some())
        .count();
    let must_join_exceeded_threshold = handles
        .iter()
        .any(|task| task.kind == TaskKind::MustJoin && task.handle.is_some());
    // The deadline is only a health threshold for durability/disposition owners.
    // No MustJoin is aborted or detached; all retained handles remain owned.
    reap_tasks(handles, Some(TaskKind::MustJoin), &mut joined).await;
    CleanupOutcome {
        forced_abort,
        abort_safe_incomplete,
        must_join_exceeded_threshold,
    }
}

impl Runtime {
    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub(crate) fn blueprint(&self) -> Arc<crate::pipeline::RuntimeBlueprint> {
        Arc::clone(&self.blueprint)
    }

    pub async fn shutdown(self) {
        self.shutdown_with_progress(|| {}).await;
    }

    /// Reports completed work, never elapsed waiting time. A single blocked
    /// operation without real progress can still exceed the SCM wait hint.
    pub async fn shutdown_with_progress(self, mut notify: impl FnMut() + Send) {
        info!(
            "initiating graceful shutdown (timeout: {}s)",
            SHUTDOWN_TIMEOUT.as_secs()
        );
        let mut handles = self.handles;
        let observing = self.shutdown_progress.begin();
        let cleanup =
            shutdown_tasks_with_progress(&self.shutdown_tx, &mut handles, SHUTDOWN_TIMEOUT, || {
                self.shutdown_progress.mark()
            });
        tokio::pin!(cleanup);
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let cleanup = loop {
            tokio::select! {
                biased;
                outcome = &mut cleanup => {
                    // One final flush of actual progress, not a trailing task.
                    if observing.take() { notify(); }
                    break outcome;
                }
                _ = tick.tick() => {
                    if observing.take() { notify(); }
                }
            }
        };
        drop(observing);
        if cleanup.abort_safe_incomplete != 0 {
            error!(
                "shutdown reached the {}s hard deadline — {} abort-safe task(s) remain incomplete after abort",
                SHUTDOWN_TIMEOUT.as_secs(),
                cleanup.abort_safe_incomplete,
            );
        } else if cleanup.must_join_exceeded_threshold {
            warn!(
                "shutdown exceeded the {}s health threshold; must-join resource owners completed before return",
                SHUTDOWN_TIMEOUT.as_secs(),
            );
        } else if cleanup.forced_abort {
            warn!("shutdown exceeded the graceful phase; pending tasks were aborted and joined");
        } else {
            info!("shutdown complete");
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn shutdown_reaps_completed_owners_behind_a_pending_join() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let (tx, _) = tokio::sync::watch::channel(false);
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let joined = Arc::new(AtomicUsize::new(0));
        let observed = joined.clone();
        let shutdown = tokio::spawn(async move {
            let mut tasks = vec![
                super::TrackedTask {
                    kind: super::TaskKind::MustJoin,
                    handle: Some(tokio::spawn(async move {
                        let _ = wait.await;
                    })),
                },
                super::TrackedTask {
                    kind: super::TaskKind::MustJoin,
                    handle: Some(tokio::spawn(async {})),
                },
            ];
            super::shutdown_tasks_with_progress(
                &tx,
                &mut tasks,
                std::time::Duration::from_secs(1),
                || {
                    observed.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
            assert!(tasks.iter().all(|task| task.handle.is_none()));
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before_release = joined.load(Ordering::SeqCst);
        assert!(!shutdown.is_finished());
        release.send(()).unwrap();
        shutdown.await.unwrap();
        assert_eq!(
            before_release, 1,
            "a pending first owner must not hide later completions"
        );
        assert_eq!(joined.load(Ordering::SeqCst), 2);
    }
    use super::*;

    #[tokio::test]
    async fn shutdown_progress_requires_join_and_retains_must_join_ownership() {
        let (shutdown_tx, _) = watch::channel(false);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&progress);
        let cleanup = tokio::spawn(async move {
            let mut tasks = vec![TrackedTask {
                kind: TaskKind::MustJoin,
                handle: Some(tokio::spawn(async move { release_rx.await.unwrap() })),
            }];
            let outcome = shutdown_tasks_with_progress(
                &shutdown_tx,
                &mut tasks,
                std::time::Duration::from_millis(10),
                || {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
            )
            .await;
            assert!(tasks.iter().all(|task| task.handle.is_none()));
            outcome
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            !cleanup.is_finished(),
            "must-join owner must not be detached"
        );
        release_tx.send(()).unwrap();
        assert!(cleanup.await.unwrap().must_join_exceeded_threshold);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 1);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn durability_owners_are_must_join_and_network_inputs_are_abort_safe() {
        assert_eq!(input_task_kind("journal"), TaskKind::MustJoin);
        assert_eq!(input_task_kind("syslog_udp"), TaskKind::AbortSafe);
        assert_eq!(pipeline_task_kind(false, true), TaskKind::MustJoin);
        assert_eq!(pipeline_task_kind(false, false), TaskKind::AbortSafe);
        assert_eq!(
            output_task_kind("file", &queue::QueueType::Memory, false, false),
            TaskKind::MustJoin
        );
        assert_eq!(
            output_task_kind("syslog_udp", &queue::QueueType::Memory, false, false),
            TaskKind::AbortSafe
        );
    }
}

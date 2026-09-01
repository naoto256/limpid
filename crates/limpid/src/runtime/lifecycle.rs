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
    if input_type == "journal" {
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
    let _ = shutdown_tx.send(true);
    let started = tokio::time::Instant::now();
    let overall_deadline = started + total_timeout;
    let abort_reserve = ABORT_JOIN_RESERVE.min(total_timeout / 2);
    let graceful_deadline = overall_deadline - abort_reserve;

    for task in handles.iter_mut() {
        let Some(handle) = task.handle.as_mut() else {
            continue;
        };
        match tokio::time::timeout_at(graceful_deadline, handle).await {
            Ok(result) => {
                record_join_result(result);
                task.handle = None;
            }
            Err(_) => break,
        }
    }

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

    for task in handles.iter_mut() {
        if task.kind != TaskKind::AbortSafe {
            continue;
        }
        let Some(handle) = task.handle.as_mut() else {
            continue;
        };
        match tokio::time::timeout_at(overall_deadline, handle).await {
            Ok(result) => {
                record_join_result(result);
                task.handle = None;
            }
            Err(_) => break,
        }
    }

    // Consume MustJoin handles that completed by the health threshold before
    // deciding whether the threshold was exceeded. This also preserves the
    // one-poll ownership invariant for every completed handle.
    for task in handles.iter_mut() {
        if task.kind == TaskKind::MustJoin
            && task
                .handle
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
            && let Some(handle) = task.handle.take()
        {
            record_join_result(handle.await);
        }
    }

    let abort_safe_incomplete = handles
        .iter()
        .filter(|task| task.kind == TaskKind::AbortSafe && task.handle.is_some())
        .count();
    let must_join_exceeded_threshold = handles
        .iter()
        .any(|task| task.kind == TaskKind::MustJoin && task.handle.is_some());

    // Must-join tasks own durability or disposition state. The 10-second
    // deadline is a health threshold for them, never permission to abort or
    // detach. Await every remaining owner before returning.
    for task in handles.iter_mut() {
        if task.kind != TaskKind::MustJoin {
            continue;
        }
        if let Some(handle) = task.handle.as_mut() {
            record_join_result(handle.await);
            task.handle = None;
        }
    }

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
        info!(
            "initiating graceful shutdown (timeout: {}s)",
            SHUTDOWN_TIMEOUT.as_secs()
        );
        let mut handles = self.handles;
        let cleanup = shutdown_tasks(&self.shutdown_tx, &mut handles).await;
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
    use super::*;

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

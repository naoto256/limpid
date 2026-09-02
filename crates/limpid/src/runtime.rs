//! Daemon runtime: wires inputs, pipelines, output queues, and outputs
//! into a running system.
//!
//! Runtime does NOT count metrics — each component counts its own.
//! Runtime distributes one shared metrics registry to every component.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::control::ControlServer;
use crate::dsl::ast::*;
use crate::dsl::props;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::{LtpMetrics, PipelineMetrics, Registry};
use crate::modules::{self, HasMetrics, Module, ModuleRegistry};
use crate::pipeline::CompiledConfig;
use crate::queue::{self, QueueConfig, QueueSender};
use crate::tap::TapRegistry;

mod lifecycle;
use lifecycle::*;

pub struct Runtime {
    shutdown_tx: watch::Sender<bool>,
    handles: Vec<TrackedTask>,
    config_file: PathBuf,
    blueprint: Arc<crate::pipeline::RuntimeBlueprint>,
    #[cfg(test)]
    test_identity: RuntimeTestIdentity,
}

#[cfg(test)]
struct RuntimeTestIdentity {
    metrics_registry: Arc<Registry>,
    funcs: Arc<FunctionRegistry>,
    tap: TapRegistry,
}

#[cfg(test)]
tokio::task_local! {
    static LATE_POST_LISTENER_FAILURE: std::cell::Cell<bool>;
    static DROP_CLEANUP_RESULT: Arc<DropCleanupObserver>;
    static POST_OUTPUT_PREACTIVATION_FAILURE: std::cell::RefCell<Option<PostOutputFailure>>;
    static STARTUP_TASK_COMPLETIONS: Arc<StartupTaskCompletionObserver>;
}

#[cfg(test)]
#[derive(Default)]
struct DropCleanupObserver {
    outcome: std::sync::Mutex<Option<CleanupOutcome>>,
    completed: tokio::sync::Notify,
}

#[cfg(test)]
impl DropCleanupObserver {
    fn complete(&self, outcome: CleanupOutcome) {
        *self.outcome.lock().expect("drop cleanup observer") = Some(outcome);
        self.completed.notify_one();
    }

    async fn wait(&self) -> CleanupOutcome {
        loop {
            if let Some(outcome) = *self.outcome.lock().expect("drop cleanup observer") {
                return outcome;
            }
            self.completed.notified().await;
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct StartupTaskCompletionObserver {
    output_queues: std::sync::atomic::AtomicUsize,
    pipelines: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
enum PostOutputFailureMode {
    Error,
    Park,
}

#[cfg(test)]
struct PostOutputFailure {
    mode: PostOutputFailureMode,
    reached: Arc<tokio::sync::Notify>,
    enqueue_probe: bool,
}

mod startup;
pub(crate) use startup::init_tables;

mod pipeline_worker;
use pipeline_worker::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;
    use crate::event::Event;
    use crate::metrics::{MetricsError, OutputMetrics, Registry};
    use crate::modules::Output;
    use crate::queue::{QueueAckHandle, QueueType};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::str::FromStr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn input_queue_timer(name: &str) -> crate::metrics::InputQueueTimer {
        crate::metrics::InputQueueTimer::register(&Registry::new(), name)
            .expect("test input queue timer must register")
    }

    fn full_stdout_pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: pipe initializes both integers on success.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: successful pipe returned two uniquely-owned descriptors.
        let (read_fd, write_fd) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let flags = unsafe { libc::fcntl(write_fd.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe {
                libc::fcntl(
                    write_fd.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            },
            -1
        );
        let chunk = [b'x'; 8192];
        loop {
            // SAFETY: descriptor and chunk are live for write(2).
            let written =
                unsafe { libc::write(write_fd.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
            if written == -1 {
                let error = std::io::Error::last_os_error();
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                break;
            }
        }
        (read_fd, write_fd)
    }

    fn assert_output_duplicate(error: &MetricsError, label_value: &str, diagnostic: &str) {
        let (name, labelset) = match error {
            MetricsError::DuplicateSeries { name, labelset } => (name, labelset),
            other => panic!("expected DuplicateSeries, got {other:?}"),
        };
        assert!(
            [
                "limpid_output_events_received_total",
                "limpid_output_events_injected_total",
                "limpid_output_events_written_total",
                "limpid_output_events_failed_total",
                "limpid_output_retries_total",
                "limpid_output_events_wedged_total",
                "limpid_output_events_errored_unwritable_total",
            ]
            .contains(&name.as_str())
        );
        assert_eq!(labelset, &[("output".to_owned(), label_value.to_owned())]);
        assert!(diagnostic.contains(&format!("name={name:?}")));
        assert!(diagnostic.contains(&format!("labelset={labelset:?}")));
    }

    fn compiled_config(src: &str) -> CompiledConfig {
        CompiledConfig::from_config(parse_config(src).expect("parse config"))
            .expect("compile config")
    }

    fn bound_blueprint(config: &CompiledConfig) -> Arc<crate::pipeline::BoundRuntimeBlueprint> {
        bound_blueprint_with_registry(config, &Registry::new())
    }

    fn bound_blueprint_with_registry(
        config: &CompiledConfig,
        registry: &Registry,
    ) -> Arc<crate::pipeline::BoundRuntimeBlueprint> {
        Arc::new(
            crate::pipeline::compile_runtime_blueprint(config)
                .expect("compile test blueprint")
                .bind(registry)
                .expect("bind test blueprint"),
        )
    }

    fn runtime_test_identity() -> RuntimeTestIdentity {
        RuntimeTestIdentity {
            metrics_registry: Arc::new(Registry::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
        }
    }

    struct ShutdownCountingOutput {
        metrics: Arc<crate::metrics::OutputMetrics>,
        shutdown_calls: std::sync::atomic::AtomicUsize,
        resolved: std::sync::atomic::AtomicUsize,
        parked: std::sync::Mutex<Vec<QueueAckHandle>>,
        consumed: tokio::sync::Notify,
    }

    impl ShutdownCountingOutput {
        fn new() -> Self {
            Self {
                metrics: crate::metrics::OutputMetrics::for_testing(),
                shutdown_calls: std::sync::atomic::AtomicUsize::new(0),
                resolved: std::sync::atomic::AtomicUsize::new(0),
                parked: std::sync::Mutex::new(Vec::new()),
                consumed: tokio::sync::Notify::new(),
            }
        }
    }

    impl HasMetrics for ShutdownCountingOutput {
        type Stats = crate::metrics::OutputMetrics;

        fn metrics(&self) -> Arc<Self::Stats> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for ShutdownCountingOutput {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> Result<()> {
            self.parked.lock().unwrap().push(ack);
            self.consumed.notify_one();
            Ok(())
        }

        async fn consume_shutdown(&self, _event: &Event, ack: QueueAckHandle) -> Result<()> {
            self.parked.lock().unwrap().push(ack);
            self.consumed.notify_one();
            Ok(())
        }

        async fn shutdown(
            &self,
            _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
        ) -> Result<()> {
            self.shutdown_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for ack in self.parked.lock().unwrap().drain(..) {
                ack.resolve_delivered();
                self.resolved
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn startup_rollback_calls_output_shutdown_once_and_resolves_parked_ack() {
        let (mut sender, receiver) = queue::create_queue(
            "rollback-output".to_owned(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        let writer = Arc::new(ShutdownCountingOutput::new());
        sender.attach_metrics(writer.metrics());
        sender
            .send(crate::event::QueuedEvent::new(
                Event::new(
                    bytes::Bytes::from_static(b"parked"),
                    "127.0.0.1:1".parse().unwrap(),
                ),
                crate::time::UnixNanos::now(),
            ))
            .await
            .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut guard = StartupGuard::new(shutdown_tx);
        let metrics = writer.metrics();
        let writer_for_task: Arc<dyn Output> = writer.clone();
        guard.track(
            TaskKind::MustJoin,
            tokio::spawn(async move {
                queue::run_queue_consumer(
                    receiver,
                    writer_for_task,
                    None,
                    metrics,
                    None,
                    shutdown_rx,
                )
                .await;
            }),
        );
        writer.consumed.notified().await;

        let error = guard.rollback(anyhow::anyhow!("late failure")).await;
        assert!(error.to_string().contains("rolled back"));
        assert_eq!(
            writer
                .shutdown_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
        );
        assert!(writer.parked.lock().unwrap().is_empty());
        assert_eq!(writer.resolved.load(std::sync::atomic::Ordering::SeqCst), 1,);
        drop(sender);
    }

    #[tokio::test]
    async fn startup_success_regression_delivers_and_shuts_output_down_once() {
        let (mut sender, receiver) = queue::create_queue(
            "success-output".to_owned(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        let writer = Arc::new(ShutdownCountingOutput::new());
        sender.attach_metrics(writer.metrics());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut guard = StartupGuard::new(shutdown_tx);
        let metrics = writer.metrics();
        let writer_for_task: Arc<dyn Output> = writer.clone();
        guard.track(
            TaskKind::MustJoin,
            tokio::spawn(async move {
                queue::run_queue_consumer(
                    receiver,
                    writer_for_task,
                    None,
                    metrics,
                    None,
                    shutdown_rx,
                )
                .await;
            }),
        );
        let (shutdown_tx, handles) = guard.commit();
        let runtime = Runtime {
            shutdown_tx,
            handles,
            config_file: PathBuf::from("success-test.limpid"),
            blueprint: crate::pipeline::compile_runtime_blueprint(&compiled_config(""))
                .expect("compile empty blueprint"),
            test_identity: runtime_test_identity(),
        };

        sender
            .send(crate::event::QueuedEvent::new(
                Event::new(
                    bytes::Bytes::from_static(b"delivered"),
                    "127.0.0.1:1".parse().unwrap(),
                ),
                crate::time::UnixNanos::now(),
            ))
            .await
            .unwrap();
        writer.consumed.notified().await;
        runtime.shutdown().await;

        assert_eq!(
            writer
                .shutdown_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
        );
        assert_eq!(writer.resolved.load(std::sync::atomic::Ordering::SeqCst), 1,);
        assert!(writer.parked.lock().unwrap().is_empty());
        drop(sender);
    }

    #[tokio::test]
    async fn cancelled_startup_guard_transfers_bounded_cleanup_to_runtime() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let cleanup_observer = Arc::new(DropCleanupObserver::default());
        DROP_CLEANUP_RESULT
            .scope(Arc::clone(&cleanup_observer), async move {
                let mut guard = StartupGuard::new(shutdown_tx);
                guard.track(
                    TaskKind::AbortSafe,
                    tokio::spawn(async move {
                        let terminal = shutdown_change_is_terminal(&mut shutdown_rx).await;
                        let _ = done_tx.send(terminal);
                    }),
                );
                drop(guard);
            })
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), done_rx)
                .await
                .expect("drop cleanup must not orphan the tracked task")
                .expect("tracked task must report terminal shutdown")
        );
        let cleanup = tokio::time::timeout(Duration::from_millis(100), cleanup_observer.wait())
            .await
            .expect("drop cleanup task must finish");
        assert_eq!(cleanup.abort_safe_incomplete, 0);
    }

    #[tokio::test]
    async fn startup_guard_dropped_on_external_thread_uses_captured_executor() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let owners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cleanup_observer = Arc::new(DropCleanupObserver::default());
        let guard = DROP_CLEANUP_RESULT
            .scope(Arc::clone(&cleanup_observer), async {
                let mut guard = StartupGuard::new(shutdown_tx);
                let owners_in_task = Arc::clone(&owners);
                guard.track(
                    TaskKind::AbortSafe,
                    tokio::spawn(async move {
                        struct Owner(Arc<std::sync::atomic::AtomicUsize>);
                        impl Drop for Owner {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        owners_in_task.fetch_add(1, Ordering::SeqCst);
                        let _owner = Owner(owners_in_task);
                        let _ = shutdown_change_is_terminal(&mut shutdown_rx).await;
                    }),
                );
                tokio::task::yield_now().await;
                guard
            })
            .await;
        assert_eq!(owners.load(Ordering::SeqCst), 1);

        std::thread::spawn(move || drop(guard)).join().unwrap();
        let cleanup = tokio::time::timeout(Duration::from_secs(1), cleanup_observer.wait())
            .await
            .expect("captured runtime must complete externally-triggered cleanup");
        assert_eq!(cleanup.abort_safe_incomplete, 0);
        assert_eq!(owners.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cleanup_consumes_completed_handles_once_and_aborts_only_pending_tasks() {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct EndOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for EndOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let first = tokio::spawn(async {});
        let ended_in_task = Arc::clone(&ended);
        let blocked = tokio::spawn(async move {
            let _reservation = reservation;
            let _end = EndOnDrop(ended_in_task);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let mut handles = vec![
            TrackedTask {
                kind: TaskKind::AbortSafe,
                handle: Some(first),
            },
            TrackedTask {
                kind: TaskKind::AbortSafe,
                handle: Some(blocked),
            },
        ];
        let cleanup =
            shutdown_tasks_with_timeout(&shutdown_tx, &mut handles, Duration::from_millis(80))
                .await;

        assert!(cleanup.forced_abort);
        assert_eq!(cleanup.abort_safe_incomplete, 0);
        assert!(handles.iter().all(|task| task.handle.is_none()));
        assert!(ended.load(Ordering::SeqCst));
        std::net::TcpListener::bind(address)
            .expect("aborted cooperative owner must release its socket before return");
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_health_threshold_never_detaches_or_aborts_must_join_task() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_in_task = Arc::clone(&finished);
        let handle = tokio::spawn(async move {
            let _ = release_rx.await;
            finished_in_task.store(true, Ordering::SeqCst);
        });
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let mut guard = StartupGuard::new(shutdown_tx);
        guard.track(TaskKind::MustJoin, handle);

        let rollback = tokio::spawn(async move {
            guard
                .rollback_with_timeout(
                    anyhow::anyhow!("original startup error"),
                    Duration::from_millis(50),
                )
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        assert!(!rollback.is_finished());
        assert!(!finished.load(Ordering::SeqCst));

        release_tx.send(()).unwrap();
        let error = rollback.await.unwrap();
        let chain = format!("{error:#}");
        assert!(chain.contains("original startup error"), "{chain}");
        assert!(chain.contains("health threshold"), "{chain}");
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn durability_owners_are_must_join_and_private_network_tasks_are_abort_safe() {
        let memory = QueueType::Memory;
        let disk = QueueType::Disk {
            path: "/tmp/test-wal".to_owned(),
            max_size: 1024,
        };
        assert_eq!(
            output_task_kind("file", &memory, false, false),
            TaskKind::MustJoin
        );
        assert_eq!(
            output_task_kind("stdout", &disk, false, false),
            TaskKind::MustJoin
        );
        assert_eq!(
            output_task_kind("stdout", &memory, true, false),
            TaskKind::MustJoin
        );
        assert_eq!(
            output_task_kind("stdout", &memory, false, true),
            TaskKind::MustJoin
        );
        assert_eq!(pipeline_task_kind(true, false), TaskKind::MustJoin);
        assert_eq!(pipeline_task_kind(false, true), TaskKind::MustJoin);
        assert_eq!(input_task_kind("journal"), TaskKind::MustJoin);

        assert_eq!(
            output_task_kind("stdout", &memory, false, false),
            TaskKind::AbortSafe
        );
        assert_eq!(
            output_task_kind("syslog_tcp", &memory, false, false),
            TaskKind::AbortSafe
        );
        assert_eq!(pipeline_task_kind(false, false), TaskKind::AbortSafe);
        assert_eq!(input_task_kind("syslog_tcp"), TaskKind::AbortSafe);
    }

    #[tokio::test]
    async fn startup_rollback_with_full_stdout_pipe_resolves_current_ack() {
        let (read_fd, write_fd) = full_stdout_pipe();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.shutdown_signal = shutdown_rx;
        let properties =
            crate::dsl::module_props::ModuleProperties::from_parts("stdout", Vec::new());
        let output = Arc::new(
            crate::modules::output::stdout::StdoutOutput::from_properties(
                "rollback-stdout",
                &properties,
                &ctx,
            )
            .unwrap(),
        );
        let event = Event::new(
            Bytes::from_static(b"blocked-rollback"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let mut guard = StartupGuard::new(shutdown_tx);
        guard.track(
            TaskKind::AbortSafe,
            tokio::spawn(async move {
                crate::modules::output::stdout::with_test_stdout_fd(write_fd, async move {
                    output.consume(&event, ack).await.unwrap();
                })
                .await
                .unwrap();
            }),
        );
        tokio::task::yield_now().await;

        let error = guard
            .rollback(anyhow::anyhow!("late startup failure"))
            .await;
        assert!(format!("{error:#}").contains("late startup failure"));
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        drop(read_fd);
    }

    #[tokio::test]
    async fn normal_shutdown_with_full_stdout_pipe_resolves_current_ack() {
        let (read_fd, write_fd) = full_stdout_pipe();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.shutdown_signal = shutdown_rx;
        let properties =
            crate::dsl::module_props::ModuleProperties::from_parts("stdout", Vec::new());
        let output = Arc::new(
            crate::modules::output::stdout::StdoutOutput::from_properties(
                "shutdown-stdout",
                &properties,
                &ctx,
            )
            .unwrap(),
        );
        let event = Event::new(
            Bytes::from_static(b"blocked-shutdown"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let handle = tokio::spawn(async move {
            crate::modules::output::stdout::with_test_stdout_fd(write_fd, async move {
                output.consume(&event, ack).await.unwrap();
            })
            .await
            .unwrap();
        });
        let runtime = Runtime {
            shutdown_tx,
            handles: vec![TrackedTask {
                kind: TaskKind::AbortSafe,
                handle: Some(handle),
            }],
            config_file: PathBuf::from("stdout-shutdown-test.limpid"),
            blueprint: crate::pipeline::compile_runtime_blueprint(&compiled_config(""))
                .expect("compile empty blueprint"),
            test_identity: runtime_test_identity(),
        };
        tokio::task::yield_now().await;

        runtime.shutdown().await;
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        drop(read_fd);
    }

    #[tokio::test]
    async fn full_pipe_queue_consumer_is_bounded_and_disposes_once_on_abort() {
        let (read_fd, write_fd) = full_stdout_pipe();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.shutdown_signal = shutdown_rx.clone();
        let properties =
            crate::dsl::module_props::ModuleProperties::from_parts("stdout", Vec::new());
        let output = Arc::new(
            crate::modules::output::stdout::StdoutOutput::from_properties(
                "shutdown-drain-full-pipe",
                &properties,
                &ctx,
            )
            .unwrap(),
        );
        let (sender, receiver) = queue::create_queue(
            "shutdown-drain-full-pipe".to_string(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 1,
            },
        )
        .unwrap();
        sender
            .send(crate::event::QueuedEvent::new(
                Event::new(
                    Bytes::from_static(b"blocked-drain"),
                    "127.0.0.1:0".parse().unwrap(),
                ),
                crate::time::UnixNanos::now(),
            ))
            .await
            .unwrap();
        drop(sender);
        shutdown_tx.send(true).unwrap();

        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops_in_task = Arc::clone(&drops);
        let writer: Arc<dyn Output> = output.clone();
        let metrics = output.metrics();
        let handle = tokio::spawn(async move {
            crate::modules::output::stdout::with_test_stdout_fd(write_fd, async move {
                queue::with_test_implicit_drop_count(drops_in_task, async move {
                    queue::run_queue_consumer(receiver, writer, None, metrics, None, shutdown_rx)
                        .await;
                })
                .await;
            })
            .await
            .unwrap();
        });
        let mut guard = StartupGuard::new(shutdown_tx);
        guard.track(TaskKind::AbortSafe, handle);

        let started = std::time::Instant::now();
        let error = guard
            .rollback_with_timeout(
                anyhow::anyhow!("forced shutdown"),
                Duration::from_millis(100),
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{error:#}").contains("forced shutdown"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(output.metrics().events_written.load(Ordering::SeqCst), 0);
        drop(read_fd);
    }

    #[tokio::test(start_paused = true)]
    async fn disk_pipeline_wal_barrier_remains_must_join_past_ten_seconds() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);
        let wal = dir.path().join("queue");
        let config = compiled_config(&format!(
            r#"
control {{ socket {:?} }}
def input source {{ type syslog_udp bind "{addr}" }}
def output sink {{
    type file
    path {:?}
    queue {{ type disk path {:?} max_size "1MB" }}
}}
def pipeline p {{ input source; output sink }}
"#,
            dir.path().join("control.sock"),
            dir.path().join("delivered.log"),
            wal,
        ));
        let barrier = queue::TestWalBarrier::new();
        let completions = Arc::new(StartupTaskCompletionObserver::default());
        let runtime = STARTUP_TASK_COMPLETIONS
            .scope(
                Arc::clone(&completions),
                queue::with_test_wal_barrier(
                    Arc::clone(&barrier),
                    Runtime::start(config, dir.path().join("wal-barrier.limpid")),
                ),
            )
            .await
            .unwrap();

        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"<13>wal-exact", addr).unwrap();
        barrier.wait_reached().await;

        let shutdown = tokio::spawn(runtime.shutdown());
        while completions.output_queues.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(
            !shutdown.is_finished(),
            "WAL producer must remain MustJoin after 10s"
        );

        barrier.release();
        shutdown.await.unwrap();

        let (_sender, mut receiver) = queue::create_queue(
            "wal-proof".to_string(),
            QueueConfig {
                queue_type: QueueType::Disk {
                    path: wal.to_string_lossy().into_owned(),
                    max_size: 1024 * 1024,
                },
                capacity: 1,
            },
        )
        .unwrap();
        let (persisted, _) = receiver
            .recv()
            .await
            .expect("released WAL write must persist");
        assert_eq!(&persisted.egress[..], b"<13>wal-exact");
        assert!(
            receiver.try_recv().is_none(),
            "WAL barrier must persist exactly once"
        );
    }

    fn batched_output_only_config(name: &str, control_socket: &Path) -> CompiledConfig {
        compiled_config(&format!(
            r#"
control {{ socket {control_socket:?} }}
def output {name} {{
    type http
    peer {{ url "http://127.0.0.1:1/" }}
    batch_size 100
}}
"#
        ))
    }

    #[tokio::test]
    async fn post_output_preactivation_error_shuts_real_batched_actor_once() {
        let output_name = "preactivation_error_batched";
        let observer = crate::modules::output::batched::observe_shutdown_for_testing(output_name);
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let reached = Arc::new(tokio::sync::Notify::new());
        let result = POST_OUTPUT_PREACTIVATION_FAILURE
            .scope(
                std::cell::RefCell::new(Some(PostOutputFailure {
                    mode: PostOutputFailureMode::Error,
                    reached,
                    enqueue_probe: true,
                })),
                Runtime::start(
                    batched_output_only_config(output_name, &dir.path().join("control.sock")),
                    PathBuf::from("preactivation-error.limpid"),
                ),
            )
            .await;
        let error = match result {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("failpoint must reject startup");
            }
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("post-output preactivation failure"),
            "{error:#}"
        );
        tokio::time::timeout(Duration::from_secs(2), observer.wait_for_completion())
            .await
            .expect("real batched output shutdown must complete");
        assert_eq!(observer.calls(), 1);
        assert_eq!(observer.completions(), 1);
        assert_eq!(observer.resolved_dispositions(), 1);
    }

    #[tokio::test]
    async fn post_output_preactivation_cancellation_completes_guard_and_batched_actor_cleanup() {
        let output_name = "preactivation_cancel_batched";
        let observer = crate::modules::output::batched::observe_shutdown_for_testing(output_name);
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let reached = Arc::new(tokio::sync::Notify::new());
        let reached_for_task = Arc::clone(&reached);
        let cleanup_observer = Arc::new(DropCleanupObserver::default());
        let startup = tokio::spawn(DROP_CLEANUP_RESULT.scope(
            Arc::clone(&cleanup_observer),
            POST_OUTPUT_PREACTIVATION_FAILURE.scope(
                std::cell::RefCell::new(Some(PostOutputFailure {
                    mode: PostOutputFailureMode::Park,
                    reached: reached_for_task,
                    enqueue_probe: true,
                })),
                Runtime::start(
                    batched_output_only_config(output_name, &dir.path().join("control.sock")),
                    PathBuf::from("preactivation-cancel.limpid"),
                ),
            ),
        ));

        reached.notified().await;
        startup.abort();
        let cancellation = startup.await;
        assert!(matches!(cancellation, Err(error) if error.is_cancelled()));
        let cleanup = tokio::time::timeout(Duration::from_secs(2), cleanup_observer.wait())
            .await
            .expect("drop fallback cleanup must finish");
        assert_eq!(cleanup.abort_safe_incomplete, 0);
        tokio::time::timeout(Duration::from_secs(2), observer.wait_for_completion())
            .await
            .expect("real batched output actor must complete after cancellation");
        assert_eq!(observer.calls(), 1);
        assert_eq!(observer.completions(), 1);
        assert_eq!(observer.resolved_dispositions(), 1);
    }

    #[cfg(unix)]
    fn write_ltp_test_identity(path: &Path) -> String {
        use base64::Engine as _;
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair as _};
        use std::os::unix::fs::PermissionsExt as _;

        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        std::fs::write(
            path,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        base64::engine::general_purpose::STANDARD.encode(spki)
    }

    #[cfg(unix)]
    async fn runtime_control_command(socket: &Path, command: &str) -> String {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

        let mut stream = tokio::net::UnixStream::connect(socket)
            .await
            .expect("connect runtime control socket");
        stream
            .write_all(format!("{command}\n").as_bytes())
            .await
            .expect("write runtime control command");
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .expect("read runtime control response");
        response
    }

    #[cfg(unix)]
    async fn assert_late_failure_scenario() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let node_key = dir.path().join("node.pem");
        let peer_key = dir.path().join("peer.pem");
        let _node_spki = write_ltp_test_identity(&node_key);
        let peer_spki = write_ltp_test_identity(&peer_key);
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = reservation.local_addr().unwrap();
        drop(reservation);
        let control_socket = dir.path().join("control.sock");
        let delivered = dir.path().join("delivered.log");
        let source = format!(
            r#"
node_id "node"
node_key {node_key:?}
control {{ socket {control_socket:?} }}
def input inbound {{
    type ltp
    bind "{bind}"
    peer {{ node_id "peer" pubkey {peer_spki:?} }}
}}
def output delivered {{ type file path {delivered:?} }}
def pipeline receive {{ input inbound; output delivered }}
"#,
        );

        // Establish and stop the old runtime first: this is the reload shape
        // whose same-port restoration must remain possible after a candidate
        // fails late in startup.
        let old_blueprint = crate::pipeline::compile_runtime_blueprint(&compiled_config(&source))
            .expect("compile rollback blueprint");
        let old =
            Runtime::start_blueprint(Arc::clone(&old_blueprint), dir.path().join("old.limpid"))
                .await
                .unwrap();
        let old_list = runtime_control_command(&control_socket, "list").await;
        assert!(old_list.contains("\"name\":\"receive\""), "{old_list}");
        assert!(
            old.test_identity
                .tap
                .subscribe("input inbound")
                .await
                .is_some()
        );
        assert!(
            old.test_identity
                .tap
                .subscribe("output delivered")
                .await
                .is_some()
        );
        assert!(
            old.test_identity
                .tap
                .subscribe("input candidate")
                .await
                .is_none()
        );
        let old_metrics = Arc::clone(&old.test_identity.metrics_registry);
        let old_funcs = Arc::clone(&old.test_identity.funcs);
        old_metrics
            .counter("limpid_test_old_runtime_marker_total")
            .help("Test-only marker proving rollback registry replacement.")
            .build()
            .expect("register old runtime marker")
            .inc();
        old.shutdown().await;

        let candidate_source =
            source.replace("def pipeline receive", "def pipeline candidate_receive");
        let candidate_blueprint =
            crate::pipeline::compile_runtime_blueprint(&compiled_config(&candidate_source))
                .expect("compile distinct candidate blueprint");
        assert!(!Arc::ptr_eq(&candidate_blueprint, &old_blueprint));

        let completions = Arc::new(StartupTaskCompletionObserver::default());
        let candidate = STARTUP_TASK_COMPLETIONS
            .scope(
                Arc::clone(&completions),
                LATE_POST_LISTENER_FAILURE.scope(
                    std::cell::Cell::new(true),
                    Runtime::start_blueprint(
                        candidate_blueprint,
                        dir.path().join("candidate.limpid"),
                    ),
                ),
            )
            .await;
        let error = match candidate {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("late failpoint must reject candidate");
            }
            Err(error) => error,
        };
        let chain = format!("{error:#}");
        assert!(
            chain.contains("late post-listener startup failure"),
            "{chain}"
        );
        assert!(chain.contains("rolled back all started tasks"), "{chain}");
        assert_eq!(completions.output_queues.load(Ordering::SeqCst), 1);
        assert_eq!(completions.pipelines.load(Ordering::SeqCst), 1);

        let probe = std::net::TcpListener::bind(bind)
            .expect("candidate rollback must release the real LTP listener immediately");
        drop(probe);

        let restored = Runtime::start_blueprint(
            Arc::clone(&old_blueprint),
            dir.path().join("restored.limpid"),
        )
        .await
        .expect("old runtime must restore on the same port after candidate rollback");
        assert!(Arc::ptr_eq(&restored.blueprint(), &old_blueprint));
        assert!(!Arc::ptr_eq(
            &restored.test_identity.metrics_registry,
            &old_metrics
        ));
        assert!(!Arc::ptr_eq(&restored.test_identity.funcs, &old_funcs));
        let restored_snapshot =
            serde_json::to_string(&restored.test_identity.metrics_registry.snapshot())
                .expect("serialize restored registry");
        assert!(
            !restored_snapshot.contains("limpid_test_old_runtime_marker_total"),
            "rollback reused the old counter registry: {restored_snapshot}"
        );
        assert_eq!(
            runtime_control_command(&control_socket, "list").await,
            old_list,
            "rollback changed old blueprint control flow JSON"
        );
        assert!(
            restored
                .test_identity
                .tap
                .subscribe("input inbound")
                .await
                .is_some()
        );
        assert!(
            restored
                .test_identity
                .tap
                .subscribe("output delivered")
                .await
                .is_some()
        );
        restored.shutdown().await;
        std::net::TcpListener::bind(bind)
            .expect("normal shutdown must leave no orphan listener or runtime task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn late_post_listener_failure_rolls_back_all_started_tasks() {
        assert_late_failure_scenario().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_candidate_releases_same_port_before_old_restore() {
        assert_late_failure_scenario().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_error_leaves_no_orphan_socket_or_task() {
        assert_late_failure_scenario().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_runtimes_deliver_one_event_over_mutual_rpk_ltp() {
        use std::os::unix::fs::PermissionsExt as _;
        use tokio::io::AsyncWriteExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let node_a_key = dir.path().join("node-a.pem");
        let node_b_key = dir.path().join("node-b.pem");
        let node_a_spki = write_ltp_test_identity(&node_a_key);
        let node_b_spki = write_ltp_test_identity(&node_b_key);
        let delivered_path = dir.path().join("delivered.log");
        let node_a_socket = dir.path().join("node-a.sock");
        let node_b_socket = dir.path().join("node-b.sock");

        let ltp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ltp_addr = ltp_listener.local_addr().unwrap();
        drop(ltp_listener);
        let node_b_config = compiled_config(&format!(
            r#"
node_id "node-b"
node_key {node_b_key:?}
control {{ socket {node_b_socket:?} }}
def input from_a {{
    type ltp
    bind "{ltp_addr}"
    peer {{ node_id "node-a" pubkey {node_a_spki:?} }}
}}
def output delivered {{ type file path {delivered_path:?} }}
def pipeline receive {{ input from_a; output delivered }}
"#
        ));
        let node_b = Runtime::start(node_b_config, dir.path().join("node-b.limpid"))
            .await
            .unwrap();

        let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();
        drop(tcp_listener);
        let node_a_config = compiled_config(&format!(
            r#"
node_id "node-a"
node_key {node_a_key:?}
control {{ socket {node_a_socket:?} }}
def input source {{ type syslog_tcp bind "{tcp_addr}" }}
def output to_b {{
    type ltp
    peer {{ node_id "node-b" pubkey {node_b_spki:?} endpoint "{ltp_addr}" }}
}}
def pipeline relay {{ input source; output to_b }}
"#
        ));
        let node_a = Runtime::start(node_a_config, dir.path().join("node-a.limpid"))
            .await
            .unwrap();

        let marker = b"<13>two-runtime-mutual-rpk";
        let mut frame = marker.to_vec();
        frame.push(b'\n');
        let mut sender = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tokio::net::TcpStream::connect(tcp_addr).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                }
            }
        })
        .await
        .expect("syslog TCP input did not become ready");
        sender.write_all(&frame).await.unwrap();
        sender.flush().await.unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(bytes) = tokio::fs::read(&delivered_path).await
                    && bytes == frame
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;

        node_a.shutdown().await;
        node_b.shutdown().await;
        delivered.expect("two-daemon LTP delivery timed out");
        assert_eq!(tokio::fs::read(&delivered_path).await.unwrap(), frame);
    }

    fn metric_series(registry: &Registry, name: &str) -> Vec<serde_json::Value> {
        let snapshot = serde_json::to_value(registry.snapshot()).expect("serialize snapshot");
        snapshot["metrics"]
            .as_array()
            .expect("metrics array")
            .iter()
            .find(|family| family["name"] == name)
            .unwrap_or_else(|| panic!("missing metric family {name}"))["series"]
            .as_array()
            .expect("series array")
            .clone()
    }

    fn series_value(registry: &Registry, family: &str, labels: &[(&str, &str)]) -> u64 {
        let expected: serde_json::Map<String, serde_json::Value> = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::json!(value)))
            .collect();
        metric_series(registry, family)
            .into_iter()
            .find(|series| series["labels"].as_object() == Some(&expected))
            .unwrap_or_else(|| panic!("missing {family} series for {expected:?}"))["value"]
            .as_u64()
            .expect("counter value")
    }

    #[tokio::test]
    async fn startup_preserves_metric_registration_errors_from_real_factories() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let config = CompiledConfig::from_config(
            parse_config(&format!(
                "control {{ socket {:?} }} def output conflicting {{ type stdout }}",
                dir.path().join("control.sock")
            ))
            .expect("parse"),
        )
        .expect("compile");
        let registry = Arc::new(Registry::new());
        OutputMetrics::register(&registry, "conflicting")
            .expect("preseeded output metrics must register");

        let error = match Runtime::start_with_registry(
            config,
            PathBuf::from("metrics-conflict-test.limpid"),
            registry,
        )
        .await
        {
            Ok(_) => panic!("daemon startup unexpectedly swallowed the registration conflict"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        let metrics_error = error
            .chain()
            .find_map(|source| source.downcast_ref::<MetricsError>())
            .unwrap_or_else(|| {
                panic!(
                    "MetricsError must remain downcastable in the startup error chain: {error:#}"
                )
            });
        assert_output_duplicate(metrics_error, "conflicting", &diagnostic);
    }

    #[tokio::test]
    async fn public_start_delegates_to_the_registry_wired_startup_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let source = format!("control {{ socket {:?} }}", socket.display().to_string());
        let config =
            CompiledConfig::from_config(parse_config(&source).expect("parse")).expect("compile");

        let runtime = Runtime::start(config, PathBuf::from("public-start-test.limpid"))
            .await
            .expect("public start must use the working registry-wired startup path");
        runtime.shutdown().await;
    }

    fn listener_startup_config(
        input_body: &str,
        control_socket: &Path,
        output_path: &Path,
    ) -> CompiledConfig {
        compiled_config(&format!(
            r#"
control {{ socket {control_socket:?} }}
def input source {{ {input_body} }}
def output sink {{ type file path {output_path:?} }}
def pipeline p {{ input source; output sink }}
"#
        ))
    }

    async fn runtime_start_error(config: CompiledConfig, path: PathBuf) -> anyhow::Error {
        match Runtime::start(config, path).await {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("runtime startup unexpectedly succeeded")
            }
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn occupied_syslog_tcp_and_udp_fail_runtime_start_before_commit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp.local_addr().unwrap();
        let tcp_error = runtime_start_error(
            listener_startup_config(
                &format!("type syslog_tcp bind \"{tcp_addr}\""),
                &dir.path().join("tcp-control.sock"),
                &dir.path().join("tcp.log"),
            ),
            dir.path().join("tcp.limpid"),
        )
        .await;
        assert!(format!("{tcp_error:#}").contains("failed to start input 'source'"));

        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_addr = udp.local_addr().unwrap();
        let udp_error = runtime_start_error(
            listener_startup_config(
                &format!("type syslog_udp bind \"{udp_addr}\""),
                &dir.path().join("udp-control.sock"),
                &dir.path().join("udp.log"),
            ),
            dir.path().join("udp.limpid"),
        )
        .await;
        assert!(format!("{udp_error:#}").contains("failed to start input 'source'"));
    }

    #[tokio::test]
    async fn invalid_syslog_tls_and_unix_bind_fail_runtime_start_before_commit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        let tls_body = format!(
            "type syslog_tcp bind \"127.0.0.1:0\" tls {{ cert {:?} key {:?} }}",
            dir.path().join("missing-cert.pem"),
            dir.path().join("missing-key.pem")
        );
        let tls_error = runtime_start_error(
            listener_startup_config(
                &tls_body,
                &dir.path().join("tls-control.sock"),
                &dir.path().join("tls.log"),
            ),
            dir.path().join("tls.limpid"),
        )
        .await;
        assert!(format!("{tls_error:#}").contains("failed to start input 'source'"));

        let long_name = "s".repeat(140);
        let unix_path = dir.path().join(long_name);
        let unix_error = runtime_start_error(
            listener_startup_config(
                &format!("type unix_socket path {unix_path:?}"),
                &dir.path().join("unix-control.sock"),
                &dir.path().join("unix.log"),
            ),
            dir.path().join("unix.limpid"),
        )
        .await;
        assert!(format!("{unix_error:#}").contains("failed to start input 'source'"));
        assert!(!unix_path.exists());
    }

    #[tokio::test]
    async fn control_bind_failure_rolls_back_started_resources() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let control_path = dir.path().join("c".repeat(140));
        let config = compiled_config(&format!(
            "control {{ socket {control_path:?} }} def output sink {{ type file path {:?} }}",
            dir.path().join("sink.log")
        ));
        let error = runtime_start_error(config, dir.path().join("control-fail.limpid")).await;
        assert!(format!("{error:#}").contains("failed to bind"));
        assert!(!control_path.exists());
    }

    #[tokio::test]
    async fn occupied_control_socket_rejects_start_and_preserves_active_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let control_path = dir.path().join("control.sock");
        let active = std::os::unix::net::UnixListener::bind(&control_path).unwrap();
        let config = compiled_config(&format!(
            "control {{ socket {control_path:?} }} def output sink {{ type file path {:?} }}",
            dir.path().join("sink.log")
        ));

        let error = runtime_start_error(config, dir.path().join("occupied-control.limpid")).await;
        assert!(format!("{error:#}").contains("active listener"));
        std::os::unix::net::UnixStream::connect(&control_path)
            .expect("failed startup must not unlink the active owner's socket");

        drop(active);
        std::fs::remove_file(&control_path).unwrap();
        let rebound = std::os::unix::net::UnixListener::bind(&control_path)
            .expect("failed startup must permit immediate same-resource rebind");
        drop(rebound);
    }

    async fn assert_startup_build_info(
        configured_node_id: Option<&str>,
        expected_node_id: &str,
        expected_resolver_calls: usize,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let node_id = configured_node_id
            .map(|node_id| format!("node_id \"{node_id}\"\n"))
            .unwrap_or_default();
        let source = format!(
            "{node_id}control {{ socket {:?} }}",
            socket.display().to_string()
        );
        let config = compiled_config(&source);
        let registry = Arc::new(Registry::new());
        let resolver_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&resolver_calls);

        let runtime = Runtime::start_with_registry_and_node_id_resolver(
            config,
            PathBuf::from("build-info-startup-test.limpid"),
            Arc::clone(&registry),
            move || {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(format!("resolved-host-{call}"))
            },
        )
        .await
        .expect("runtime must start");

        let labels = [
            ("node_id", expected_node_id),
            ("version", env!("CARGO_PKG_VERSION")),
        ];
        assert_eq!(series_value(&registry, "limpid_build_info", &labels), 1);
        assert_eq!(metric_series(&registry, "limpid_build_info").len(), 1);
        assert_eq!(
            resolver_calls.load(Ordering::Relaxed),
            expected_resolver_calls,
            "startup must resolve hostname exactly when node_id is omitted"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn startup_with_explicit_node_id_skips_hostname_and_registers_that_value() {
        assert_startup_build_info(Some("configured-node"), "configured-node", 0).await;
    }

    #[tokio::test]
    async fn startup_without_node_id_resolves_hostname_once_and_registers_that_value() {
        assert_startup_build_info(None, "resolved-host-1", 1).await;
    }

    #[tokio::test]
    async fn startup_preflights_a_declared_node_key_and_ignores_an_omitted_one() {
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let key = dir.path().join("node-key.pem");
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate key");
        std::fs::write(
            &key,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .expect("write key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .expect("secure key mode");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut spki = crate::ltp::ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let peer_pubkey = base64::engine::general_purpose::STANDARD.encode(spki);

        let source = format!(
            "node_id \"node-a\"\nnode_key {:?}\ncontrol {{ socket {:?} }}\n\
             def output ltp_out {{ type ltp peer {{ node_id \"peer-a\" pubkey {:?} endpoint \"127.0.0.1:1\" }} }}",
            key.display().to_string(),
            socket.display().to_string(),
            peer_pubkey,
        );
        let runtime = Runtime::start(
            compiled_config(&source),
            PathBuf::from("node-key-startup-test.limpid"),
        )
        .await
        .expect("declared valid key must pass startup preflight");
        runtime.shutdown().await;

        let omitted_socket = dir.path().join("omitted-control.sock");
        let omitted = format!(
            "node_id \"node-a\"\ncontrol {{ socket {:?} }}",
            omitted_socket.display().to_string()
        );
        let runtime = Runtime::start(
            compiled_config(&omitted),
            PathBuf::from("missing-path-is-not-consulted.limpid"),
        )
        .await
        .expect("omitted node_key must not trigger filesystem preflight");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn startup_fails_before_tasks_when_a_declared_node_key_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
            .expect("secure control parent");
        let socket = dir.path().join("control.sock");
        let missing = dir.path().join("missing-node-key.pem");
        let source = format!(
            "node_id \"node-a\"\nnode_key {:?}\ncontrol {{ socket {:?} }}",
            missing.display().to_string(),
            socket.display().to_string()
        );

        let error = match Runtime::start(
            compiled_config(&source),
            PathBuf::from("node-key-failure-test.limpid"),
        )
        .await
        {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("declared missing node_key must fail startup")
            }
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("secure open failed"));
        assert!(
            !socket.exists(),
            "control task must not start before key preflight"
        );
    }

    #[tokio::test]
    async fn startup_propagates_duplicate_build_info_from_the_actual_registry() {
        let config = compiled_config("node_id \"configured-node\"");
        let registry = Arc::new(Registry::new());
        crate::metrics::register_build_info(
            &registry,
            env!("CARGO_PKG_VERSION"),
            "configured-node",
        )
        .expect("preseed build info");
        let resolver_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&resolver_calls);

        let error = match Runtime::start_with_registry_and_node_id_resolver(
            config,
            PathBuf::from("duplicate-build-info-startup-test.limpid"),
            Arc::clone(&registry),
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok("unexpected-hostname".to_owned())
            },
        )
        .await
        {
            Ok(runtime) => {
                runtime.shutdown().await;
                panic!("duplicate build-info registration must fail startup");
            }
            Err(error) => error,
        };

        let metrics_error = error
            .downcast_ref::<MetricsError>()
            .expect("startup error must retain MetricsError");
        let expected_labelset = vec![
            ("node_id".to_owned(), "configured-node".to_owned()),
            ("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        ];
        match metrics_error {
            MetricsError::DuplicateSeries { name, labelset } => {
                assert_eq!(name, "limpid_build_info");
                assert_eq!(labelset, &expected_labelset);
            }
            other => panic!("expected DuplicateSeries, got {other:?}"),
        }
        let diagnostic = error.to_string();
        assert!(
            diagnostic.contains(&format!("name={:?}", "limpid_build_info")),
            "diagnostic must identify the metric family: {diagnostic}"
        );
        assert!(
            diagnostic.contains(&format!("labelset={expected_labelset:?}")),
            "diagnostic must include the complete labelset: {diagnostic}"
        );
        assert_eq!(resolver_calls.load(Ordering::Relaxed), 0);
        assert_eq!(metric_series(&registry, "limpid_build_info").len(), 1);
    }

    /// End-to-end-ish fan-in runtime test: two independent mpsc channels
    /// (simulating two input sources) both push events into a dispatcher
    /// that shares a single `PipelineWorker`. Events from both sides land
    /// on the same pipeline — we verify via the worker's own metrics.
    #[tokio::test]
    async fn fan_in_merges_two_inputs_into_single_worker() {
        // Minimal pipeline with a single `drop` step; the body doesn't matter
        // for this test — we only care that events flow through the worker.
        let fan_in_config = compiled_config("def pipeline p { input a, b; drop }");
        let metrics_registry = Registry::new();
        let runtime_blueprint = bound_blueprint_with_registry(&fan_in_config, &metrics_registry);
        let worker = Arc::new(
            PipelineWorker::from_bound(
                runtime_blueprint.blueprint.pipeline_id("p").unwrap(),
                &runtime_blueprint,
                &metrics_registry,
            )
            .expect("pipeline metrics must register"),
        );
        let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(vec![Arc::clone(&worker)]);
        let worker_for_a = Arc::clone(&workers[0]);
        let worker_for_b = Arc::new(
            PipelineWorker::from_bound(
                runtime_blueprint.blueprint.pipeline_id("p").unwrap(),
                &runtime_blueprint,
                &Registry::new(),
            )
            .expect("a second fan-in worker must reuse the bound descriptor"),
        );
        assert!(Arc::ptr_eq(
            &worker_for_a.execution,
            &worker_for_b.execution,
        ));

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx_a, rx_a) = mpsc::channel::<Event>(16);
        let (tx_b, rx_b) = mpsc::channel::<Event>(16);

        let tap = TapRegistry::new();
        tap.register("input a").await;
        tap.register("input b").await;

        let disk_outputs = Arc::new(HashSet::new());
        let ctx_a = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::clone(&disk_outputs),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: tap.clone(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        let ctx_b = PipelineContext {
            output_senders: Arc::clone(&ctx_a.output_senders),
            disk_outputs: Arc::clone(&disk_outputs),
            funcs: Arc::clone(&ctx_a.funcs),
            tap: tap.clone(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };

        let workers_a = Arc::clone(&workers);
        let workers_b = Arc::clone(&workers);
        let sd_a = shutdown_rx.clone();
        let sd_b = shutdown_rx.clone();
        let h_a = tokio::spawn(async move {
            let timer = input_queue_timer("fan-in-a");
            run_pipeline_workers(rx_a, &workers_a, &ctx_a, "a", &timer, sd_a).await;
        });
        let h_b = tokio::spawn(async move {
            let timer = input_queue_timer("fan-in-b");
            run_pipeline_workers(rx_b, &workers_b, &ctx_b, "b", &timer, sd_b).await;
        });

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        for _ in 0..3 {
            tx_a.send(Event::new(Bytes::from_static(b"from_a"), addr))
                .await
                .unwrap();
        }
        for _ in 0..5 {
            tx_b.send(Event::new(Bytes::from_static(b"from_b"), addr))
                .await
                .unwrap();
        }
        drop(tx_a);
        drop(tx_b);

        // Wait for both dispatchers to drain (they exit when their senders drop).
        tokio::time::timeout(Duration::from_secs(2), async {
            let _ = h_a.await;
            let _ = h_b.await;
        })
        .await
        .expect("dispatchers should drain promptly");

        // All 8 events should have been attributed to the shared worker.
        assert_eq!(worker.metrics.events_received.load(Ordering::Relaxed), 8);
        assert_eq!(worker.metrics.events_dropped.load(Ordering::Relaxed), 8);
        assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn pipeline_inflight_counts_concurrent_runs_and_returns_to_zero() {
        let cfg = compiled_config("def pipeline p { input a, b; output sink; finish }");
        let metrics_registry = Registry::new();
        let runtime_blueprint = bound_blueprint_with_registry(&cfg, &metrics_registry);
        let worker = Arc::new(
            PipelineWorker::from_bound(
                runtime_blueprint.blueprint.pipeline_id("p").unwrap(),
                &runtime_blueprint,
                &metrics_registry,
            )
            .expect("pipeline metrics must register"),
        );
        let workers: Arc<Vec<Arc<PipelineWorker>>> = Arc::new(vec![Arc::clone(&worker)]);

        let (queue_sender, mut queue_receiver) = crate::queue::create_queue(
            "sink".to_owned(),
            crate::queue::QueueConfig {
                queue_type: crate::queue::QueueType::Memory,
                capacity: 1,
            },
        )
        .expect("memory queue");
        queue_sender
            .send(crate::event::QueuedEvent::new(
                Event::new(
                    Bytes::from_static(b"filler"),
                    "127.0.0.1:0".parse().unwrap(),
                ),
                crate::time::UnixNanos::now(),
            ))
            .await
            .expect("prefill queue");

        let tap = TapRegistry::new();
        tap.register("input a").await;
        tap.register("input b").await;
        let ctx = Arc::new(PipelineContext {
            output_senders: Arc::new(HashMap::from([("sink".to_owned(), queue_sender)])),
            disk_outputs: Arc::new(HashSet::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap,
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        });
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (tx_a, rx_a) = mpsc::channel(1);
        let (tx_b, rx_b) = mpsc::channel(1);
        let h_a = {
            let workers = Arc::clone(&workers);
            let ctx = Arc::clone(&ctx);
            let shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                let timer = input_queue_timer("blocked-a");
                run_pipeline_workers(rx_a, &workers, &ctx, "a", &timer, shutdown).await;
            })
        };
        let h_b = {
            let workers = Arc::clone(&workers);
            let ctx = Arc::clone(&ctx);
            tokio::spawn(async move {
                let timer = input_queue_timer("blocked-b");
                run_pipeline_workers(rx_b, &workers, &ctx, "b", &timer, shutdown_rx).await;
            })
        };

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        tx_a.send(Event::new(Bytes::from_static(b"a"), addr))
            .await
            .unwrap();
        tx_b.send(Event::new(Bytes::from_static(b"b"), addr))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while worker.metrics.inflight.load(Ordering::Relaxed) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both blocked pipeline runs must become observable");

        for _ in 0..3 {
            queue_receiver.recv().await.expect("queued event");
        }
        drop(tx_a);
        drop(tx_b);
        tokio::time::timeout(Duration::from_secs(2), async {
            h_a.await.unwrap();
            h_b.await.unwrap();
        })
        .await
        .expect("pipeline workers must drain");
        assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
        assert_eq!(worker.metrics.events_finished.load(Ordering::Relaxed), 2);
    }

    fn make_err_ctx(reason: &str) -> crate::pipeline::ErroredEventContext {
        let addr = std::net::SocketAddr::from_str("127.0.0.1:0").unwrap();
        let ev = Event::new(Bytes::from_static(b"test-event"), addr);
        crate::pipeline::ErroredEventContext::Process {
            timestamp: chrono::Utc::now(),
            pipeline: "test_pipeline".to_string(),
            site: "(test process)".to_string(),
            reason: reason.to_string(),
            event: crate::pipeline::ProcessEvent::from_owned(&ev),
        }
    }

    #[tokio::test]
    async fn write_errored_to_dlq_writes_to_configured_error_log() {
        // The shared DLQ-routing helper feeds the writer when one is
        // configured. Pin that the failure JSONL actually lands in
        // the file — this is the recovery path operators rely on.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("dlq.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(log_path.clone()));
        let metrics = PipelineMetrics::for_testing();
        let err_ctx = make_err_ctx("simulated runtime error");

        write_errored_to_dlq(
            &err_ctx,
            &metrics,
            Some(&writer),
            crate::error_log::ErrorLogFallback::default(),
        )
        .await;

        // Errored counter is bumped at the caller (worker.metrics);
        // the helper itself only writes. Verify the JSONL is on disk.
        // `ErrorLogWriter::write` now awaits `shutdown()` on the file
        // handle before returning, so the record is visible by the
        // time the helper's future resolves — but keep the async
        // reader for symmetry with the runtime path.
        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            contents.contains("simulated runtime error"),
            "DLQ file must contain the reason; got: {contents}"
        );
        assert!(
            contents.contains("test_pipeline"),
            "DLQ file must name the pipeline; got: {contents}"
        );
        assert_eq!(
            metrics.events_errored_unwritable.load(Ordering::Relaxed),
            0,
            "unwritable counter must not bump on a successful write"
        );
    }

    #[tokio::test]
    async fn write_errored_to_dlq_without_writer_does_not_panic() {
        // Baseline: when `error_log` isn't configured, the helper
        // emits a structured tracing line instead. The structured
        // line is observed via `tracing` subscribers in operator
        // setups; the test here pins the no-panic contract so the
        // logged-only branch can't regress to an unwrap somewhere.
        let metrics = PipelineMetrics::for_testing();
        let err_ctx = make_err_ctx("no DLQ configured");

        write_errored_to_dlq(
            &err_ctx,
            &metrics,
            None,
            crate::error_log::ErrorLogFallback::default(),
        )
        .await;

        // Sanity: no metric is touched on this branch (the caller
        // already bumped events_errored before calling us).
        assert_eq!(metrics.events_errored_unwritable.load(Ordering::Relaxed), 0,);
    }

    #[tokio::test]
    async fn output_enqueue_failure_splits_one_dlq_record_per_failed_output() {
        // When a pipeline lists multiple `output` targets and none of
        // them resolve at runtime (= unknown output names slipped past
        // startup validation, or queues were torn down), the enqueue
        // path must produce ONE DLQ record per failed output rather
        // than a single joined record. That lets the operator replay
        // each output independently via
        // `limpidctl inject output <name>` without re-running sibling
        // sinks that were already fine.
        use crate::pipeline::ErroredEventContext;
        let cfg =
            compiled_config("def pipeline p { input i; output sink_a; output sink_b; finish }");
        let runtime_blueprint = bound_blueprint(&cfg);
        let pipeline_id = runtime_blueprint.blueprint.pipeline_id("p").unwrap();
        let execution = Arc::clone(
            runtime_blueprint
                .pipeline_execution(pipeline_id)
                .expect("pipeline p"),
        );
        let ctx = PipelineContext {
            // Empty output_senders → every `output` statement falls
            // into the "unknown output" arm and is reported as a
            // failed enqueue. This is exactly the codepath the runtime
            // is meant to split per-output.
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };

        let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
        let event = Event::new(Bytes::from_static(b"payload"), addr);
        let mut bump = bumpalo::Bump::new();
        let result = run_pipeline_with_outputs_inner(
            &execution,
            &event,
            &ctx,
            &mut bump,
            crate::time::UnixNanos::now(),
        )
        .await
        .expect("pipeline execution should not propagate");

        assert_eq!(
            result.termination,
            crate::pipeline::PipelineTermination::Errored
        );
        assert_eq!(
            result.errored.len(),
            2,
            "two failed outputs must produce two DLQ records"
        );
        let mut names: Vec<String> = result
            .errored
            .iter()
            .map(|ctx| match ctx {
                ErroredEventContext::Output {
                    output_name, site, ..
                } => {
                    assert!(site.ends_with(" enqueue"), "unexpected site: {site}");
                    assert_eq!(*site, format!("{} enqueue", output_name));
                    output_name.clone()
                }
                other => panic!("expected Output variant, got {:?}", other),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["sink_a".to_string(), "sink_b".to_string()]);
    }

    #[tokio::test]
    async fn pipeline_inflight_covers_errored_termination_and_direct_error_dlq_work() {
        for (body, reason) in [
            ("error \"terminal failure\"", "terminal failure"),
            (
                "error missing_runtime_function()",
                "missing_runtime_function",
            ),
        ] {
            let cfg = compiled_config(&format!("def pipeline p {{ input i; {body} }}"));
            let registry = Registry::new();
            let runtime_blueprint = bound_blueprint_with_registry(&cfg, &registry);
            let worker = Arc::new(
                PipelineWorker::from_bound(
                    runtime_blueprint.blueprint.pipeline_id("p").unwrap(),
                    &runtime_blueprint,
                    &registry,
                )
                .expect("pipeline metrics must register"),
            );
            let dir = tempfile::tempdir().unwrap();
            let log_path = dir.path().join("pipeline-errors.jsonl");
            let error_log = Arc::new(crate::error_log::ErrorLogWriter::new(log_path.clone()));
            let guard = error_log.hold_write_lock_for_testing().await;
            let ctx = PipelineContext {
                output_senders: Arc::new(HashMap::new()),
                disk_outputs: Arc::new(HashSet::new()),
                funcs: Arc::new(FunctionRegistry::new()),
                tap: TapRegistry::new(),
                error_log: Some(Arc::clone(&error_log)),
                error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            };
            let event = Event::new(
                Bytes::from_static(b"payload"),
                SocketAddr::from_str("127.0.0.1:0").unwrap(),
            );
            let task_worker = Arc::clone(&worker);
            let task = tokio::spawn(async move {
                let mut bump = bumpalo::Bump::new();
                let timer = input_queue_timer("error-path");
                process_event(&event, &[task_worker], &ctx, "input i", &timer, &mut bump).await;
            });

            tokio::time::timeout(Duration::from_secs(2), async {
                while worker.metrics.events_errored.load(Ordering::Relaxed) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("runtime error must reach terminal bookkeeping");
            assert!(!task.is_finished(), "DLQ write must still be held");
            assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 1);

            drop(guard);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("DLQ completion must release the pipeline")
                .expect("pipeline task must not panic");
            assert_eq!(worker.metrics.inflight.load(Ordering::Relaxed), 0);
            let record = tokio::fs::read_to_string(&log_path).await.unwrap();
            assert!(record.contains(reason), "unexpected DLQ record: {record}");
        }
    }

    /// Structural pin: the pipeline worker's shutdown arm closes
    /// `event_rx` and drains with `recv().await` until `None`, not
    /// `try_recv()` snapshot. The old snapshot loop had the same
    /// permit-holder race as the output queue drain: an input task
    /// that had reserved an mpsc permit but not yet written the
    /// value would complete after the worker exited, silently
    /// dropping the event. Mirror-tested at the tokio-mpsc level in
    /// `queue::tests::tokio_mpsc_close_then_permit_send_still_visible`.
    #[test]
    fn pipeline_worker_shutdown_arm_uses_close_recv_pattern() {
        let src = include_str!("runtime/pipeline_worker.rs");
        // Anchor on the specific inner select arm inside
        // `run_pipeline_workers`, not the outer `let event = tokio::select!`
        // that races receive vs shutdown.
        let marker = "// Close the receiver first, then drain with";
        let start = src
            .find(marker)
            .expect("pipeline worker shutdown drain marker must exist");
        let tail = &src[start..];
        let body_end = tail.find("break;").expect("shutdown arm must break out");
        let body = &tail[..body_end];

        assert!(
            body.contains("event_rx.close()"),
            "pipeline worker shutdown must close event_rx before draining",
        );
        assert!(
            body.contains("event_rx.recv().await"),
            "pipeline worker shutdown must drain with recv().await, not try_recv()",
        );
        assert!(
            !body.contains("event_rx.try_recv()"),
            "pipeline worker shutdown must not use try_recv() — permit-holder race",
        );
    }

    #[tokio::test]
    async fn input_queue_wait_is_observed_once_for_normal_receive_and_shutdown_drain() {
        let registry = Registry::new();
        let normal_timer = crate::metrics::InputQueueTimer::register(&registry, "normal").unwrap();
        let drain_timer = crate::metrics::InputQueueTimer::register(&registry, "drain").unwrap();
        let context = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };

        let (normal_tx, normal_rx) = mpsc::channel(1);
        normal_tx
            .send(Event::new(
                Bytes::from_static(b"normal"),
                "127.0.0.1:0".parse().unwrap(),
            ))
            .await
            .unwrap();
        drop(normal_tx);
        let (_normal_shutdown_tx, normal_shutdown_rx) = tokio::sync::watch::channel(false);
        run_pipeline_workers(
            normal_rx,
            &[],
            &context,
            "normal",
            &normal_timer,
            normal_shutdown_rx,
        )
        .await;
        assert_eq!(normal_timer.count(), 1);

        let (drain_tx, drain_rx) = mpsc::channel(2);
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            drain_tx
                .send(Event::new(
                    Bytes::copy_from_slice(payload),
                    "127.0.0.1:0".parse().unwrap(),
                ))
                .await
                .unwrap();
        }
        let (drain_shutdown_tx, drain_shutdown_rx) = tokio::sync::watch::channel(false);
        drain_shutdown_tx.send(true).unwrap();
        run_pipeline_workers(
            drain_rx,
            &[],
            &context,
            "drain",
            &drain_timer,
            drain_shutdown_rx,
        )
        .await;
        assert_eq!(drain_timer.count(), 2);
    }

    #[tokio::test]
    async fn fan_out_pipelines_share_one_dispatch_boundary_after_input_queue_wait() {
        let config = compiled_config(
            r#"
def input i { type syslog_tcp bind "127.0.0.1:0" }
def output sink { type stdout }
def pipeline first { input i; output sink; finish }
def pipeline second { input i; output sink; finish }
"#,
        );
        let registry = Registry::new();
        let runtime_blueprint = bound_blueprint_with_registry(&config, &registry);
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let mut workers: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|name| {
                PipelineWorker::from_bound(
                    runtime_blueprint.blueprint.pipeline_id(name).unwrap(),
                    &runtime_blueprint,
                    &registry,
                )
                .unwrap()
            })
            .collect();
        workers[0].serial_test_gate = Some(Arc::clone(&gate));
        let first_execution = runtime_blueprint
            .pipeline_execution(runtime_blueprint.blueprint.pipeline_id("first").unwrap())
            .unwrap();
        assert!(Arc::ptr_eq(&workers[0].execution, first_execution,));
        let workers: Vec<_> = workers.into_iter().map(Arc::new).collect();
        let timer = crate::metrics::InputQueueTimer::register(&registry, "i").unwrap();
        let context = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        let dispatch_started_at = crate::time::UnixNanos::now();
        let mut event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        event.received_at =
            crate::time::UnixNanos::new(dispatch_started_at.get() - 60_000_000_000).to_datetime();
        runtime_blueprint
            .blueprint
            .reset_pipeline_by_id_calls_for_testing();

        let process = async {
            process_event_at(
                &event,
                &workers,
                &context,
                "input i",
                &timer,
                &mut bumpalo::Bump::new(),
                dispatch_started_at,
            )
            .await;
        };
        let hold_first_worker = async {
            gate.wait().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            gate.wait().await;
        };
        tokio::join!(process, hold_first_worker);

        assert_eq!(
            runtime_blueprint
                .blueprint
                .pipeline_by_id_calls_for_testing(),
            0,
            "event dispatch must use startup-resolved pipeline descriptors"
        );
        assert_eq!(timer.count(), 1);
        assert_eq!(timer.sum(), 60.0);
        let pipeline_series = metric_series(&registry, "limpid_pipeline_processing_seconds");
        assert_eq!(pipeline_series.len(), 2);
        for series in &pipeline_series {
            assert_eq!(series["count"], 1);
            assert!(series["sum"].as_f64().unwrap() < 5.0);
        }
        let later = pipeline_series
            .iter()
            .find(|series| series["labels"]["pipeline"] == "second")
            .unwrap();
        assert!(
            later["sum"].as_f64().unwrap() >= 0.075,
            "the later serial worker must retain time spent behind the first worker's gate"
        );
    }

    #[test]
    fn startup_is_guarded_until_all_listener_handles_are_registered() {
        let src = format!(
            "{}{}",
            include_str!("runtime/lifecycle.rs"),
            include_str!("runtime/startup.rs")
        );
        assert!(src.contains(&["struct Startup", "Guard"].concat()));
        assert!(src.contains(&["startup_guard.", "commit"].concat()));
        assert!(src.contains(&["late_post_listener", "_failure"].concat()));
    }

    #[test]
    fn startup_transaction_mutant_sensitivity() {
        let lifecycle = include_str!("runtime/lifecycle.rs");
        let startup = include_str!("runtime/startup.rs");
        let lifecycle = &lifecycle[..lifecycle.find("#[cfg(test)]\nmod tests").unwrap()];
        let startup = &startup[..startup.find("#[cfg(test)]\nmod tests").unwrap()];
        let production = format!("{lifecycle}{startup}");
        let production = production.as_str();
        let guard = production.find("StartupGuard::new").unwrap();
        assert!(
            production.find("validate_control_socket_parent").unwrap() < guard,
            "control validation must stay before the first guarded task spawn",
        );
        assert!(
            production
                .find("blueprint.bind(&metrics_registry)")
                .unwrap()
                < guard,
            "blueprint metric binding must stay pre-spawn",
        );
        assert!(
            production
                .find("blueprint.bind(&metrics_registry)")
                .unwrap()
                < production.find("init_tables_from_globals").unwrap(),
            "blueprint compile/bind failure must precede table/resource acquisition",
        );
        assert!(
            production.find("PipelineWorker::from_bound").unwrap() < guard,
            "pipeline identity workers must be planned before the guard",
        );
        assert!(
            production.find("input_queue_sizes.insert").unwrap() < guard,
            "input existence and queue-size parsing must stay pre-spawn",
        );
        let output_factory = production.find(".create_output(").unwrap();
        assert!(guard < output_factory, "guard must own output resources");
        let output_consumer = production[output_factory..]
            .find("startup_guard.track(")
            .unwrap()
            + output_factory;
        let post_output_edge = production[output_consumer..]
            .find("post_output_preactivation_failure(&mut sender)")
            .unwrap()
            + output_consumer;
        assert!(output_factory < output_consumer && output_consumer < post_output_edge);
        assert!(production[output_consumer..post_output_edge].contains("output_task_kind"));
        let listeners = production.find("start_listener_groups").unwrap();
        let failpoint = production.rfind("late_post_listener_failure()").unwrap();
        let commit = production.rfind("startup_guard.commit()").unwrap();
        assert!(listeners < failpoint && failpoint < commit);
        let rollback = production.find("async fn rollback").unwrap();
        let rollback_body = &production[rollback..production.find("fn commit").unwrap()];
        assert!(rollback_body.contains("shutdown_tasks"));
        assert!(!rollback_body.contains(".abort()"));
        let cleanup = &production[production.find("shutdown_tasks_with_timeout").unwrap()..guard];
        assert!(cleanup.contains("task.handle = None"));
        assert!(cleanup.contains("task.handle.take()"));
        assert!(cleanup.contains("graceful_deadline"));
        assert!(cleanup.contains("overall_deadline"));
        assert!(cleanup.contains("TaskKind::AbortSafe"));
        assert!(cleanup.contains("TaskKind::MustJoin"));
        assert!(cleanup.contains("Await every remaining owner before returning"));
    }

    #[tokio::test]
    async fn closed_shutdown_watch_is_terminal_for_pipeline_worker() {
        let (sender, mut receiver) = watch::channel(false);
        drop(sender);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                shutdown_change_is_terminal(&mut receiver),
            )
            .await
            .expect("closed watch must resolve without spinning")
        );
        let marker = ["shutdown_change_is_terminal", "(&mut shutdown)"].concat();
        assert!(include_str!("runtime/pipeline_worker.rs").contains(&marker));
    }

    #[tokio::test]
    async fn closed_pipeline_watch_drains_already_queued_event_before_exit() {
        let registry = Registry::new();
        let config = compiled_config("def pipeline p { input i; finish }");
        let runtime_blueprint = bound_blueprint_with_registry(&config, &registry);
        let worker = Arc::new(
            PipelineWorker::from_bound(
                runtime_blueprint.blueprint.pipeline_id("p").unwrap(),
                &runtime_blueprint,
                &registry,
            )
            .unwrap(),
        );
        let workers = vec![Arc::clone(&worker)];
        let ctx = PipelineContext {
            output_senders: Arc::new(HashMap::new()),
            disk_outputs: Arc::new(HashSet::new()),
            funcs: Arc::new(FunctionRegistry::new()),
            tap: TapRegistry::new(),
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
        };
        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .send(Event::new(
                Bytes::from_static(b"queued-before-watch-close"),
                "127.0.0.1:1".parse().unwrap(),
            ))
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        drop(shutdown_tx);

        tokio::time::timeout(
            Duration::from_secs(2),
            run_pipeline_workers(
                event_rx,
                &workers,
                &ctx,
                "i",
                &input_queue_timer("closed-watch"),
                shutdown_rx,
            ),
        )
        .await
        .expect("closed watch must terminate after draining the input channel");
        assert_eq!(worker.metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(worker.metrics.events_discarded.load(Ordering::Relaxed), 1);
        assert!(event_tx.is_closed());
    }
}

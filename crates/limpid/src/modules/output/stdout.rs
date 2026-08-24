//! Stdout output: writes event messages to standard output (debugging/testing).

use std::fmt;
use std::io;
#[cfg(test)]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;

use crate::dsl::schema::PropertySpec;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

async fn shutdown_change_is_terminal(shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

/// `output stdout` has no module-specific properties; only the
/// common `retry { ... } / queue { ... }` sub-blocks apply.
const STDOUT_OUTPUT_SCHEMA: &[PropertySpec] = &[
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

/// Process-wide stdout readiness registration and serialization. Every stdout
/// output shares this one Unix transport so frames from distinct output actors
/// cannot interleave. It is deliberately module-local: this is not a generic
/// writer abstraction and does not change the output trait.
struct StdoutTransport {
    backend: StdoutBackend,
    serial: tokio::sync::Mutex<()>,
    #[cfg(test)]
    observer: Option<Arc<WriteObserver>>,
}

enum StdoutBackend {
    Async(tokio::io::unix::AsyncFd<StdoutFd>),
    Regular(StdoutFd),
}

struct StdoutFd {
    fd: RawFd,
    // Production borrows fd 1. Tests can give the transport an owned pipe or
    // PTY descriptor whose lifetime must cover readiness registration.
    _owned: Option<OwnedFd>,
}

impl AsRawFd for StdoutFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

#[derive(Debug)]
struct StdoutWriteError {
    source: io::Error,
    written: usize,
}

impl fmt::Display for StdoutWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.written == 0 {
            write!(formatter, "stdout write failed: {}", self.source)
        } else {
            write!(
                formatter,
                "stdout write failed after {} confirmed byte(s): {}",
                self.written, self.source
            )
        }
    }
}

impl std::error::Error for StdoutWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

static STDOUT_TRANSPORT: OnceLock<Arc<StdoutTransport>> = OnceLock::new();
static STDOUT_TRANSPORT_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
tokio::task_local! {
    static TEST_STDOUT_TRANSPORT: Arc<StdoutTransport>;
}

#[cfg(test)]
#[derive(Default)]
struct WriteObserver {
    written: std::sync::atomic::AtomicUsize,
    waiting_count: std::sync::atomic::AtomicUsize,
    waiting: tokio::sync::Notify,
    progressed: tokio::sync::Notify,
}

impl StdoutTransport {
    fn stdout() -> Result<Arc<Self>> {
        #[cfg(test)]
        if let Ok(transport) = TEST_STDOUT_TRANSPORT.try_with(Arc::clone) {
            return Ok(transport);
        }

        Self::shared_for_cell(&STDOUT_TRANSPORT, &STDOUT_TRANSPORT_INIT, || {
            Self::from_fd(1, None)
        })
    }

    /// Returns the one process-wide transport without memoizing constructor
    /// failures. Initialization is serialized before the constructor touches
    /// fd 1, so a successful transport has no racing AsyncFd owner or flag
    /// restoration from a losing candidate.
    fn shared_for_cell(
        cell: &OnceLock<Arc<Self>>,
        init_lock: &std::sync::Mutex<()>,
        initialize: impl FnOnce() -> io::Result<Self>,
    ) -> Result<Arc<Self>> {
        Self::shared_for_cell_inner(cell, init_lock, || {}, initialize)
    }

    fn shared_for_cell_inner(
        cell: &OnceLock<Arc<Self>>,
        init_lock: &std::sync::Mutex<()>,
        before_lock: impl FnOnce(),
        initialize: impl FnOnce() -> io::Result<Self>,
    ) -> Result<Arc<Self>> {
        if let Some(transport) = cell.get() {
            return Ok(Arc::clone(transport));
        }

        before_lock();
        let _init_guard = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(transport) = cell.get() {
            return Ok(Arc::clone(transport));
        }

        let transport = Arc::new(initialize()?);
        if cell.set(Arc::clone(&transport)).is_ok() {
            return Ok(transport);
        }
        Ok(Arc::clone(cell.get().expect(
            "successful stdout transport initialization must populate OnceLock",
        )))
    }

    #[cfg(test)]
    fn shared_for_cell_observed(
        cell: &OnceLock<Arc<Self>>,
        init_lock: &std::sync::Mutex<()>,
        before_lock: impl FnOnce(),
        initialize: impl FnOnce() -> io::Result<Self>,
    ) -> Result<Arc<Self>> {
        Self::shared_for_cell_inner(cell, init_lock, before_lock, initialize)
    }

    fn from_fd(fd: RawFd, owned: Option<OwnedFd>) -> io::Result<Self> {
        let backend = if fd_is_regular_file(fd)? {
            StdoutBackend::Regular(StdoutFd { fd, _owned: owned })
        } else {
            let original_flags = get_fd_flags(fd)?;
            let changed = original_flags & libc::O_NONBLOCK == 0;
            if changed {
                set_fd_flags(fd, original_flags | libc::O_NONBLOCK)?;
            }
            match tokio::io::unix::AsyncFd::new(StdoutFd { fd, _owned: owned }) {
                Ok(async_fd) => StdoutBackend::Async(async_fd),
                Err(error) => {
                    if changed {
                        let _ = set_fd_flags(fd, original_flags);
                    }
                    return Err(error);
                }
            }
        };
        Ok(Self {
            backend,
            serial: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            observer: None,
        })
    }

    #[cfg(test)]
    fn from_owned(fd: OwnedFd, observer: Option<Arc<WriteObserver>>) -> io::Result<Arc<Self>> {
        let raw = fd.as_raw_fd();
        let mut transport = Self::from_fd(raw, Some(fd))?;
        transport.observer = observer;
        Ok(Arc::new(transport))
    }

    async fn write_frame(
        &self,
        frame: &[u8],
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> std::result::Result<(), StdoutWriteError> {
        self.write_frame_mode(frame, shutdown, false).await
    }

    async fn write_frame_drain(
        &self,
        frame: &[u8],
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> std::result::Result<(), StdoutWriteError> {
        self.write_frame_mode(frame, shutdown, true).await
    }

    async fn write_frame_mode(
        &self,
        frame: &[u8],
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        drain: bool,
    ) -> std::result::Result<(), StdoutWriteError> {
        if !drain && *shutdown.borrow() {
            return Err(StdoutWriteError {
                source: io::Error::new(io::ErrorKind::Interrupted, "stdout shutdown requested"),
                written: 0,
            });
        }

        let _serial = if drain {
            self.serial.lock().await
        } else {
            tokio::select! {
                guard = self.serial.lock() => guard,
                terminal = shutdown_change_is_terminal(shutdown) => {
                    let message = if terminal {
                        "stdout shutdown requested"
                    } else {
                        "stdout shutdown signal changed"
                    };
                    return Err(StdoutWriteError {
                        source: io::Error::new(io::ErrorKind::Interrupted, message),
                        written: 0,
                    });
                }
            }
        };

        if let StdoutBackend::Regular(fd) = &self.backend {
            let raw_fd = fd.as_raw_fd();
            let bytes = frame.to_vec();
            return tokio::task::spawn_blocking(move || raw_write_all(raw_fd, &bytes))
                .await
                .map_err(|error| StdoutWriteError {
                    source: io::Error::other(format!(
                        "stdout regular-file writer task failed: {error}"
                    )),
                    written: 0,
                })?;
        }

        let StdoutBackend::Async(fd) = &self.backend else {
            unreachable!("regular stdout handled above")
        };

        let mut offset = 0usize;
        while offset < frame.len() {
            if !drain && *shutdown.borrow() {
                return Err(StdoutWriteError {
                    source: io::Error::new(io::ErrorKind::Interrupted, "stdout shutdown requested"),
                    written: offset,
                });
            }
            #[cfg(test)]
            if let Some(observer) = &self.observer {
                observer
                    .waiting_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                observer.waiting.notify_waiters();
            }
            let mut readiness = if drain {
                fd.writable().await.map_err(|source| StdoutWriteError {
                    source,
                    written: offset,
                })?
            } else {
                tokio::select! {
                    result = fd.writable() => result.map_err(|source| StdoutWriteError { source, written: offset })?,
                    terminal = shutdown_change_is_terminal(shutdown) => {
                        let message = if terminal {
                            "stdout shutdown requested"
                        } else {
                            "stdout shutdown signal changed"
                        };
                        return Err(StdoutWriteError {
                            source: io::Error::new(io::ErrorKind::Interrupted, message),
                            written: offset,
                        });
                    }
                }
            };
            match readiness.try_io(|inner| raw_write(inner.get_ref().as_raw_fd(), &frame[offset..]))
            {
                Ok(Ok(0)) => {
                    return Err(StdoutWriteError {
                        source: io::Error::new(
                            io::ErrorKind::WriteZero,
                            "stdout write returned zero",
                        ),
                        written: offset,
                    });
                }
                Ok(Ok(written)) => {
                    offset += written;
                    #[cfg(test)]
                    if let Some(observer) = &self.observer {
                        observer
                            .written
                            .store(offset, std::sync::atomic::Ordering::SeqCst);
                        observer.progressed.notify_waiters();
                    }
                }
                Ok(Err(source)) => {
                    return Err(StdoutWriteError {
                        source,
                        written: offset,
                    });
                }
                Err(_would_block) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor supplied by the process or owned by
    // the transport. fcntl does not access Rust memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        // SAFETY: same descriptor validity argument as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn get_fd_flags(fd: RawFd) -> io::Result<i32> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

fn set_fd_flags(fd: RawFd, flags: i32) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn fd_is_regular_file(fd: RawFd) -> io::Result<bool> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.st_mode & libc::S_IFMT == libc::S_IFREG)
}

pub(crate) fn stdout_is_regular_file() -> io::Result<bool> {
    fd_is_regular_file(1)
}

fn raw_write_all(fd: RawFd, bytes: &[u8]) -> std::result::Result<(), StdoutWriteError> {
    let mut written = 0;
    while written < bytes.len() {
        match raw_write(fd, &bytes[written..]) {
            Ok(0) => {
                return Err(StdoutWriteError {
                    source: io::Error::new(io::ErrorKind::WriteZero, "stdout write returned zero"),
                    written,
                });
            }
            Ok(count) => written += count,
            Err(source) => return Err(StdoutWriteError { source, written }),
        }
    }
    Ok(())
}

fn raw_write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` is valid for the duration of write(2); the descriptor is
    // held alive by `StdoutFd` (or is process fd 1).
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

#[cfg(test)]
pub(crate) async fn with_test_stdout_fd<T>(
    fd: OwnedFd,
    future: impl std::future::Future<Output = T>,
) -> io::Result<T> {
    let transport = StdoutTransport::from_owned(fd, None)?;
    Ok(TEST_STDOUT_TRANSPORT.scope(transport, future).await)
}

pub struct StdoutOutput {
    name: String,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

impl Module for StdoutOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(STDOUT_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        Ok(Self {
            name: name.to_string(),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
            shutdown_signal: ctx.shutdown_signal.clone(),
        })
    }
}

impl HasMetrics for StdoutOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl StdoutOutput {
    async fn write_event_async(
        &self,
        event: &Event,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> std::result::Result<(), StdoutWriteError> {
        let frame = super::frame_with_newline(&event.egress);
        let transport = StdoutTransport::stdout().map_err(|error| StdoutWriteError {
            source: io::Error::other(error.to_string()),
            written: 0,
        })?;
        transport.write_frame(&frame, shutdown).await?;
        self.metrics.bytes_written.inc_by(frame.len() as u64);
        Ok(())
    }

    async fn write_event_drain(&self, event: &Event) -> std::result::Result<(), StdoutWriteError> {
        let frame = super::frame_with_newline(&event.egress);
        let transport = StdoutTransport::stdout().map_err(|error| StdoutWriteError {
            source: io::Error::other(error.to_string()),
            written: 0,
        })?;
        let mut shutdown = self.shutdown_signal.clone();
        transport.write_frame_drain(&frame, &mut shutdown).await?;
        self.metrics.bytes_written.inc_by(frame.len() as u64);
        Ok(())
    }

    #[cfg(test)]
    fn write_event_to(&self, out: &mut impl std::io::Write, event: &Event) -> Result<()> {
        let buf = super::frame_with_newline(&event.egress);
        out.write_all(&buf)
            .map_err(|error| anyhow::anyhow!("stdout write failed: {error}"))?;
        self.metrics.bytes_written.inc_by(buf.len() as u64);
        Ok(())
    }

    async fn route_failed_event(&self, event: &Event, ack: QueueAckHandle, reason: &str) {
        let outcome = crate::modules::route_event_to_dlq(
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack.position(),
            reason,
        )
        .await;
        crate::modules::resolve_ack_from_dlq_outcome(ack, outcome, &self.metrics);
    }

    async fn consume_nonblocking(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            match self.write_event_async(event, &mut shutdown).await {
                Ok(()) => {
                    self.metrics.in_retry.set(0);
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(error) => {
                    attempt += 1;
                    self.metrics.retries.inc();

                    // A retry after a partial frame would duplicate the
                    // confirmed prefix. Resolve it through the existing DLQ
                    // policy immediately instead. Shutdown interruption is
                    // likewise terminal for the current queue acknowledgement.
                    if error.written != 0 || *shutdown.borrow() {
                        self.metrics.in_retry.set(0);
                        let reason =
                            format!("output write stopped after {attempt} attempt(s): {error}");
                        self.route_failed_event(event, ack, &reason).await;
                        return Ok(());
                    }

                    if attempt >= self.retry.max_attempts {
                        self.metrics.in_retry.set(0);
                        let reason =
                            format!("output write failed after {attempt} attempts: {error}");
                        self.route_failed_event(event, ack, &reason).await;
                        return Ok(());
                    }
                    self.metrics.in_retry.set(1);
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        error,
                        wait
                    );
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        self.metrics.in_retry.set(0);
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry after {attempt} attempts: {error}"
                        );
                        self.route_failed_event(event, ack, &reason).await;
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    #[cfg(test)]
    async fn consume_with_write<F>(
        &self,
        event: &Event,
        ack: QueueAckHandle,
        mut write: F,
    ) -> Result<()>
    where
        F: FnMut(&Event) -> Result<()> + Send,
    {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            match write(event) {
                Ok(()) => {
                    self.metrics.in_retry.set(0);
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.inc();
                    if attempt >= self.retry.max_attempts {
                        self.metrics.in_retry.set(0);
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    self.metrics.in_retry.set(1);
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    // Race the backoff sleep against shutdown. If the runtime
                    // signals shutdown mid-sleep, do NOT keep retrying — the
                    // retry budget (default 1+2+4+8 = 15 s) can outlast the
                    // runtime's 10 s shutdown budget, and if we don't return
                    // the queue consumer's select! never gets back to its
                    // shutdown arm. Route the pending event to DLQ, resolve
                    // `Recovered`, and return.
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        self.metrics.in_retry.set(0);
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Output for StdoutOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.consume_nonblocking(event, ack).await
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut ack = ack;
        ack.allow_abort_drop();
        match self.write_event_drain(event).await {
            Ok(()) => {
                self.metrics.events_written.inc();
                ack.resolve_delivered();
            }
            Err(error) => {
                let reason = format!("output shutdown write failed: {error}");
                self.route_failed_event(event, ack, &reason).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod metrics_registration_tests {
    use super::*;
    use crate::dsl::module_props::ModuleProperties;
    use crate::metrics::{MetricsError, OutputMetrics, Registry};

    fn unix_pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: `fds` points to two writable integers and successful pipe(2)
        // initializes both descriptors, which are immediately owned below.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: successful pipe(2) returned new uniquely-owned descriptors.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn test_output(
        name: &str,
        shutdown_signal: tokio::sync::watch::Receiver<bool>,
        retry: RetryConfig,
    ) -> Arc<StdoutOutput> {
        Arc::new(StdoutOutput {
            name: name.to_owned(),
            retry,
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal,
        })
    }

    fn one_attempt_retry() -> RetryConfig {
        RetryConfig {
            max_attempts: 1,
            initial_wait: std::time::Duration::from_millis(1),
            max_wait: std::time::Duration::from_millis(1),
            backoff: crate::queue::BackoffStrategy::Fixed,
        }
    }

    #[tokio::test]
    async fn failed_stdout_transport_initialization_is_retried_and_usable() {
        let cell = OnceLock::new();
        let init_lock = std::sync::Mutex::new(());
        let attempts = std::sync::atomic::AtomicUsize::new(0);

        let error = match StdoutTransport::shared_for_cell(&cell, &init_lock, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(io::Error::other("forced first initialization failure"))
        }) {
            Ok(_) => panic!("forced initialization failure unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("forced first initialization failure")
        );
        assert!(
            cell.get().is_none(),
            "a failed constructor must not populate the cell"
        );

        let (read_fd, write_fd) = unix_pipe();
        let transport = StdoutTransport::shared_for_cell(&cell, &init_lock, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let raw = write_fd.as_raw_fd();
            StdoutTransport::from_fd(raw, Some(write_fd))
        })
        .unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(Arc::ptr_eq(&transport, cell.get().unwrap()));

        let read_fd = async_owned_fd(read_fd);
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        transport
            .write_frame(b"retry-success\n", &mut shutdown_rx)
            .await
            .unwrap();
        assert_eq!(read_exact_fd(&read_fd, 14).await, b"retry-success\n");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 5)]
    async fn concurrent_stdout_first_use_constructs_one_shared_async_fd_transport() {
        const CALLERS: usize = 4;
        let cell = Arc::new(OnceLock::new());
        let init_lock = Arc::new(std::sync::Mutex::new(()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        match StdoutTransport::shared_for_cell(&cell, &init_lock, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(io::Error::other("forced pre-race failure"))
        }) {
            Ok(_) => panic!("forced pre-race failure unexpectedly succeeded"),
            Err(error) => assert!(error.to_string().contains("forced pre-race failure")),
        }
        assert!(cell.get().is_none());

        let (read_fd, write_fd) = unix_pipe();
        let shared_raw_fd = write_fd.as_raw_fd();
        assert_eq!(get_fd_flags(shared_raw_fd).unwrap() & libc::O_NONBLOCK, 0);
        let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
        let tasks: Vec<_> = (0..CALLERS)
            .map(|_| {
                let cell = Arc::clone(&cell);
                let init_lock = Arc::clone(&init_lock);
                let attempts = Arc::clone(&attempts);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    StdoutTransport::shared_for_cell_observed(
                        &cell,
                        &init_lock,
                        || {
                            barrier.wait();
                        },
                        || {
                            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            StdoutTransport::from_fd(shared_raw_fd, None)
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let mut transports = Vec::with_capacity(CALLERS);
        for task in tasks {
            transports.push(task.await.unwrap());
        }

        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one failed call and exactly one racing successful constructor must run"
        );
        let canonical = Arc::clone(cell.get().unwrap());
        assert!(
            transports
                .iter()
                .all(|transport| Arc::ptr_eq(transport, &canonical)),
            "all callers must receive the one canonical successful transport"
        );
        assert_ne!(
            get_fd_flags(shared_raw_fd).unwrap() & libc::O_NONBLOCK,
            0,
            "no losing constructor may restore blocking flags behind the winner"
        );

        let read_fd = async_owned_fd(read_fd);
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        canonical
            .write_frame(b"shared-first-use\n", &mut shutdown_rx)
            .await
            .unwrap();
        assert_eq!(read_exact_fd(&read_fd, 17).await, b"shared-first-use\n");

        drop(transports);
        drop(canonical);
        drop(cell);
        drop(write_fd);
    }

    fn fill_pipe(fd: RawFd) -> usize {
        set_nonblocking(fd).unwrap();
        let chunk = [b'x'; 8192];
        let mut total = 0;
        loop {
            match raw_write(fd, &chunk) {
                Ok(written) => total += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return total,
                Err(error) => panic!("fill pipe: {error}"),
            }
        }
    }

    async fn read_exact_fd(fd: &tokio::io::unix::AsyncFd<StdoutFd>, size: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(size);
        while bytes.len() < size {
            let mut ready = fd.readable().await.unwrap();
            match ready.try_io(|inner| {
                let mut chunk = [0u8; 8192];
                let remaining = (size - bytes.len()).min(chunk.len());
                // SAFETY: chunk is valid writable memory and the descriptor is
                // held by AsyncFd for the entire read.
                let count = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        chunk.as_mut_ptr().cast(),
                        remaining,
                    )
                };
                if count == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(chunk[..count as usize].to_vec())
                }
            }) {
                Ok(Ok(chunk)) if chunk.is_empty() => panic!("unexpected EOF"),
                Ok(Ok(chunk)) => bytes.extend_from_slice(&chunk),
                Ok(Err(error)) => panic!("read failed: {error}"),
                Err(_would_block) => {}
            }
        }
        bytes
    }

    fn async_owned_fd(fd: OwnedFd) -> tokio::io::unix::AsyncFd<StdoutFd> {
        let raw = fd.as_raw_fd();
        set_nonblocking(raw).unwrap();
        tokio::io::unix::AsyncFd::new(StdoutFd {
            fd: raw,
            _owned: Some(fd),
        })
        .unwrap()
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
                "limpid_output_bytes_written_total",
            ]
            .contains(&name.as_str())
        );
        assert_eq!(labelset, &[("output".to_owned(), label_value.to_owned())]);
        assert!(diagnostic.contains(&format!("name={name:?}")));
        assert!(diagnostic.contains(&format!("labelset={labelset:?}")));
    }

    #[tokio::test]
    async fn full_stdout_pipe_is_shutdown_interruptible_and_resolves_current_ack() {
        let (read_fd, write_fd) = unix_pipe();
        let capacity = fill_pipe(write_fd.as_raw_fd());
        assert!(capacity > 0);
        let observer = Arc::new(WriteObserver::default());
        let transport = StdoutTransport::from_owned(write_fd, Some(Arc::clone(&observer))).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = test_output("stdout-full-pipe", shutdown_rx, one_attempt_retry());
        let event = Event::new(
            bytes::Bytes::from_static(b"blocked"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(TEST_STDOUT_TRANSPORT.scope(transport, async move {
            task_output.consume(&event, ack).await
        }));

        while observer
            .waiting_count
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            observer.waiting.notified().await;
        }
        let readiness_waits = observer
            .waiting_count
            .load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            observer
                .waiting_count
                .load(std::sync::atomic::Ordering::SeqCst),
            readiness_waits,
            "a full pipe must stay parked on readiness rather than busy-spin"
        );
        assert!(ack_rx.try_recv().is_err());
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("shutdown must interrupt stdout readiness wait")
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        drop(read_fd);
    }

    #[tokio::test]
    async fn partial_stdout_write_then_shutdown_never_retries_confirmed_prefix() {
        let (read_fd, write_fd) = unix_pipe();
        let observer = Arc::new(WriteObserver::default());
        let transport = StdoutTransport::from_owned(write_fd, Some(Arc::clone(&observer))).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = test_output("stdout-partial", shutdown_rx, one_attempt_retry());
        let payload = vec![b'p'; 2 * 1024 * 1024];
        let frame_len = payload.len() + 1;
        let event = Event::new(bytes::Bytes::from(payload), "127.0.0.1:0".parse().unwrap());
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(TEST_STDOUT_TRANSPORT.scope(transport, async move {
            task_output.consume(&event, ack).await
        }));

        while observer.written.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            observer.progressed.notified().await;
        }
        let confirmed = observer.written.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            confirmed < frame_len,
            "fixture must stop at a partial frame"
        );
        assert!(!task.is_finished());
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("partial write must be interruptible")
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert_eq!(
            observer.written.load(std::sync::atomic::Ordering::SeqCst),
            confirmed,
            "shutdown must not restart a partially written frame"
        );
        assert_eq!(
            output
                .metrics
                .retries
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the partial attempt is terminal and is never retried"
        );
        drop(read_fd);
    }

    #[tokio::test(start_paused = true)]
    async fn broken_pipe_uses_retry_then_existing_dlq_ack_semantics() {
        let (read_fd, write_fd) = unix_pipe();
        drop(read_fd);
        let transport = StdoutTransport::from_owned(write_fd, None).unwrap();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = test_output(
            "stdout-epipe",
            shutdown_rx,
            RetryConfig {
                max_attempts: 2,
                initial_wait: std::time::Duration::from_millis(25),
                max_wait: std::time::Duration::from_millis(25),
                backoff: crate::queue::BackoffStrategy::Fixed,
            },
        );
        let event = Event::new(
            bytes::Bytes::from_static(b"epipe"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(TEST_STDOUT_TRANSPORT.scope(transport, async move {
            task_output.consume(&event, ack).await
        }));

        while output
            .metrics
            .in_retry
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(std::time::Duration::from_millis(25)).await;
        task.await.unwrap().unwrap();
        assert_eq!(
            output
                .metrics
                .retries
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert_eq!(
            output
                .metrics
                .events_failed
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn pty_transport_preserves_payload_bytes_verbatim() {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: pointers are valid; null termios/winsize requests defaults.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty returned uniquely-owned descriptors.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: slave is live and tcgetattr initializes termios on success.
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), termios.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr succeeded, so the value is initialized.
        let mut termios = unsafe { termios.assume_init() };
        // SAFETY: cfmakeraw mutates only the initialized termios value.
        unsafe { libc::cfmakeraw(&mut termios) };
        // SAFETY: slave is live and termios remains initialized.
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) },
            0
        );

        let transport = StdoutTransport::from_owned(slave, None).unwrap();
        let transport_keepalive = Arc::clone(&transport);
        let master = async_owned_fd(master);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = test_output("stdout-pty", shutdown_rx, one_attempt_retry());
        let payload = bytes::Bytes::from_static(b"\x00\xffA\nB");
        let event = Event::new(payload.clone(), "127.0.0.1:0".parse().unwrap());
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        TEST_STDOUT_TRANSPORT
            .scope(transport, output.consume(&event, ack))
            .await
            .unwrap();
        let observed = read_exact_fd(&master, payload.len() + 1).await;
        drop(transport_keepalive);
        let mut expected = payload.to_vec();
        expected.push(b'\n');
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn queue_consumer_shutdown_drains_writable_stdout_exactly() {
        let (read_fd, write_fd) = unix_pipe();
        let transport = StdoutTransport::from_owned(write_fd, None).unwrap();
        let read_fd = async_owned_fd(read_fd);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = test_output(
            "stdout-shutdown-drain",
            shutdown_rx.clone(),
            one_attempt_retry(),
        );
        let payload = bytes::Bytes::from_static(b"drain-me");
        let event = Event::new(payload.clone(), "127.0.0.1:0".parse().unwrap());
        let (sender, receiver) = crate::queue::create_queue(
            "stdout-shutdown-drain".to_string(),
            crate::queue::QueueConfig {
                queue_type: crate::queue::QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        sender
            .send(crate::event::QueuedEvent::new(
                event,
                crate::time::UnixNanos::now(),
            ))
            .await
            .unwrap();
        drop(sender);
        shutdown_tx.send(true).unwrap();

        let writer: Arc<dyn crate::modules::Output> = output.clone();
        let metrics = Arc::clone(&output.metrics);
        let reader = tokio::spawn(async move { read_exact_fd(&read_fd, payload.len() + 1).await });
        TEST_STDOUT_TRANSPORT
            .scope(transport, async move {
                crate::queue::run_queue_consumer(
                    receiver,
                    writer,
                    None,
                    metrics,
                    None,
                    shutdown_rx,
                )
                .await;
            })
            .await;

        let observed = reader.await.unwrap();
        assert_eq!(observed, b"drain-me\n");
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            output
                .metrics
                .events_failed
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn subprocess_fd1_regular_file_preserves_bytes_and_flags() {
        const CHILD: &str = "LIMPID_STDOUT_REGULAR_FILE_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            use std::os::fd::IntoRawFd as _;
            let saved = unsafe { libc::dup(1) };
            assert_ne!(saved, -1);
            let file = std::fs::File::create(path).unwrap();
            let file_fd = file.into_raw_fd();
            assert_ne!(unsafe { libc::dup2(file_fd, 1) }, -1);
            unsafe { libc::close(file_fd) };

            let flags_before = get_fd_flags(1).unwrap();
            assert_eq!(flags_before & libc::O_NONBLOCK, 0);
            assert!(stdout_is_regular_file().unwrap());
            let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let output = test_output("stdout-regular-child", shutdown_rx, one_attempt_retry());
            let event = Event::new(
                bytes::Bytes::from_static(b"regular-file-exact"),
                "127.0.0.1:0".parse().unwrap(),
            );
            let (ack, mut ack_rx) = QueueAckHandle::for_test();
            output.consume(&event, ack).await.unwrap();
            assert!(matches!(
                ack_rx.recv().await,
                Some((_, crate::queue::AckDisposition::Delivered))
            ));
            assert_eq!(get_fd_flags(1).unwrap(), flags_before);
            assert_ne!(unsafe { libc::dup2(saved, 1) }, -1);
            unsafe { libc::close(saved) };
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stdout.log");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("modules::output::stdout::metrics_registration_tests::subprocess_fd1_regular_file_preserves_bytes_and_flags")
            .arg("--nocapture")
            .env(CHILD, &output_path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(output_path).unwrap(), b"regular-file-exact\n");
    }

    #[tokio::test]
    async fn multiple_stdout_outputs_share_one_frame_serialization_lock() {
        let (read_fd, write_fd) = unix_pipe();
        let transport = StdoutTransport::from_owned(write_fd, None).unwrap();
        let read_fd = async_owned_fd(read_fd);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let first = test_output("stdout-a", shutdown_rx.clone(), one_attempt_retry());
        let second = test_output("stdout-b", shutdown_rx, one_attempt_retry());
        let a = vec![b'a'; 256 * 1024];
        let b = vec![b'b'; 256 * 1024];
        let total = a.len() + b.len() + 2;
        let reader = tokio::spawn(async move { read_exact_fd(&read_fd, total).await });

        let first_transport = Arc::clone(&transport);
        let first_task = tokio::spawn(TEST_STDOUT_TRANSPORT.scope(first_transport, async move {
            let event = Event::new(bytes::Bytes::from(a), "127.0.0.1:0".parse().unwrap());
            let (ack, _ack_rx) = QueueAckHandle::for_test();
            first.consume(&event, ack).await
        }));
        let second_task = tokio::spawn(TEST_STDOUT_TRANSPORT.scope(transport, async move {
            let event = Event::new(bytes::Bytes::from(b), "127.0.0.1:0".parse().unwrap());
            let (ack, _ack_rx) = QueueAckHandle::for_test();
            second.consume(&event, ack).await
        }));
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();
        let observed = reader.await.unwrap();
        let split = observed.iter().position(|byte| *byte == b'\n').unwrap();
        assert!(
            split == 256 * 1024
                && (observed[..split].iter().all(|byte| *byte == b'a')
                    || observed[..split].iter().all(|byte| *byte == b'b'))
        );
        let second_frame = &observed[split + 1..];
        assert_eq!(second_frame.len(), 256 * 1024 + 1);
        assert_eq!(second_frame.last(), Some(&b'\n'));
        let expected = if observed[0] == b'a' { b'b' } else { b'a' };
        assert!(
            second_frame[..second_frame.len() - 1]
                .iter()
                .all(|byte| *byte == expected)
        );
    }

    #[test]
    fn stdout_transport_mutant_sensitivity_pins_nonblocking_worker_path() {
        let source = include_str!("stdout.rs");
        assert!(!source.contains(&["std::io::", "stdout"].concat()));
        assert_eq!(source.matches(&["spawn_", "blocking"].concat()).count(), 1);
        assert!(source.contains("StdoutBackend::Regular"));
        assert!(source.contains("StdoutBackend::Async"));
        assert!(source.contains("fd.writable()"));
        assert!(source.contains("readiness.try_io"));
        assert!(source.contains("let mut offset = 0usize"));
        assert!(source.contains("let _serial = tokio::select!"));
        assert!(source.contains("error.written != 0"));
    }

    #[test]
    fn factory_uses_the_shared_registry_and_propagates_registration_conflicts() {
        let registry = Arc::new(Registry::new());
        OutputMetrics::register(&registry, "conflicting")
            .expect("preseeded output metrics must register");
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.metrics = Arc::clone(&registry);
        let properties = ModuleProperties::from_parts("stdout", Vec::new());

        let error = match StdoutOutput::from_properties("conflicting", &properties, &ctx) {
            Ok(_) => panic!("factory unexpectedly swallowed the registration conflict"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        let metrics_error = error
            .chain()
            .find_map(|source| source.downcast_ref::<MetricsError>())
            .unwrap_or_else(|| {
                panic!(
                    "MetricsError must remain downcastable in the anyhow source chain: {error:#}"
                )
            });
        assert_output_duplicate(metrics_error, "conflicting", &diagnostic);
    }

    #[tokio::test]
    async fn successful_write_counts_payload_and_adapter_newline_once() {
        let ctx = crate::modules::BuildContext::for_testing();
        let properties = ModuleProperties::from_parts("stdout", Vec::new());
        let output = StdoutOutput::from_properties("stdout-bytes", &properties, &ctx).unwrap();
        let payload = bytes::Bytes::from_static(b"stdout-payload");
        let event = crate::event::Event::new(payload.clone(), "127.0.0.1:0".parse().unwrap());
        let (ack, _ack_rx) = crate::queue::QueueAckHandle::for_test();

        output.consume(&event, ack).await.unwrap();

        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            (payload.len() + 1) as u64
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "event and byte counters remain independent"
        );
    }

    #[test]
    fn writer_seam_counts_only_a_fully_confirmed_stdout_frame() {
        use std::io::{self, Write};

        struct AlwaysFails;

        impl Write for AlwaysFails {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "test failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let ctx = crate::modules::BuildContext::for_testing();
        let properties = ModuleProperties::from_parts("stdout", Vec::new());
        let output = StdoutOutput::from_properties("stdout-writer", &properties, &ctx).unwrap();
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"stdout-payload"),
            "127.0.0.1:0".parse().unwrap(),
        );

        let mut written = Vec::new();
        output.write_event_to(&mut written, &event).unwrap();
        assert_eq!(written, b"stdout-payload\n");
        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            written.len() as u64
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the private write seam owns bytes, not event disposition"
        );

        let mut failing = AlwaysFails;
        output
            .write_event_to(&mut failing, &event)
            .expect_err("an unconfirmed stdout write must fail");
        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            written.len() as u64,
            "a failed write must not add any bytes"
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_retry_exposes_backoff_then_clears_on_success() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = Arc::new(StdoutOutput {
            name: "stdout-retry".to_owned(),
            retry: RetryConfig {
                max_attempts: 2,
                initial_wait: std::time::Duration::from_millis(50),
                max_wait: std::time::Duration::from_millis(50),
                backoff: crate::queue::BackoffStrategy::Fixed,
            },
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal: shutdown_rx,
        });
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"retry"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (failed_tx, failed_rx) = tokio::sync::oneshot::channel();
        let failed_tx = Arc::new(std::sync::Mutex::new(Some(failed_tx)));
        let task_output = Arc::clone(&output);
        let task_attempts = Arc::clone(&attempts);
        let task = tokio::spawn(async move {
            task_output
                .consume_with_write(&event, ack, move |_| {
                    let attempt = task_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if attempt == 0 {
                        failed_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                        anyhow::bail!("scripted first failure");
                    }
                    Ok(())
                })
                .await
        });

        failed_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        tokio::time::advance(output.retry.initial_wait).await;
        task.await.unwrap().unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            output
                .metrics
                .retries
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Delivered))
        ));
    }

    #[tokio::test]
    async fn active_shutdown_clears_scripted_retry_state() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = Arc::new(StdoutOutput {
            name: "stdout-shutdown".to_owned(),
            retry: RetryConfig {
                max_attempts: 3,
                initial_wait: std::time::Duration::from_secs(5),
                max_wait: std::time::Duration::from_secs(5),
                backoff: crate::queue::BackoffStrategy::Fixed,
            },
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal: shutdown_rx,
        });
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"shutdown"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        let (failed_tx, failed_rx) = tokio::sync::oneshot::channel();
        let failed_tx = Arc::new(std::sync::Mutex::new(Some(failed_tx)));
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(async move {
            task_output
                .consume_with_write(&event, ack, move |_| {
                    if let Some(tx) = failed_tx.lock().unwrap().take() {
                        tx.send(()).unwrap();
                    }
                    anyhow::bail!("scripted persistent failure")
                })
                .await
        });
        failed_rx.await.unwrap();
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("shutdown must stop retry")
            .unwrap()
            .unwrap();
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}

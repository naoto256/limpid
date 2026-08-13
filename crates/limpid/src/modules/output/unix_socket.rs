//! Unix socket output: sends event messages to a Unix domain socket.
//! Maintains a persistent connection with automatic reconnection on failure.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::persistent_conn::{
    PersistentConn, WriteReconnectOutcome, write_with_reconnect,
    write_with_reconnect_shutdown_aware,
};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

const UNIX_SOCKET_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "path",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "expected_peer_uid",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct UnixSocketOutput {
    name: String,
    pub path: PathBuf,
    conn: Mutex<Option<UnixStream>>,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
    /// UIDs that the peer service listening on `path` may report via
    /// its socket credentials (Linux `SO_PEERCRED` / macOS
    /// `LOCAL_PEERCRED`) and still be accepted.
    ///
    /// - **Unset `expected_peer_uid`**: `{daemon euid, 0}`. Root is
    ///   trusted because the canonical peer on packaged
    ///   deployments is `journald` (`/dev/log` → root-listener) and
    ///   an attacker who can `bind` as root has already crossed a
    ///   larger trust boundary than this check defends; the check
    ///   is aimed at non-root co-tenants who `bind` a squatter
    ///   socket at the path before the real peer restarts. `syslog`
    ///   (the packaged `User=`) matches the euid arm.
    /// - **`expected_peer_uid "<name>"`**: the default set is
    ///   **replaced** (not extended) with the resolved uid alone —
    ///   root is refused too. This is the operator lock-down mode
    ///   for deployments with a known dedicated collector uid.
    ///
    /// Path-shape trust (parent owner, final-component symlink
    /// refusal, socket-shape preflight) is intentionally **not**
    /// applied to this connect-side sink: `/dev/log` is a symlink
    /// on systemd installs and its parent `/dev` is root-owned, so
    /// bind-side predicates would refuse the most common
    /// deployment.
    allowed_peer_uids: Vec<u32>,
}

impl Module for UnixSocketOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(UNIX_SOCKET_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        let properties = properties.user_properties();
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("output '{}': unix_socket requires 'path'", name))?;
        let allowed_peer_uids = match props::get_string(properties, "expected_peer_uid") {
            Some(user) => {
                // Explicit lock-down: replace the default allow set
                // with the single resolved uid. Root is refused too.
                let uid = resolve_uid(&user).with_context(|| {
                    format!(
                        "output '{}': failed to resolve expected_peer_uid '{}'",
                        name, user
                    )
                })?;
                vec![uid]
            }
            None => {
                // Default: allow the daemon's own euid plus root.
                // `{daemon euid, 0}` covers the two canonical peer
                // identities on packaged deployments (`syslog` uid
                // for the daemon-owned collector, `0` for
                // journald's `/dev/log`) while refusing any other
                // uid — the co-tenant-squatter defense.
                #[cfg(unix)]
                {
                    let self_euid = unsafe { libc::geteuid() };
                    if self_euid == 0 {
                        vec![0]
                    } else {
                        vec![self_euid, 0]
                    }
                }
                #[cfg(not(unix))]
                {
                    Vec::new()
                }
            }
        };
        Ok(Self {
            name: name.to_string(),
            path: PathBuf::from(path),
            conn: Mutex::new(None),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
            shutdown_signal: ctx.shutdown_signal.clone(),
            allowed_peer_uids,
        })
    }
}

impl HasMetrics for UnixSocketOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for UnixSocketOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            // Split the attempt into a pre-send phase (mutex lock +
            // reconnect + pre-write shutdown check) and a send phase
            // (`write_frame`). Shutdown only cancels the pre-send
            // side; a partial write that had already reached the
            // wire would otherwise be masked as `Recovered` and
            // double-sent on the next start's retry.
            let write_result = match write_with_reconnect_shutdown_aware(
                self,
                &self.conn,
                &event.egress,
                &mut shutdown,
            )
            .await
            {
                WriteReconnectOutcome::Delivered => Ok(()),
                WriteReconnectOutcome::Err(e) => Err(e),
                WriteReconnectOutcome::PreSendShutdown => {
                    self.metrics.in_retry.set(0);
                    let reason = format!(
                        "output '{}': write attempt abandoned on shutdown (pre-send)",
                        self.name
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
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
                    return Ok(());
                }
            };
            match write_result {
                Ok(()) => {
                    self.metrics.in_retry.set(0);
                    // Metric ownership stays with the caller so
                    // `finalize_shutdown_singleton_disposition` on the
                    // shutdown-drain path (which also owns the
                    // success bump) does not double-count against the
                    // transport helper. Sibling syslog_udp uses the
                    // identical shape after the previous fix.
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

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        // `write_with_reconnect` may cross the byte-on-wire
        // boundary: a stream-oriented connection can accept the
        // first frame bytes into the kernel socket buffer and even
        // ship them before a mid-write disconnect or the outer
        // `tokio::time::timeout` fires. Once the outer Elapsed
        // fires — or any inner Err arrives out of a reconnect
        // loop — the wire state is ambiguous, so route through the
        // `_ambiguous` finalizer: force `Dropped` (disk queue
        // wedges for next-start replay, memory queue falls back to
        // `Recovered`) and never fabricate the honest-Recovered
        // guarantee the transport does not support. See
        // `persistent_conn::PersistentConn::write_frame` for the
        // partial-send documentation this discipline is enforcing.
        let result = match tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            write_with_reconnect(self, &self.conn, &event.egress),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {:?}",
                crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
            )),
        };
        crate::modules::finalize_shutdown_singleton_disposition_ambiguous(
            result,
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PersistentConn for UnixSocketOutput {
    type Stream = UnixStream;

    async fn connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("unix_socket connect to {}", self.path.display()))?;
        // Peer-credential trust: the connect-side sink defends
        // against a co-tenant squatter binding on `self.path`
        // before the legitimate peer restarts. Every reconnect is
        // its own credential check — a one-shot startup validation
        // would miss the exact race window this defends. See the
        // `allowed_peer_uids` field docs on the struct for the
        // default-set semantics.
        #[cfg(unix)]
        {
            let cred = stream.peer_cred().with_context(|| {
                format!(
                    "unix_socket '{}': failed to read peer credentials on socket at {}",
                    self.name,
                    self.path.display()
                )
            })?;
            let peer_uid = cred.uid();
            if !self.allowed_peer_uids.contains(&peer_uid) {
                anyhow::bail!(
                    "unix_socket '{}': peer at {} runs as uid {}, which is not in the allowed \
                     set {:?} — refusing to ship events. This defends against a co-tenant \
                     process that bound a squatter socket at the path between the peer \
                     service's restarts; the daemon must not silently hand the failure \
                     JSONL for every event to whichever process happens to hold the socket \
                     inode. If the observed uid is the legitimate collector, set \
                     `expected_peer_uid \"<user>\"` on the output. If the observed uid is \
                     unexpected, investigate: kill the squatter or restart the intended peer.",
                    self.name,
                    self.path.display(),
                    peer_uid,
                    self.allowed_peer_uids,
                );
            }
        }
        Ok(stream)
    }

    async fn write_frame(&self, stream: &mut UnixStream, payload: &Bytes) -> Result<()> {
        // Write the payload verbatim. `String::from_utf8_lossy`
        // would silently replace non-UTF-8 bytes with U+FFFD
        // (`\xEF\xBF\xBD`), which is exactly the wrong default for
        // a security telemetry pipeline shipping opaque payloads
        // to a local collector.
        //
        // Payload bytes and the trailing `\n` are concatenated into
        // one buffer and handed to a single `write_all`, so an I/O
        // error between the payload and the delimiter — which the
        // previous two-`write_all` shape could produce as
        // `Ok(payload) → Err(delimiter)` — no longer leaves an
        // unterminated line for the retry / DLQ path to double.
        // `write_all` still loops internally, so this is a boundary
        // narrowing, not an atomicity guarantee.
        let buf = super::frame_with_newline(payload);
        stream.write_all(&buf).await?;
        stream.flush().await?;
        // After `write_all` and `flush` both return successfully,
        // record the exact framed-buffer length as adapter-confirmed;
        // this does not assert peer receipt. Callers must not
        // duplicate the bump on their success arms.
        self.metrics.bytes_written.inc_by(buf.len() as u64);
        Ok(())
    }
}

/// Resolve a username to its uid via `getpwnam_r`. Same reentrant
/// pattern the file output uses for its `owner` property (see
/// `crates/limpid/src/modules/output/file.rs::resolve_uid`). Called
/// once at construction time so runtime `connect()` never blocks
/// on NSS. Numeric-string uids fall through as invalid names and
/// surface the diagnostic — deliberate: the config surface is
/// name-based like the file output's, and mixing numeric and named
/// forms invites operator confusion about which one wins under
/// user-database rebuilds.
#[cfg(unix)]
fn resolve_uid(name: &str) -> Result<u32> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    const NSS_RECORD_BUF: usize = 4096;
    let c_name = CString::new(name)?;
    let mut buf = [0u8; NSS_RECORD_BUF];
    let mut pwd: MaybeUninit<libc::passwd> = MaybeUninit::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        anyhow::bail!("getpwnam_r failed for '{}': errno {}", name, rc);
    }
    if result.is_null() {
        anyhow::bail!("user '{}' not found", name);
    }
    // SAFETY: `getpwnam_r` returned success (`rc == 0`) and `result`
    // is non-null, which by the contract of `getpwnam_r(3)`
    // guarantees `pwd` has been fully written with a valid
    // `libc::passwd` — `result` and `pwd.as_ptr()` point to the same
    // memory. Reading through the initialised `MaybeUninit` (rather
    // than dereferencing the raw `*result`) promotes the
    // "libc initialised this" guarantee to the type system so static
    // analysis (CodeQL, miri, etc.) can see the initialisation
    // without having to reason about the FFI contract.
    let pwd = unsafe { pwd.assume_init() };
    Ok(pwd.pw_uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::queue::QueueAckHandle;
    use bytes::Bytes;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use tempfile::TempDir;

    /// Steady-state `consume` success bumps `events_written` exactly
    /// once. Regression against a metric-ownership drift that would
    /// double-count with the transport helper or leave it at zero.
    #[tokio::test]
    async fn steady_state_consume_success_bumps_events_written_once() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("out.sock");
        let listener = StdUnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _listener = tokio::net::UnixListener::from_std(listener).unwrap();

        let output = build_test_output(&socket_path).await;
        let event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.expect("consume");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "steady-state consume success must bump events_written exactly once"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful send must not bump events_failed"
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            (event.egress.len() + 1) as u64,
            "the newline in the Unix-stream frame is part of the transferred buffer"
        );
    }

    /// Shutdown-drain success via `finalize_shutdown_singleton_disposition`
    /// bumps `events_written` exactly once (previously double: once
    /// inside `write_with_reconnect` and once in the helper).
    #[tokio::test]
    async fn shutdown_consume_success_bumps_events_written_once() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("out.sock");
        let listener = StdUnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _listener = tokio::net::UnixListener::from_std(listener).unwrap();

        let output = build_test_output(&socket_path).await;
        let event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _rx) = QueueAckHandle::for_test();
        output
            .consume_shutdown(&event, ack)
            .await
            .expect("consume_shutdown");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "consume_shutdown success must bump events_written exactly once (via helper)"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful drain must not bump events_failed"
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            (event.egress.len() + 1) as u64,
            "shutdown and steady-state count the same framed buffer"
        );
    }

    #[test]
    fn concrete_writer_owns_confirmed_newline_frame_bytes() {
        fn braced_body<'a>(source: &'a str, signature: &str) -> &'a str {
            let signature_start = source.find(signature).expect("function must exist");
            let block_start = source[signature_start..]
                .find('{')
                .map(|offset| signature_start + offset)
                .expect("function must have a body");
            let mut depth = 0usize;
            for (offset, byte) in source.as_bytes()[block_start..].iter().enumerate() {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return &source[block_start..=block_start + offset];
                        }
                    }
                    _ => {}
                }
            }
            panic!("function body must be balanced");
        }

        fn assert_writer_ownership(source: &str, caller_paths: &str) {
            let writer = braced_body(source, "async fn write_frame(&self");
            let frame_call = "super::frame_with_newline(payload)";
            assert_eq!(
                writer.matches(frame_call).count(),
                1,
                "the concrete writer must construct one exact framed buffer"
            );
            let call_start = writer.find(frame_call).expect("framing call must exist");
            let binding_start = writer[..call_start]
                .rfind("let ")
                .expect("framed buffer must have a local binding");
            let statement_end = writer[call_start..]
                .find(';')
                .map(|offset| call_start + offset + 1)
                .expect("framed-buffer binding must end");
            let statement = &writer[binding_start..statement_end];
            let (left, right) = statement
                .strip_prefix("let ")
                .expect("binding must start with let")
                .split_once('=')
                .expect("binding must have an initializer");
            assert_eq!(
                right.matches(frame_call).count(),
                1,
                "the framed buffer must originate in the same let statement"
            );
            let identifier = left
                .split_once(':')
                .map_or(left, |(identifier, _)| identifier)
                .trim();
            assert!(
                !identifier.is_empty()
                    && !identifier.as_bytes()[0].is_ascii_digit()
                    && identifier
                        .bytes()
                        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()),
                "the framed buffer must use a plain local identifier"
            );

            let write_pattern = format!("write_all(&{identifier})");
            let bump_pattern = format!("bytes_written.inc_by({identifier}.len() as u64)");
            assert_eq!(
                writer.matches(&write_pattern).count(),
                1,
                "the exact framed local must flow to one write_all"
            );
            assert_eq!(
                writer.matches(&bump_pattern).count(),
                1,
                "the exact framed local must flow to one byte bump"
            );
            let write = writer
                .find(&write_pattern)
                .expect("framed write must exist");
            let flush = writer
                .find("flush().await")
                .expect("the concrete writer must flush before confirming bytes");
            let bump = writer.find(&bump_pattern).expect("byte bump must exist");
            assert!(
                statement_end <= write && write < flush && flush < bump,
                "bytes are confirmed only after the exact buffer is written and flushed"
            );
            assert!(
                !caller_paths.contains("bytes_written")
                    && !caller_paths.contains("event.egress.len() + 1"),
                "callers must not own byte bumps or reconstruct the exact newline frame length"
            );
        }

        fn assert_rejected(source: &str, caller_paths: &str) {
            assert!(
                std::panic::catch_unwind(|| assert_writer_ownership(source, caller_paths)).is_err(),
                "counterexample must fail the ownership check"
            );
        }

        let renamed = r#"
            async fn write_frame(&self, payload: &Bytes) {
                let framed_local = super::frame_with_newline(payload);
                attempt += 1;
                stream.write_all(&framed_local).await?;
                stream.flush().await?;
                self.metrics.bytes_written.inc_by(framed_local.len() as u64);
            }
            // trailing comment and unrelated helper placement are allowed.
            fn helper() { let attempt = 1 + 1; }
        "#;
        assert_writer_ownership(renamed, "attempt += 1;");
        assert_rejected(
            &renamed.replace("write_all(&framed_local)", "write_all(payload)"),
            "",
        );
        assert_rejected(
            r#"
                async fn write_frame(&self, payload: &Bytes) {
                    let framed_local = super::frame_with_newline(payload);
                    stream.write_all(&framed_local).await?;
                    self.metrics.bytes_written.inc_by(framed_local.len() as u64);
                    stream.flush().await?;
                }
            "#,
            "",
        );
        assert_rejected(renamed, "self.metrics.bytes_written.inc_by(1);");
        assert_rejected(
            r#"
                async fn write_frame(&self, payload: &Bytes) {
                    let framed_local = super::frame_with_newline(payload);
                    stream.write_all(&framed_local).await?;
                    stream.flush().await?;
                    self.metrics.bytes_written.inc_by(framed_local.len() as u64);
                    self.metrics.bytes_written.inc_by(framed_local.len() as u64);
                }
            "#,
            "",
        );
        assert_rejected(renamed, "let n = event.egress.len() + 1;");

        let src = include_str!("unix_socket.rs");
        let consume_start = src
            .find("async fn consume(&self, event: &Event")
            .expect("consume must exist");
        let impl_end = src[consume_start..]
            .find("impl PersistentConn for UnixSocketOutput")
            .map(|offset| consume_start + offset)
            .expect("PersistentConn impl must follow Output impl");
        let output_paths = &src[consume_start..impl_end];
        assert_writer_ownership(src, output_paths);
    }

    #[tokio::test]
    async fn write_frame_failure_counts_no_bytes() {
        let dir = TempDir::new().unwrap();
        let output = build_test_output(&dir.path().join("unused.sock")).await;
        let (mut writer, peer) = tokio::net::UnixStream::pair().unwrap();
        drop(peer);

        let result = <UnixSocketOutput as PersistentConn>::write_frame(
            &output,
            &mut writer,
            &Bytes::from_static(b"payload"),
        )
        .await;
        assert!(
            result.is_err(),
            "a closed peer cannot confirm transferred bytes"
        );
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn retry_counts_only_the_eventually_confirmed_frame_once() {
        use tokio::io::AsyncReadExt;

        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("retry.sock");
        let output = std::sync::Arc::new(build_test_output_with_retry(&socket_path).await);
        let event = Event::new(
            Bytes::from_static(b"retry-payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let expected = super::super::frame_with_newline(&event.egress);
        let (ack, _rx) = QueueAckHandle::for_test();
        let output_for_task = output.clone();
        let event_for_task = event.clone();
        let consume = tokio::spawn(async move {
            output_for_task.consume(&event_for_task, ack).await.unwrap();
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while output.metrics.retries.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first failed connection attempt must remain bounded");
        assert_eq!(output.metrics.in_retry.load(Ordering::Relaxed), 1);

        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let expected_len = expected.len();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut actual = vec![0; expected_len];
            stream.read_exact(&mut actual).await.unwrap();
            actual
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), consume)
            .await
            .expect("retry must complete")
            .unwrap();
        assert_eq!(receiver.await.unwrap(), expected);
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            expected.len() as u64
        );
        assert_eq!(output.metrics.events_written.load(Ordering::Relaxed), 1);
        assert_eq!(output.metrics.in_retry.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn missing_peer_counts_neither_bytes_nor_written_events() {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;

        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("missing.sock");
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![
                Property::KeyValue {
                    key: "path".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit(
                        socket_path.to_str().unwrap().to_string(),
                    )),
                    value_span: None,
                },
                Property::Block {
                    key: "retry".into(),
                    key_span: None,
                    properties: vec![Property::KeyValue {
                        key: "max_attempts".into(),
                        key_span: None,
                        value: Expr::spanless(ExprKind::IntLit(1)),
                        value_span: None,
                    }],
                },
            ],
        );
        let output = UnixSocketOutput::from_properties(
            "unix-failure",
            &props,
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        let event = Event::new(
            Bytes::from_static(b"unreachable"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _ack_rx) = QueueAckHandle::for_test();

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            output.consume(&event, ack),
        )
        .await
        .expect("missing peer failure must remain bounded")
        .unwrap();

        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.events_written.load(Ordering::Relaxed), 0);
    }

    /// Structural pin: `consume_shutdown` routes its result through
    /// the `_ambiguous` finalizer, not the plain
    /// `finalize_shutdown_singleton_disposition`. The plain variant
    /// resolves shutdown-time send failures as `Recovered` — which
    /// on a disk queue advances the cursor past a DLQ record even
    /// though the payload's bytes may already have been written to
    /// the peer socket (kernel buffer, first frame partially on
    /// wire, or reconnect-mid-write). That produces a double-state:
    /// the downstream received part or all of the record AND the
    /// DLQ record is available for replay. The `_ambiguous`
    /// variant force-Dropped-so-wedged, holding the cursor for
    /// operator reconciliation on next start.
    #[test]
    fn consume_shutdown_uses_ambiguous_finalizer() {
        let src = include_str!("unix_socket.rs");
        let start = src
            .find("async fn consume_shutdown(")
            .expect("consume_shutdown fn must exist");
        let end_offset = src[start..]
            .find("\n}\n")
            .expect("consume_shutdown body must end with a top-level closing brace");
        let body = &src[start..start + end_offset];
        assert!(
            body.contains("finalize_shutdown_singleton_disposition_ambiguous("),
            "consume_shutdown must route through the _ambiguous finalizer to hold the disk \
             cursor on partial-wire failures — the plain variant would honest-Recovered and \
             risk downstream duplicates on next-start replay."
        );
        assert!(
            !body.contains("crate::modules::finalize_shutdown_singleton_disposition("),
            "consume_shutdown must not fall back to the plain finalizer — the wire state \
             cannot be proved pre-boundary from outside the write helper."
        );
    }

    /// Default policy pin: with no `expected_peer_uid` configured
    /// the allow set is `{daemon euid, 0}`. A non-root daemon
    /// carries both entries so `journald` (`/dev/log` = root) and
    /// a daemon-owned collector are both accepted; a root daemon
    /// collapses to just `[0]`.
    #[test]
    fn default_allow_set_is_daemon_euid_plus_root() {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![Property::KeyValue {
                key: "path".into(),
                key_span: None,
                value: Expr::spanless(ExprKind::StringLit("/tmp/never.sock".into())),
                value_span: None,
            }],
        );
        let out = UnixSocketOutput::from_properties(
            "u",
            &props,
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");
        let self_euid = unsafe { libc::geteuid() };
        if self_euid == 0 {
            assert_eq!(
                out.allowed_peer_uids,
                vec![0],
                "root daemon: default allow set collapses to [0]"
            );
        } else {
            assert!(
                out.allowed_peer_uids.contains(&self_euid) && out.allowed_peer_uids.contains(&0),
                "non-root daemon: default allow set must contain both euid and 0; got {:?}",
                out.allowed_peer_uids
            );
            assert_eq!(
                out.allowed_peer_uids.len(),
                2,
                "default allow set must be exactly {{euid, 0}}"
            );
        }
    }

    /// `expected_peer_uid` **replaces** the default allow set — root
    /// is refused too. Pin the strict-mode semantics against the
    /// operator-facing docstring.
    #[test]
    fn expected_peer_uid_replaces_default_set() {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        // Try `nobody` first; fall through to the current user if
        // the test image doesn't have a `nobody` entry (Docker
        // scratch, minimal Alpine, some CI runners).
        let user_name: String = if resolve_uid("nobody").is_ok() {
            "nobody".into()
        } else {
            unsafe {
                let euid = libc::geteuid();
                let pwd = libc::getpwuid(euid);
                if pwd.is_null() {
                    panic!("cannot resolve current user for test — no `nobody` either");
                }
                std::ffi::CStr::from_ptr((*pwd).pw_name)
                    .to_str()
                    .expect("username is UTF-8")
                    .to_string()
            }
        };
        let expected_uid = resolve_uid(&user_name).expect("resolve test user");
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![
                Property::KeyValue {
                    key: "path".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit("/tmp/never.sock".into())),
                    value_span: None,
                },
                Property::KeyValue {
                    key: "expected_peer_uid".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit(user_name.clone())),
                    value_span: None,
                },
            ],
        );
        let out = UnixSocketOutput::from_properties(
            "u",
            &props,
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build");
        assert_eq!(
            out.allowed_peer_uids,
            vec![expected_uid],
            "expected_peer_uid must REPLACE the default set (root not implicitly included)"
        );
    }

    /// A misspelt `expected_peer_uid` surfaces at build time, not
    /// at first connect. Prevents a config typo from turning into a
    /// silent socket-squatter acceptance later.
    #[test]
    fn expected_peer_uid_unknown_user_fails_at_build() {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![
                Property::KeyValue {
                    key: "path".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit("/tmp/never.sock".into())),
                    value_span: None,
                },
                Property::KeyValue {
                    key: "expected_peer_uid".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit(
                        "definitely_not_a_real_user_xyzzy".into(),
                    )),
                    value_span: None,
                },
            ],
        );
        let err = match UnixSocketOutput::from_properties(
            "u",
            &props,
            &crate::modules::BuildContext::for_testing(),
        ) {
            Ok(_) => panic!("build must fail on unresolvable expected_peer_uid"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("expected_peer_uid") && msg.contains("definitely_not_a_real_user_xyzzy"),
            "diagnostic must name the property and the offending value: {msg}"
        );
    }

    /// Structural pin: `PersistentConn::connect` verifies the peer
    /// credential against `self.allowed_peer_uids` on every call.
    /// Behavioural coverage for the *reject* arm needs a listener
    /// running under a different uid, which is root-only; pin the
    /// shape at the source so a mechanical refactor that strips
    /// the check fails at test time. The default-set *accept* arm
    /// is exercised by every existing e2e test in this module,
    /// which runs its listener under the process euid.
    #[test]
    fn connect_verifies_peer_uid_against_allowed_set() {
        let src = include_str!("unix_socket.rs");
        // Anchor on the `PersistentConn for UnixSocketOutput` impl
        // header so the `async fn connect(` we grab is the one on
        // the trait impl, not any incidental mention elsewhere.
        let impl_start = src
            .find("impl PersistentConn for UnixSocketOutput {")
            .expect("PersistentConn impl must exist");
        let connect_start = src[impl_start..]
            .find("async fn connect(")
            .map(|off| impl_start + off)
            .expect("connect fn inside the impl block");
        let body_end = src[connect_start..]
            .find("async fn write_frame")
            .expect("connect body ends before write_frame");
        let body = &src[connect_start..connect_start + body_end];
        assert!(
            body.contains("peer_cred()"),
            "connect must call peer_cred() on the freshly-connected socket"
        );
        assert!(
            body.contains("allowed_peer_uids.contains("),
            "connect must reject a peer whose uid is not in self.allowed_peer_uids"
        );
    }

    async fn build_test_output(socket_path: &std::path::Path) -> UnixSocketOutput {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![Property::KeyValue {
                key: "path".into(),
                key_span: None,
                value: Expr::spanless(ExprKind::StringLit(
                    socket_path.to_str().unwrap().to_string(),
                )),
                value_span: None,
            }],
        );
        UnixSocketOutput::from_properties("u", &props, &crate::modules::BuildContext::for_testing())
            .expect("build")
    }

    async fn build_test_output_with_retry(socket_path: &std::path::Path) -> UnixSocketOutput {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![
                Property::KeyValue {
                    key: "path".into(),
                    key_span: None,
                    value: Expr::spanless(ExprKind::StringLit(
                        socket_path.to_str().unwrap().to_string(),
                    )),
                    value_span: None,
                },
                Property::Block {
                    key: "retry".into(),
                    key_span: None,
                    properties: vec![
                        Property::KeyValue {
                            key: "max_attempts".into(),
                            key_span: None,
                            value: Expr::spanless(ExprKind::IntLit(3)),
                            value_span: None,
                        },
                        Property::KeyValue {
                            key: "initial_wait".into(),
                            key_span: None,
                            value: Expr::spanless(ExprKind::StringLit("20ms".into())),
                            value_span: None,
                        },
                        Property::KeyValue {
                            key: "max_wait".into(),
                            key_span: None,
                            value: Expr::spanless(ExprKind::StringLit("20ms".into())),
                            value_span: None,
                        },
                    ],
                },
            ],
        );
        UnixSocketOutput::from_properties(
            "u-retry",
            &props,
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("build")
    }
}

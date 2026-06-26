//! Syslog TCP input with RFC 6587 framing (octet counting + non-transparent),
//! with optional TLS termination (mTLS supported via `tls { ca cert key }`).
//!
//! When the `tls { ... }` block is present, the listener terminates TLS
//! before applying the same framing logic; without it, the listener
//! reads plaintext directly. Default port flips per configuration:
//! 6514 (RFC 5425) when TLS is enabled, 514 (RFC 6587) otherwise.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, info, warn};

use super::rate_limit::RateLimiter;
use super::validate::validate_pri;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};
use crate::tls::TlsConfig;

const SYSLOG_TCP_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "bind",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    // `auto` is the default — autodetect framing per connection from
    // the first byte (digit → octet counting, `<` → non-transparent).
    PropertySpec {
        name: "framing",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["auto", "octet_counting", "non_transparent"]),
    },
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(crate::tls::TLS_SERVER_BLOCK_PROPERTIES),
    },
    PropertySpec {
        name: "rate_limit",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "max_connections",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

// ---------------------------------------------------------------------------
// AcceptedStream — plaintext TCP or TLS, behind one AsyncRead façade
// ---------------------------------------------------------------------------

/// Per-connection stream, polymorphic over plaintext / TLS. Both
/// variants implement [`AsyncRead`] so framing helpers
/// (`detect_framing`, `read_octet_counting`, `read_non_transparent`)
/// don't branch on the transport.
///
/// The TLS variant is boxed because `TlsStream` carries rustls
/// connection state much larger than a bare `TcpStream`; leaving it
/// inline would inflate every `BufReader` allocation including
/// plaintext connections.
pub(crate) enum AcceptedStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for AcceptedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AcceptedStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            AcceptedStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

/// Maximum size of a single syslog message (bytes).
/// RFC 5424 recommends supporting at least 2048; we allow up to 1 MiB.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Maximum digits in an octet-counting MSG-LEN field.
/// 7 digits covers up to 9_999_999 (well above MAX_MESSAGE_SIZE).
const MAX_MSG_LEN_DIGITS: usize = 7;

/// Idle timeout per connection. If no data arrives within this window,
/// the connection is dropped. Prevents resource leaks from dead peers.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Upper bound on the TLS handshake. A client that opens TCP but never
/// finishes the handshake (slow handshakes happen on real networks;
/// hung-mid-handshake attempts happen on hostile ones) would otherwise
/// pin a task on `acceptor.accept().await` forever and consume one of
/// the `max_connections` slots. 10 s is loose enough for high-latency
/// links + a slow CPU on the client and tight enough that a handful of
/// stalled handshakes can't exhaust the slot pool.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Syslog TCP framing method per RFC 6587.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpFraming {
    /// Auto-detect per RFC 6587: digit → octet counting, '<' → LF-delimited.
    Auto,
    /// RFC 6587 §3.4.1 — `MSG-LEN SP SYSLOG-MSG`
    OctetCounting,
    /// RFC 6587 §3.4.2 — messages terminated by LF (or CRLF / NUL)
    NonTransparent,
}

/// Syslog TCP input: listens on a TCP socket and produces Events.
///
/// Supports both framing methods defined in RFC 6587:
/// - **Octet counting**: each message is prefixed with its byte length
/// - **Non-transparent framing**: messages are delimited by LF/CRLF/NUL
///
/// By default, the framing is auto-detected per connection based on the
/// first byte received (digit → octet counting, '<' → non-transparent).
/// Default maximum simultaneous connections per TCP/TLS listener.
pub const DEFAULT_MAX_CONNECTIONS: u64 = 1024;

pub struct SyslogTcpInput {
    pub bind_addr: String,
    pub framing: TcpFraming,
    /// `Some` enables TLS termination on this listener (mTLS when
    /// `ca_path` is set). `None` keeps the listener in plaintext
    /// mode.
    pub tls_config: Option<TlsConfig>,
    pub rate_limit: Option<u64>,
    pub max_connections: usize,
    metrics: Arc<InputMetrics>,
}

impl Module for SyslogTcpInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_TCP_INPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::modules::ModuleProperties,
        _ctx: &crate::modules::BuildContext,
    ) -> anyhow::Result<Self> {
        let properties = properties.user_properties();
        let tls_config =
            TlsConfig::from_properties_block(&format!("input '{}'", name), properties)?;
        // The crypto provider must be installed before rustls server
        // config assembly. Only pay the cost when TLS is actually
        // configured for this input.
        if tls_config.is_some() {
            crate::tls::install_default_crypto_provider();
        }
        // RFC 5425 (TLS) → 6514, RFC 6587 (TCP plain) → 514. The
        // default flips with the `tls` block presence so configs
        // typically don't need an explicit `port`.
        let default_port = if tls_config.is_some() { 6514 } else { 514 };
        let bind = props::get_string(properties, "bind")
            .unwrap_or_else(|| format!("0.0.0.0:{}", default_port));
        let framing = match props::get_ident(properties, "framing").as_deref() {
            Some("octet_counting") => TcpFraming::OctetCounting,
            Some("non_transparent") => TcpFraming::NonTransparent,
            // `auto` (explicit) or absent — both fall through to the
            // per-connection autodetect. Any other value would have
            // been rejected by the schema validator before we reach
            // this point.
            _ => TcpFraming::Auto,
        };
        let rate_limit = props::get_strictly_positive_int(properties, "rate_limit")?;
        let max_connections = props::get_positive_int(properties, "max_connections")?
            .unwrap_or(DEFAULT_MAX_CONNECTIONS) as usize;
        Ok(Self {
            bind_addr: bind,
            framing,
            tls_config,
            max_connections,
            rate_limit,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for SyslogTcpInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for SyslogTcpInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        // Build the TLS acceptor up-front (rather than per-connection)
        // so cert / key parse errors fail-fast at daemon start.
        let acceptor = match self.tls_config {
            Some(ref tls) => {
                let server_config = crate::tls::build_server_config(tls).await?;
                Some(TlsAcceptor::from(server_config))
            }
            None => None,
        };

        let listener = TcpListener::bind(&self.bind_addr).await?;
        info!(
            "syslog_tcp listening on {} ({})",
            self.bind_addr,
            if acceptor.is_some() {
                "TLS"
            } else {
                "plaintext"
            }
        );

        let limiter: Option<Arc<RateLimiter>> = self.rate_limit.map(|r| {
            info!("syslog_tcp rate_limit: {} events/sec", r);
            Arc::new(RateLimiter::new(r))
        });

        let mut conn_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        loop {
            // Clean up finished connection handles periodically
            conn_handles.retain(|h| !h.is_finished());

            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("syslog_tcp: shutting down ({} active connections)", conn_handles.len());
                        for h in &conn_handles {
                            h.abort();
                        }
                        break;
                    }
                }

                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            if conn_handles.len() >= self.max_connections {
                                warn!("syslog_tcp: max connections ({}) reached, rejecting {}", self.max_connections, addr);
                                drop(stream);
                                continue;
                            }
                            let tx = tx.clone();
                            let framing = self.framing;
                            let limiter = limiter.clone();
                            let metrics = Arc::clone(&self.metrics);
                            let acceptor = acceptor.clone();
                            debug!("syslog_tcp: new connection from {}", addr);
                            conn_handles.push(tokio::spawn(async move {
                                let accepted = match acceptor {
                                    Some(acceptor) => {
                                        // Bound the TLS handshake: a client that
                                        // opens TCP but never finishes the
                                        // handshake (slow / malicious) would
                                        // otherwise pin a task on this stalled
                                        // future and burn one of the
                                        // `max_connections` slots until something
                                        // unrelated kills it. 10 s is loose
                                        // enough for normal TLS over high-latency
                                        // links and short enough that an attacker
                                        // can't trivially exhaust the slot pool.
                                        match tokio::time::timeout(
                                            TLS_HANDSHAKE_TIMEOUT,
                                            acceptor.accept(stream),
                                        )
                                        .await
                                        {
                                            Ok(Ok(s)) => {
                                                debug!("syslog_tcp [{}]: TLS handshake complete", addr);
                                                AcceptedStream::Tls(Box::new(s))
                                            }
                                            Ok(Err(e)) => {
                                                warn!(
                                                    "syslog_tcp [{}]: TLS handshake failed: {}",
                                                    addr, e
                                                );
                                                return;
                                            }
                                            Err(_) => {
                                                warn!(
                                                    "syslog_tcp [{}]: TLS handshake timed out after {:?}",
                                                    addr, TLS_HANDSHAKE_TIMEOUT
                                                );
                                                return;
                                            }
                                        }
                                    }
                                    None => AcceptedStream::Plain(stream),
                                };
                                let mut reader = BufReader::new(accepted);

                                // Detect framing if Auto
                                let effective_framing = if framing == TcpFraming::Auto {
                                    match detect_framing(&mut reader, addr).await {
                                        Some(f) => {
                                            debug!("syslog_tcp [{}]: detected framing {:?}", addr, f);
                                            f
                                        }
                                        None => {
                                            debug!("syslog_tcp [{}]: closed before any data", addr);
                                            return;
                                        }
                                    }
                                } else {
                                    framing
                                };

                                let reason = match effective_framing {
                                    TcpFraming::OctetCounting => {
                                        read_octet_counting(&mut reader, addr, &tx, limiter.as_deref(), Some(&metrics)).await
                                    }
                                    TcpFraming::NonTransparent => {
                                        read_non_transparent(&mut reader, addr, &tx, limiter.as_deref(), Some(&metrics)).await
                                    }
                                    TcpFraming::Auto => unreachable!(),
                                };

                                debug!("syslog_tcp [{}]: connection closed ({})", addr, reason);
                            }));
                        }
                        Err(e) => {
                            error!("syslog_tcp accept error: {}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Reason a connection was closed — returned for logging.
#[derive(Debug)]
pub(crate) enum CloseReason {
    Eof,
    Timeout,
    ChannelClosed,
    IoError(std::io::Error),
    FramingError(String),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseReason::Eof => write!(f, "EOF"),
            CloseReason::Timeout => write!(f, "idle timeout"),
            CloseReason::ChannelClosed => write!(f, "pipeline channel closed"),
            CloseReason::IoError(e) => write!(f, "I/O error: {}", e),
            CloseReason::FramingError(msg) => write!(f, "framing error: {}", msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Framing detection
// ---------------------------------------------------------------------------

/// Peek at the first byte to determine framing per RFC 6587:
/// - Digit (0x31–0x39) → octet counting
/// - '<' (0x3C) → non-transparent framing
pub(crate) async fn detect_framing<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    addr: SocketAddr,
) -> Option<TcpFraming> {
    // Apply timeout to the initial peek as well
    let result = tokio::time::timeout(IDLE_TIMEOUT, reader.fill_buf()).await;
    let buf = match result {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => {
            warn!(
                "syslog_tcp [{}]: I/O error during framing detection: {}",
                addr, e
            );
            return None;
        }
        Err(_) => {
            warn!("syslog_tcp [{}]: timeout waiting for first byte", addr);
            return None;
        }
    };

    if buf.is_empty() {
        return None;
    }

    let first = buf[0];
    if matches!(first, b'1'..=b'9') {
        // RFC 6587: MSG-LEN = NONZERO-DIGIT *DIGIT — must start with 1-9
        Some(TcpFraming::OctetCounting)
    } else {
        // '<' or anything else → non-transparent
        Some(TcpFraming::NonTransparent)
    }
}

// ---------------------------------------------------------------------------
// Octet Counting (RFC 6587 §3.4.1)
// ---------------------------------------------------------------------------

/// ```text
/// TCP-DATA     = *SYSLOG-FRAME
/// SYSLOG-FRAME = MSG-LEN SP SYSLOG-MSG
/// MSG-LEN      = NONZERO-DIGIT *DIGIT
/// ```
///
/// Validation:
/// - MSG-LEN must start with a non-zero digit (no leading zeros)
/// - MSG-LEN must not exceed MAX_MESSAGE_SIZE
/// - SYSLOG-MSG must start with '<' (PRI header) — framing integrity check
pub(crate) async fn read_octet_counting<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    addr: SocketAddr,
    tx: &tokio::sync::mpsc::Sender<Event>,
    limiter: Option<&RateLimiter>,
    metrics: Option<&InputMetrics>,
) -> CloseReason {
    loop {
        // --- Read MSG-LEN SP ---
        let msg_len = match read_msg_len(reader, addr).await {
            Ok(n) => n,
            Err(reason) => return reason,
        };

        // --- Read SYSLOG-MSG (exactly msg_len bytes) ---
        let mut buf = vec![0u8; msg_len];
        match tokio::time::timeout(IDLE_TIMEOUT, reader.read_exact(&mut buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return CloseReason::Eof;
            }
            Ok(Err(e)) => return CloseReason::IoError(e),
            Err(_) => return CloseReason::Timeout,
        }

        // --- Validate PRI (RFC 5424 §6.2.1) ---
        if let Err(e) = validate_pri(&buf) {
            warn!(
                "syslog_tcp [{}]: invalid syslog message ({}), \
                 framing likely corrupted — dropping connection",
                addr, e,
            );
            if let Some(m) = metrics {
                m.events_invalid
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return CloseReason::FramingError(format!("invalid PRI: {}", e));
        }

        if let Some(limiter) = limiter {
            limiter.acquire().await;
        }

        let raw = Bytes::from(buf);
        let event = Event::new(raw, addr);
        if tx.send(event).await.is_err() {
            return CloseReason::ChannelClosed;
        }
        if let Some(m) = metrics {
            m.events_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Read and validate the `MSG-LEN SP` prefix.
///
/// ABNF: `MSG-LEN = NONZERO-DIGIT *DIGIT`
/// - First digit must be 1–9 (no leading zeros, no bare "0")
/// - Total digits capped at MAX_MSG_LEN_DIGITS
/// - Value must be ≤ MAX_MESSAGE_SIZE
async fn read_msg_len<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    addr: SocketAddr,
) -> std::result::Result<usize, CloseReason> {
    let mut len_buf = [0u8; 1];
    let mut len_str = String::with_capacity(8);

    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, reader.read_exact(&mut len_buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(CloseReason::Eof);
            }
            Ok(Err(e)) => return Err(CloseReason::IoError(e)),
            Err(_) => return Err(CloseReason::Timeout),
        }

        let byte = len_buf[0];

        if byte == b' ' {
            // End of MSG-LEN
            if len_str.is_empty() {
                warn!("syslog_tcp [{}]: empty MSG-LEN before SP", addr);
                return Err(CloseReason::FramingError("empty MSG-LEN".into()));
            }
            break;
        }

        if !byte.is_ascii_digit() {
            warn!(
                "syslog_tcp [{}]: non-digit byte 0x{:02x} in MSG-LEN",
                addr, byte
            );
            return Err(CloseReason::FramingError(format!(
                "non-digit 0x{:02x} in MSG-LEN",
                byte
            )));
        }

        // NONZERO-DIGIT: first digit must not be '0'
        if len_str.is_empty() && byte == b'0' {
            warn!(
                "syslog_tcp [{}]: MSG-LEN starts with '0' (invalid per RFC 6587)",
                addr
            );
            return Err(CloseReason::FramingError("MSG-LEN leading zero".into()));
        }

        len_str.push(byte as char);

        if len_str.len() > MAX_MSG_LEN_DIGITS {
            warn!(
                "syslog_tcp [{}]: MSG-LEN too many digits ({})",
                addr, len_str
            );
            return Err(CloseReason::FramingError("MSG-LEN too many digits".into()));
        }
    }

    let msg_len: usize = len_str.parse().map_err(|_| {
        CloseReason::FramingError(format!("MSG-LEN '{}' is not a valid number", len_str))
    })?;

    if msg_len > MAX_MESSAGE_SIZE {
        warn!(
            "syslog_tcp [{}]: MSG-LEN {} exceeds limit {}",
            addr, msg_len, MAX_MESSAGE_SIZE
        );
        return Err(CloseReason::FramingError(format!(
            "MSG-LEN {} exceeds limit",
            msg_len
        )));
    }

    Ok(msg_len)
}

// ---------------------------------------------------------------------------
// Non-Transparent Framing (RFC 6587 §3.4.2)
// ---------------------------------------------------------------------------

/// Messages terminated by a TRAILER, typically LF.
///
/// Supported trailers: LF (%d10), CRLF (%d13.10), NUL (%d00).
///
/// We read byte-by-byte from the buffered reader to handle both LF and NUL
/// as delimiters. A max message size is enforced to prevent memory exhaustion
/// when a peer sends data without any delimiter.
pub(crate) async fn read_non_transparent<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    addr: SocketAddr,
    tx: &tokio::sync::mpsc::Sender<Event>,
    limiter: Option<&RateLimiter>,
    metrics: Option<&InputMetrics>,
) -> CloseReason {
    let mut buf = Vec::with_capacity(4096);

    loop {
        buf.clear();

        // Read until we hit LF, NUL, or EOF
        match read_until_delimiter(reader, &mut buf, addr).await {
            Ok(false) => {
                // EOF — emit any remaining data as final message
                if !buf.is_empty() {
                    // Strip trailing CR
                    while buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    if !buf.is_empty() {
                        if let Err(e) = validate_pri(&buf) {
                            warn!(
                                "syslog_tcp [{}]: invalid syslog message at EOF ({})",
                                addr, e
                            );
                            if let Some(m) = metrics {
                                m.events_invalid
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        } else {
                            if let Some(limiter) = limiter {
                                limiter.acquire().await;
                            }
                            let raw = Bytes::from(buf.clone());
                            let event = Event::new(raw, addr);
                            if tx.send(event).await.is_err() {
                                return CloseReason::ChannelClosed;
                            }
                            if let Some(m) = metrics {
                                m.events_received
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                }
                return CloseReason::Eof;
            }
            Ok(true) => {
                // Delimiter found — buf contains message without trailer
            }
            Err(reason) => return reason,
        }

        // Strip trailing CR (for CRLF case)
        while buf.last() == Some(&b'\r') {
            buf.pop();
        }

        if buf.is_empty() {
            continue; // skip empty lines
        }

        // Validate PRI (RFC 5424 §6.2.1)
        if let Err(e) = validate_pri(&buf) {
            warn!(
                "syslog_tcp [{}]: invalid syslog message ({}), \
                 dropping connection",
                addr, e,
            );
            if let Some(m) = metrics {
                m.events_invalid
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return CloseReason::FramingError(format!("invalid PRI: {}", e));
        }

        if let Some(limiter) = limiter {
            limiter.acquire().await;
        }

        let raw = Bytes::copy_from_slice(&buf);
        let event = Event::new(raw, addr);
        if tx.send(event).await.is_err() {
            return CloseReason::ChannelClosed;
        }
        if let Some(m) = metrics {
            m.events_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Read bytes into `buf` until LF (%d10) or NUL (%d00) is encountered.
/// The delimiter itself is NOT included in `buf`.
///
/// Returns `Ok(true)` if a delimiter was found, `Ok(false)` on EOF.
async fn read_until_delimiter<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    addr: SocketAddr,
) -> std::result::Result<bool, CloseReason> {
    loop {
        // Use fill_buf + consume to leverage the BufReader's internal buffer
        // for efficiency (avoid 1-byte syscalls).
        let available = match tokio::time::timeout(IDLE_TIMEOUT, reader.fill_buf()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(CloseReason::IoError(e)),
            Err(_) => return Err(CloseReason::Timeout),
        };

        if available.is_empty() {
            return Ok(false); // EOF
        }

        // Scan for LF or NUL in the available buffer
        if let Some(pos) = available.iter().position(|&b| b == b'\n' || b == b'\0') {
            // Append everything before the delimiter
            buf.extend_from_slice(&available[..pos]);
            // Consume up to and including the delimiter
            let consume_len = pos + 1;
            reader.consume(consume_len);
            return Ok(true);
        }

        // No delimiter found — append all available bytes
        let len = available.len();

        // Enforce max message size
        if buf.len() + len > MAX_MESSAGE_SIZE {
            warn!(
                "syslog_tcp [{}]: message exceeds {} bytes without delimiter — \
                 dropping connection",
                addr, MAX_MESSAGE_SIZE
            );
            return Err(CloseReason::FramingError(
                "message too large without delimiter".into(),
            ));
        }

        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn addr() -> SocketAddr {
        "127.0.0.1:514".parse().unwrap()
    }

    // --- Octet Counting ---

    #[tokio::test]
    async fn test_oc_basic() {
        let data = b"10 <134>hello18 <165>world message";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        let e1 = rx.recv().await.unwrap();
        assert_eq!(&*e1.ingress, b"<134>hello");

        let e2 = rx.recv().await.unwrap();
        assert_eq!(&*e2.ingress, b"<165>world message");

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_oc_rejects_missing_pri() {
        // MSG-LEN says 5 bytes, but content doesn't start with '<'
        let data = b"5 hello";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    #[tokio::test]
    async fn test_oc_rejects_leading_zero() {
        let data = b"010 <134>hello";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    #[tokio::test]
    async fn test_oc_rejects_non_digit() {
        let data = b"1x <134>hi";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    #[tokio::test]
    async fn test_oc_rejects_empty_len() {
        let data = b" <134>hi";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    #[tokio::test]
    async fn test_oc_eof_mid_message() {
        // MSG-LEN says 20 but only 10 bytes of payload follow
        let data = b"20 <134>hello";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::Eof));
    }

    // --- Non-Transparent Framing ---

    #[tokio::test]
    async fn test_ntf_lf() {
        let data = b"<134>hello\n<165>world\n";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<134>hello");
        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<165>world");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_ntf_crlf() {
        let data = b"<134>msg1\r\n<165>msg2\r\n";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<134>msg1");
        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<165>msg2");
    }

    #[tokio::test]
    async fn test_ntf_nul_delimiter() {
        let data = b"<134>msg1\0<165>msg2\0";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<134>msg1");
        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<165>msg2");
    }

    #[tokio::test]
    async fn test_ntf_eof_without_trailer() {
        // Message without trailing delimiter — should still be emitted
        let data = b"<134>unterminated";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<134>unterminated");
    }

    #[tokio::test]
    async fn test_ntf_skips_empty_lines() {
        let data = b"\n\n<134>hello\n\n";
        let mut reader = BufReader::new(&data[..]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        drop(tx);
        assert!(matches!(reason, CloseReason::Eof));

        assert_eq!(&*rx.recv().await.unwrap().ingress, b"<134>hello");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_oc_rejects_invalid_pri_value() {
        // Valid framing but PRI > 191
        let data = b"8 <999>msg";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_octet_counting(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    #[tokio::test]
    async fn test_ntf_rejects_invalid_pri() {
        let data = b"garbage data\n";
        let mut reader = BufReader::new(&data[..]);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let reason = read_non_transparent(&mut reader, addr(), &tx, None, None).await;
        assert!(matches!(reason, CloseReason::FramingError(_)));
    }

    // --- Framing detection ---

    #[tokio::test]
    async fn test_detect_octet_counting() {
        let data = b"123 <134>msg";
        let mut reader = BufReader::new(&data[..]);
        let framing = detect_framing(&mut reader, addr()).await;
        assert_eq!(framing, Some(TcpFraming::OctetCounting));
    }

    #[tokio::test]
    async fn test_detect_non_transparent() {
        let data = b"<134>hello\n";
        let mut reader = BufReader::new(&data[..]);
        let framing = detect_framing(&mut reader, addr()).await;
        assert_eq!(framing, Some(TcpFraming::NonTransparent));
    }

    #[tokio::test]
    async fn test_detect_empty() {
        let data = b"";
        let mut reader = BufReader::new(&data[..]);
        let framing = detect_framing(&mut reader, addr()).await;
        assert_eq!(framing, None);
    }

    // -------------------------------------------------------------------
    // from_properties — TLS block + per-config default port
    // -------------------------------------------------------------------

    use crate::dsl::ast::{Expr, ExprKind, Property};
    use tempfile::TempDir;

    struct PemFiles {
        _dir: TempDir,
        cert: String,
        key: String,
    }

    fn pem_files() -> PemFiles {
        let dir = TempDir::new().unwrap();
        let cert_params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("valid CN");
        let key_pair = rcgen::KeyPair::generate().expect("key gen");
        let cert = cert_params.self_signed(&key_pair).expect("self-sign");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
        PemFiles {
            _dir: dir,
            cert: cert_path.display().to_string(),
            key: key_path.display().to_string(),
        }
    }

    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("syslog_tcp", props.to_vec())
    }

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

    #[test]
    fn tls_handshake_timeout_is_sensible() {
        // Regression guard: the constant exists, is non-zero, and sits
        // in a band that doesn't trivially exhaust the connection pool
        // (≤ 30 s) while still tolerating real-world high-latency
        // links + slow client CPUs (≥ 1 s). A full timing test would
        // require either a 10 s real-wait or a paused-time harness
        // tangled around the runtime's task spawning; neither pays
        // for itself given the change is a mechanical tokio::time::
        // timeout wrap. Code review + this bound check catch the
        // regression modes that matter (const removed, set to 0,
        // set to a multi-minute value that lets attackers fill the
        // slot pool with stalled handshakes).
        assert!(TLS_HANDSHAKE_TIMEOUT >= Duration::from_secs(1));
        assert!(TLS_HANDSHAKE_TIMEOUT <= Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn tls_handshake_timeout_fires_against_stalled_client() {
        // The motivating slot-exhaustion vector for this timeout is
        // a TCP-only client that connects (consuming a slot) but
        // never sends the TLS ClientHello. Without the
        // `tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, …)` wrap, the
        // accept future stays pending forever and the slot is held
        // until something unrelated kills the task. The existing
        // bound-check test guards against the constant being removed
        // or set to a nonsensical value; this test exercises the
        // firing path directly, using the same `TlsAcceptor` +
        // `tokio::time::timeout` pair the production accept loop
        // calls.
        crate::tls::install_default_crypto_provider();
        let files = pem_files();
        let tls_cfg = crate::tls::TlsConfig {
            cert_path: files.cert.clone(),
            key_path: files.key.clone(),
            ca_path: None,
        };
        let server_config = crate::tls::build_server_config(&tls_cfg)
            .await
            .expect("build server config");
        let acceptor = TlsAcceptor::from(server_config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let accept_task = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept");
            // Same wrap as the production accept loop in `run`:
            // bound the handshake at TLS_HANDSHAKE_TIMEOUT so a
            // stalled client cannot pin the slot indefinitely.
            tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
        });

        // Bare TCP connect — never speaks TLS. The acceptor sits
        // waiting for the ClientHello forever; the timeout wrap is
        // the only thing that releases the slot.
        let _stalled_client = TcpStream::connect(addr).await.expect("tcp connect");

        // Let the spawned accept task make progress (real-time
        // TCP connect already returned), then jump virtual time
        // past the handshake timeout so the wrap fires.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(TLS_HANDSHAKE_TIMEOUT + Duration::from_secs(1)).await;

        let result = accept_task.await.expect("task join");
        assert!(
            result.is_err(),
            "expected timeout Err, got Ok({:?})",
            result.is_ok()
        );
    }

    #[test]
    fn build_plaintext_defaults_to_port_514() {
        let i = SyslogTcpInput::build(
            "relay",
            &mp(&[]),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("should build");
        assert_eq!(i.bind_addr, "0.0.0.0:514");
        assert!(i.tls_config.is_none());
    }

    #[test]
    fn build_with_tls_defaults_to_port_6514() {
        let files = pem_files();
        let props = vec![block(
            "tls",
            vec![
                kv("cert", ExprKind::StringLit(files.cert.clone())),
                kv("key", ExprKind::StringLit(files.key.clone())),
            ],
        )];
        let i = SyslogTcpInput::build(
            "relay",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("should build");
        assert_eq!(i.bind_addr, "0.0.0.0:6514");
        let tls = i.tls_config.as_ref().expect("tls present");
        assert_eq!(tls.cert_path, files.cert);
        assert_eq!(tls.key_path, files.key);
        assert!(tls.ca_path.is_none());
    }

    #[test]
    fn build_with_mtls_records_ca_path() {
        let files = pem_files();
        let props = vec![block(
            "tls",
            vec![
                kv("cert", ExprKind::StringLit(files.cert.clone())),
                kv("key", ExprKind::StringLit(files.key.clone())),
                kv("ca", ExprKind::StringLit(files.cert.clone())),
            ],
        )];
        let i = SyslogTcpInput::build(
            "relay",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("should build");
        let tls = i.tls_config.as_ref().expect("tls present");
        assert_eq!(tls.ca_path.as_deref(), Some(files.cert.as_str()));
    }

    #[test]
    fn build_with_tls_rejects_missing_cert() {
        let files = pem_files();
        let props = vec![block(
            "tls",
            vec![kv("key", ExprKind::StringLit(files.key))],
        )];
        let err = SyslogTcpInput::build(
            "relay",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().to_lowercase().contains("cert"), "{}", err);
    }

    #[test]
    fn build_explicit_bind_overrides_default_port() {
        let files = pem_files();
        let props = vec![
            kv("bind", ExprKind::StringLit("127.0.0.1:9999".into())),
            block(
                "tls",
                vec![
                    kv("cert", ExprKind::StringLit(files.cert)),
                    kv("key", ExprKind::StringLit(files.key)),
                ],
            ),
        ];
        let i = SyslogTcpInput::build(
            "relay",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .expect("should build");
        assert_eq!(i.bind_addr, "127.0.0.1:9999");
    }

    // ---------------------------------------------------------------------
    // Accept-loop end-to-end tests: max_connections wire enforcement +
    // valid-event delivery. Coverage-gap audit Agent A flagged these
    // paths as High risk: the framing helpers are well-tested but the
    // listener that wraps them is not.
    // ---------------------------------------------------------------------

    fn pick_port() -> u16 {
        let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    }

    fn spawn_plain_input(
        bind_addr: String,
        max_connections: usize,
    ) -> (
        tokio::task::JoinHandle<anyhow::Result<()>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::mpsc::Receiver<Event>,
        Arc<InputMetrics>,
    ) {
        let metrics = Arc::new(InputMetrics::default());
        let input = SyslogTcpInput {
            bind_addr,
            framing: TcpFraming::OctetCounting,
            tls_config: None,
            rate_limit: None,
            max_connections,
            metrics: Arc::clone(&metrics),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { input.run(tx, sd_rx).await });
        (handle, sd_tx, rx, metrics)
    }

    #[tokio::test]
    async fn accepts_valid_octet_count_message_end_to_end() {
        // Sanity baseline for the accept-loop: bind, connect, send an
        // octet-counted frame `<len> <PRI>body`, observe the Event on
        // the mpsc. Framing helpers are tested above; this exercises
        // the listener that glues them together.
        use tokio::io::AsyncWriteExt;
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let (handle, sd_tx, mut rx, _metrics) = spawn_plain_input(bind.clone(), 8);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = tokio::net::TcpStream::connect(&bind).await.unwrap();
        // Frame: "9 <13>hello" — 9-byte body, then payload.
        client.write_all(b"9 <13>hello").await.unwrap();
        client.flush().await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        assert_eq!(&event.ingress[..], b"<13>hello");

        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn max_connections_cap_rejects_additional_clients() {
        // The accept loop refuses the (max_connections + 1)th
        // connection by dropping the stream immediately. A regression
        // that off-by-oned the `>= self.max_connections` check or
        // forgot to retain `conn_handles` would silently let the cap
        // drift. Open `cap` long-lived TCP connections that never
        // send data (so their per-conn tasks stay alive), then open
        // one more and assert it gets closed promptly.
        let port = pick_port();
        let bind = format!("127.0.0.1:{port}");
        let cap = 2usize;
        let (handle, sd_tx, _rx, _metrics) = spawn_plain_input(bind.clone(), cap);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Hold `cap` connections open (no data sent → per-conn task
        // sits in fill_buf / read_exact, holding the slot).
        let mut held: Vec<tokio::net::TcpStream> = Vec::new();
        for _ in 0..cap {
            held.push(tokio::net::TcpStream::connect(&bind).await.unwrap());
        }
        // Brief yield so accept_loop registers each one in conn_handles.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // (cap + 1)th client connects but should be dropped by the
        // accept loop. The client sees a TCP close — its read returns
        // EOF (0 bytes) quickly.
        let mut extra = tokio::net::TcpStream::connect(&bind).await.unwrap();
        let mut buf = [0u8; 1];
        let read_res = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::io::AsyncReadExt::read(&mut extra, &mut buf),
        )
        .await;
        match read_res {
            Ok(Ok(0)) => {} // EOF, expected — server closed
            Ok(Ok(n)) => panic!("rejected client read {n} bytes; expected EOF"),
            Ok(Err(e)) => panic!("rejected client read errored: {e}"),
            Err(_) => panic!(
                "rejected client read timed out — server kept connection alive past max_connections"
            ),
        }

        // The original `cap` connections remain open.
        let _ = held;
        let _ = sd_tx.send(true);
        let _ = handle.await;
    }
}

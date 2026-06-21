#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;

/// Per-peer cooldown after a send/connect failure. Hardcoded for now;
/// may later be exposed as a DSL property.
pub const PEER_COOLDOWN: Duration = Duration::from_secs(5);

/// Maximum time to wait for a TCP / UDP connect (per peer).
pub const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time to wait for a TLS handshake to complete (per peer).
pub const PEER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time to wait for a single write/flush on an established
/// connection (per peer).
pub const PEER_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

type PeerAttemptFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Payload carried from `render` to `write` for every syslog output.
/// All sinks ship one frame's bytes; nothing else is needed.
pub struct SyslogPayload {
    pub egress: Bytes,
}

/// Syslog over TCP framing per RFC 6587. Used by the `syslog_tcp`
/// output (which handles both plaintext and per-peer TLS through
/// its `Conn` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogFraming {
    /// RFC 6587 §3.4.1: `MSG-LEN SP SYSLOG-MSG`.
    OctetCounting,
    /// RFC 6587 §3.4.2: messages terminated by LF.
    NonTransparent,
}

/// A configured destination. `tls` is `Some(...)` only for outputs
/// that negotiate client-side TLS; plaintext outputs leave it `None`.
#[derive(Debug, Clone)]
pub struct Peer {
    pub host: String,
    pub port: u16,
    pub tls: Option<crate::tls::ClientTlsConfig>,
}

impl Peer {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Parse `host` and `port` properties from a peer block. The caller
/// supplies the default port and the label used in error messages.
pub fn parse_host_port(
    properties: &[Property],
    default_port: u16,
    label: &str,
) -> anyhow::Result<(String, u16)> {
    let host = props::get_string(properties, "host")
        .ok_or_else(|| anyhow::anyhow!("{} requires 'host'", label))?;
    let port = match props::get_int(properties, "port") {
        Some(port) => {
            u16::try_from(port).with_context(|| format!("{} port must be 0..=65535", label))?
        }
        None => default_port,
    };
    Ok((host, port))
}

/// Iterate `peer` blocks inside a `peers` block, invoking the provided
/// per-peer parser for each one.
pub fn iter_peers_block<T, F>(
    peers_block: &[Property],
    label: &str,
    mut parse_one: F,
) -> anyhow::Result<Vec<T>>
where
    F: FnMut(&[Property]) -> anyhow::Result<T>,
{
    let mut out = Vec::new();
    for prop in peers_block {
        if let Property::Block {
            key,
            properties: inner,
            ..
        } = prop
            && key == "peer"
        {
            out.push(parse_one(inner)?);
        }
    }
    if out.is_empty() {
        anyhow::bail!("{} block must contain at least one peer", label);
    }
    Ok(out)
}

/// Write one syslog message with RFC 6587 framing.
pub async fn write_framed<S>(
    stream: &mut S,
    framing: SyslogFraming,
    payload: &Bytes,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match framing {
        SyslogFraming::OctetCounting => {
            let header = format!("{} ", payload.len());
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(payload).await?;
        }
        SyslogFraming::NonTransparent => {
            stream.write_all(payload).await?;
            stream.write_all(b"\n").await?;
        }
    }

    stream.flush().await?;
    Ok(())
}

/// Per-peer mutable state. C is the connection type (TcpStream,
/// TlsStream, UdpSocket, etc.) supplied by the calling module.
pub struct PeerState<C> {
    pub conn: Option<C>,
    pub cooldown_until: Option<Instant>,
}

impl<C> Default for PeerState<C> {
    fn default() -> Self {
        Self {
            conn: None,
            cooldown_until: None,
        }
    }
}

/// Per-peer metrics. Aggregate sum into the parent `OutputMetrics`
/// happens at write time.
#[derive(Debug, Default)]
pub struct PeerMetrics {
    pub events_written: AtomicU64,
    pub connect_failures: AtomicU64,
    pub cooldowns_entered: AtomicU64,
}

/// N-peer rotational sink with passive health check (cooldown on
/// failure). Generic over the per-peer connection type C so syslog_tcp
/// (its own `Conn` enum wrapping plaintext or TLS streams) and
/// syslog_udp (`UdpSocket`) share this layer.
pub struct PeerList<C> {
    peers: Vec<Peer>,
    cursor: AtomicUsize,
    state: Vec<Mutex<PeerState<C>>>,
    metrics: Vec<Arc<PeerMetrics>>,
}

impl<C> PeerList<C> {
    pub fn new(peers: Vec<Peer>) -> Self {
        let state = peers
            .iter()
            .map(|_| Mutex::new(PeerState::default()))
            .collect();
        let metrics = peers
            .iter()
            .map(|_| Arc::new(PeerMetrics::default()))
            .collect();
        Self {
            peers,
            cursor: AtomicUsize::new(0),
            state,
            metrics,
        }
    }

    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn peer_metrics(&self) -> &[Arc<PeerMetrics>] {
        &self.metrics
    }

    pub async fn write_with_rotation_now<F>(&self, attempt: F) -> Result<(), PeerSendError>
    where
        F: for<'a> FnMut(usize, &'a Peer, &'a mut PeerState<C>) -> PeerAttemptFuture<'a>,
    {
        self.write_with_rotation_at(Instant::now(), attempt).await
    }

    /// Round-robin attempt loop. For each available peer (cooldown
    /// expired), runs `attempt` while holding that peer's `Mutex`
    /// lock. `attempt` is responsible for: (a) ensuring `state.conn`
    /// is populated (connect if needed), (b) writing the payload, and
    /// (c) returning `Ok(())` or `Err(_)`.
    ///
    /// On `Err(_)` for a peer, the peer is marked cooled-down for
    /// `PEER_COOLDOWN` and rotation continues to the next peer.
    ///
    /// Returns `Err(AllPeersUnavailable)` if every peer was either in
    /// cooldown or failed within this call.
    pub async fn write_with_rotation_at<F>(
        &self,
        now: Instant,
        mut attempt: F,
    ) -> Result<(), PeerSendError>
    where
        F: for<'a> FnMut(usize, &'a Peer, &'a mut PeerState<C>) -> PeerAttemptFuture<'a>,
    {
        let n = self.peers.len();
        if n == 0 {
            return Err(PeerSendError::Empty);
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for offset in 0..n {
            let idx = (start + offset) % n;
            let mut state = self.state[idx].lock().await;
            if state.cooldown_until.is_some_and(|until| until > now) {
                continue;
            }

            match attempt(idx, &self.peers[idx], &mut state).await {
                Ok(()) => {
                    state.cooldown_until = None;
                    self.metrics[idx]
                        .events_written
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(_) => {
                    state.cooldown_until = Some(now + PEER_COOLDOWN);
                    self.metrics[idx]
                        .connect_failures
                        .fetch_add(1, Ordering::Relaxed);
                    self.metrics[idx]
                        .cooldowns_entered
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        Err(PeerSendError::AllPeersUnavailable { n })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PeerSendError {
    #[error("no peers configured")]
    Empty,
    #[error("all {n} peers are in cooldown or failed this round")]
    AllPeersUnavailable { n: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::AtomicUsize;

    fn peer(host: &str, port: u16) -> Peer {
        Peer {
            host: host.to_string(),
            port,
            tls: None,
        }
    }

    fn peers(n: usize) -> Vec<Peer> {
        (0..n)
            .map(|idx| peer(&format!("peer-{idx}"), 514 + idx as u16))
            .collect()
    }

    #[test]
    fn peer_timeouts_are_documented_defaults() {
        assert_eq!(PEER_CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(PEER_HANDSHAKE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(PEER_WRITE_TIMEOUT, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn empty_peer_list_errors() {
        let list = PeerList::<()>::new(Vec::new());
        let err = list
            .write_with_rotation_now(|_, _, _| Box::pin(async { Ok(()) }))
            .await
            .expect_err("empty list should fail");
        assert!(matches!(err, PeerSendError::Empty));
    }

    #[tokio::test]
    async fn round_robin_distribution_over_three_peers() {
        let list = PeerList::<()>::new(peers(3));
        let calls = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);

        for _ in 0..9 {
            let calls = Arc::clone(&calls);
            list.write_with_rotation_now(move |idx, _, _| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls[idx].fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
            })
            .await
            .expect("write should succeed");
        }

        let counts: Vec<_> = calls
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect();
        assert_eq!(counts, vec![3, 3, 3]);
    }

    #[tokio::test]
    async fn single_failure_rotates_to_next() {
        let list = PeerList::<()>::new(peers(2));

        list.write_with_rotation_now(|idx, _, _| {
            Box::pin(async move {
                if idx == 0 {
                    Err(anyhow!("down"))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .expect("second peer should succeed");

        assert_eq!(
            list.peer_metrics()[0]
                .cooldowns_entered
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            list.peer_metrics()[1]
                .events_written
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn all_peers_failed_in_one_call() {
        let list = PeerList::<()>::new(peers(3));
        let err = list
            .write_with_rotation_now(|_, _, _| Box::pin(async { Err(anyhow!("down")) }))
            .await
            .expect_err("all peers failed");
        assert!(matches!(err, PeerSendError::AllPeersUnavailable { n: 3 }));
    }

    #[tokio::test]
    async fn cooldown_skips_peer_until_expiry() {
        let list = PeerList::<()>::new(peers(3));
        let now = Instant::now();

        list.write_with_rotation_at(now, |idx, _, _| {
            Box::pin(async move {
                if idx == 0 {
                    Err(anyhow!("down"))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .expect("peer 1 should succeed after peer 0 fails");

        list.cursor.store(0, Ordering::Relaxed);
        let called = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);
        let called_for_attempt = Arc::clone(&called);
        list.write_with_rotation_at(now + Duration::from_secs(1), move |idx, _, _| {
            let called = Arc::clone(&called_for_attempt);
            Box::pin(async move {
                called[idx].fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        })
        .await
        .expect("available peer should succeed");

        assert_eq!(called[0].load(Ordering::Relaxed), 0);
        assert_eq!(called[1].load(Ordering::Relaxed), 1);

        {
            let mut state = list.state[0].lock().await;
            state.cooldown_until = Some(now - Duration::from_secs(1));
        }
        list.cursor.store(0, Ordering::Relaxed);
        list.write_with_rotation_at(now, |idx, _, _| {
            Box::pin(async move {
                assert_eq!(idx, 0);
                Ok(())
            })
        })
        .await
        .expect("expired peer should be attempted");
    }

    #[tokio::test]
    async fn cooldown_reset_on_success() {
        let list = PeerList::<()>::new(peers(1));
        let now = Instant::now();
        {
            let mut state = list.state[0].lock().await;
            state.cooldown_until = Some(now - Duration::from_secs(1));
        }

        list.write_with_rotation_at(now, |_, _, _| Box::pin(async { Ok(()) }))
            .await
            .expect("expired peer should succeed");

        let state = list.state[0].lock().await;
        assert!(state.cooldown_until.is_none());
    }

    #[tokio::test]
    async fn concurrent_writers_serialize_per_peer() {
        let list = Arc::new(PeerList::<()>::new(peers(1)));
        let in_flight = Arc::new(AtomicUsize::new(0));

        let write_one = async {
            let list = Arc::clone(&list);
            let in_flight = Arc::clone(&in_flight);
            list.write_with_rotation_now(move |_, _, _| {
                let in_flight = Arc::clone(&in_flight);
                Box::pin(async move {
                    let previous = in_flight.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(previous, 0, "same-peer attempts overlapped");
                    tokio::task::yield_now().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .await
        };
        let write_two = async {
            let list = Arc::clone(&list);
            let in_flight = Arc::clone(&in_flight);
            list.write_with_rotation_now(move |_, _, _| {
                let in_flight = Arc::clone(&in_flight);
                Box::pin(async move {
                    let previous = in_flight.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(previous, 0, "same-peer attempts overlapped");
                    tokio::task::yield_now().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .await
        };

        let (one, two) = tokio::join!(write_one, write_two);
        one.expect("ok");
        two.expect("ok");
        assert_eq!(
            list.peer_metrics()[0]
                .events_written
                .load(Ordering::Relaxed),
            2
        );
    }
}

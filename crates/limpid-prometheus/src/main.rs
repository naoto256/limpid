//! limpid-prometheus: Prometheus exporter for limpid.
//!
//! Queries limpid's control socket (`stats json`) and converts the response
//! to Prometheus text exposition format.
//!
//! Usage:
//!   limpid-prometheus                                 # defaults
//!   limpid-prometheus --bind 0.0.0.0:9100             # custom bind
//!   limpid-prometheus --socket /path/to/control.sock  # custom socket
//!
//! The HTTP endpoint has **no authentication or TLS**. The default
//! bind is loopback-only; binding to a non-loopback address (as in
//! the `0.0.0.0:9100` example above) assumes a trusted network — a
//! scrape segment behind a firewall — because anyone who can reach
//! the port can read pipeline names and event counts.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Semaphore;

/// Hard upper bound on a single control-socket round trip. limpid's
/// control socket is local IPC and a `stats` reply is typically a few
/// kilobytes returned in well under a millisecond, so anything past
/// this window means the daemon is wedged or shutting down. Capping
/// the call keeps Prometheus scrapes from piling up on a stuck peer:
/// a scrape that hits the timeout returns an error body, and the next
/// scrape gets a fresh attempt instead of waiting behind the old one.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard upper bound on how long a single accepted HTTP connection is
/// allowed to occupy a permit. This is the slowloris backstop — a peer
/// that opens a TCP connection but never sends a complete request will
/// otherwise sit in `serve_connection().await` forever, keeping its
/// permit. Once every permit is held by such an idle peer, all future
/// scrapes are refused at accept time and the exporter is effectively
/// wedged. Capping the whole connection recycles the permit even if
/// the peer never sends anything or leaves the socket half-open.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum concurrent HTTP connections served at once. Mirrors the
/// daemon's own control-socket cap (`MAX_CONTROL_CONNECTIONS` in
/// limpid's control.rs): a scrape endpoint needs one connection per
/// Prometheus server plus a little headroom, so 8 is ample, and the
/// cap keeps a misbehaving peer (or a slowloris-style dribble of idle
/// connections) from accumulating unbounded tasks. Excess connections
/// are closed immediately; the next scrape gets a fresh slot as soon
/// as an in-flight one finishes — paired with `CONNECTION_TIMEOUT`,
/// so an idle-forever peer can't perma-hold its slot.
const MAX_CONCURRENT_CONNECTIONS: usize = 8;

#[derive(Parser)]
#[command(name = "limpid-prometheus", about = "Prometheus exporter for limpid")]
struct Cli {
    /// HTTP bind address
    #[arg(long, default_value = "127.0.0.1:9100")]
    bind: SocketAddr,

    /// limpid control socket path
    #[arg(long, default_value = "/var/run/limpid/control.sock")]
    socket: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let listener = match TcpListener::bind(cli.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {}: {}", cli.bind, e);
            std::process::exit(1);
        }
    };

    eprintln!("limpid-prometheus listening on http://{}", cli.bind);
    eprintln!("  control socket: {:?}", cli.socket);

    serve(listener, cli.socket, CONNECTION_TIMEOUT).await;
}

/// Accept loop, split from `main` so the connection-cap behaviour is
/// testable against an ephemeral listener. `connection_timeout` is a
/// parameter so tests can shrink it without waiting on the real
/// 15-second bound; production callers pass `CONNECTION_TIMEOUT`.
async fn serve(listener: TcpListener, socket: PathBuf, connection_timeout: Duration) {
    let conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {}", e);
                continue;
            }
        };

        // At the cap: close the connection immediately (drop = RST/FIN)
        // rather than queueing it. A stalled scrape has QUERY_TIMEOUT
        // bounding the control-socket round trip *and* CONNECTION_TIMEOUT
        // bounding the whole HTTP conversation, so slots recycle within
        // a bounded window; the honest fix for a client hitting the cap
        // is to retry the next scrape interval.
        let permit = match Arc::clone(&conn_sem).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                drop(stream);
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let socket = socket.clone();

        tokio::spawn(async move {
            // Hold the permit for the connection's lifetime; dropping
            // it when the task ends frees the slot.
            let _permit = permit;
            let svc = service_fn(move |req| {
                let socket = socket.clone();
                async move { handle_request(req, &socket).await }
            });
            // Wrap the whole `serve_connection` future in a timeout,
            // not just the request handler. QUERY_TIMEOUT only bounds
            // the control-socket round trip inside `handle_request`,
            // so a peer that opens a TCP connection and then never
            // sends a request header (classic slowloris) would sit
            // in `serve_connection().await` indefinitely and never
            // release its permit. Once every permit is held that way,
            // scrapes accepted at the cap are dropped and the
            // exporter is wedged from the outside. `CONNECTION_TIMEOUT`
            // caps the whole conversation so an idle-forever peer
            // still frees its slot within a bounded window.
            let conn = http1::Builder::new().serve_connection(io, svc);
            match tokio::time::timeout(connection_timeout, conn).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.is_incomplete_message() => {}
                Ok(Err(e)) => eprintln!("connection error: {}", e),
                Err(_) => {
                    // Timeout fired. Dropping `conn` shuts the socket
                    // down and releases the permit at the end of this
                    // task, which is the recovery signal for the cap.
                }
            }
        });
    }
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    socket_path: &Path,
) -> Result<Response<Full<Bytes>>, Infallible> {
    match req.uri().path() {
        "/health" => match query_control(socket_path, "health").await {
            Ok(body) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from(body)))
                .unwrap()),
            Err(e) => Ok(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from(e)))
                .unwrap()),
        },
        "/metrics" => {
            let body = match query_control(socket_path, "stats").await {
                Ok(json) => match json_to_prometheus(&json) {
                    Ok(text) => text,
                    Err(e) => format!("# error: {}\n", e),
                },
                Err(e) => format!("# error: {}\n", e),
            };
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                .body(Full::new(Bytes::from(body)))
                .unwrap())
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found\n")))
            .unwrap()),
    }
}

/// Convert limpid JSON stats to Prometheus text exposition format.
fn json_to_prometheus(json: &str) -> Result<String, String> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid json: {}", e))?;

    let mut out = String::new();

    if let Some(inputs) = root.get("inputs").and_then(|v| v.as_object()) {
        write_counter(
            &mut out,
            "limpid_input_events_received_total",
            "Total events received by input.",
            "input",
            inputs,
            "events_received",
        );
        write_counter(
            &mut out,
            "limpid_input_events_invalid_total",
            "Total invalid events rejected by input.",
            "input",
            inputs,
            "events_invalid",
        );
        write_counter(
            &mut out,
            "limpid_input_events_injected_total",
            "Total events injected into this input's channel via limpidctl inject.",
            "input",
            inputs,
            "events_injected",
        );
    }

    if let Some(pipelines) = root.get("pipelines").and_then(|v| v.as_object()) {
        write_counter(
            &mut out,
            "limpid_pipeline_events_received_total",
            "Total events received by pipeline.",
            "pipeline",
            pipelines,
            "events_received",
        );
        write_counter(
            &mut out,
            "limpid_pipeline_events_finished_total",
            "Total events that finished pipeline processing.",
            "pipeline",
            pipelines,
            "events_finished",
        );
        write_counter(
            &mut out,
            "limpid_pipeline_events_dropped_total",
            "Total events explicitly dropped by pipeline.",
            "pipeline",
            pipelines,
            "events_dropped",
        );
        write_counter(
            &mut out,
            "limpid_pipeline_events_discarded_total",
            "Total events that finished the pipeline without reaching any output (likely a routing misconfiguration).",
            "pipeline",
            pipelines,
            "events_discarded",
        );
        write_counter(
            &mut out,
            "limpid_pipeline_events_errored_total",
            "Total events whose processing raised a runtime error; routed to the dead-letter queue (configured error_log file, or a structured tracing line).",
            "pipeline",
            pipelines,
            "events_errored",
        );
        write_counter(
            &mut out,
            "limpid_pipeline_events_errored_unwritable_total",
            "Subset of events_errored where the configured error_log write itself failed; alarm on this — the replay path may be incomplete.",
            "pipeline",
            pipelines,
            "events_errored_unwritable",
        );
    }

    if let Some(outputs) = root.get("outputs").and_then(|v| v.as_object()) {
        write_counter(
            &mut out,
            "limpid_output_events_received_total",
            "Total events that entered this output's queue (from pipelines + injects).",
            "output",
            outputs,
            "events_received",
        );
        write_counter(
            &mut out,
            "limpid_output_events_injected_total",
            "Total events injected into this output's queue via limpidctl inject.",
            "output",
            outputs,
            "events_injected",
        );
        write_counter(
            &mut out,
            "limpid_output_events_written_total",
            "Total events successfully written by output.",
            "output",
            outputs,
            "events_written",
        );
        write_counter(
            &mut out,
            "limpid_output_events_failed_total",
            "Total events that failed to write after all retries.",
            "output",
            outputs,
            "events_failed",
        );
        write_counter(
            &mut out,
            "limpid_output_retries_total",
            "Total retry attempts by output.",
            "output",
            outputs,
            "retries",
        );
        write_counter(
            &mut out,
            "limpid_output_events_wedged_total",
            "Total disk-queue fail-stop wedges observed by this output; alarm on this — the consumer has stopped accepting new events and will replay from the wedged cursor on next daemon start.",
            "output",
            outputs,
            "events_wedged",
        );
        write_counter(
            &mut out,
            "limpid_output_events_errored_unwritable_total",
            "Sink-side counterpart of limpid_pipeline_events_errored_unwritable_total: DLQ (error_log) write failures observed while routing an output-side failure through the DLQ. Alarm on this — the replay path may be incomplete.",
            "output",
            outputs,
            "events_errored_unwritable",
        );
    }

    Ok(out)
}

fn write_counter(
    out: &mut String,
    metric: &str,
    help: &str,
    label_key: &str,
    instances: &serde_json::Map<String, serde_json::Value>,
    json_field: &str,
) {
    let mut samples: Vec<(&str, u64)> = Vec::new();
    for (name, obj) in instances {
        if let Some(val) = obj.get(json_field).and_then(|v| v.as_u64()) {
            samples.push((name.as_str(), val));
        }
    }
    if samples.is_empty() {
        return;
    }
    samples.sort_by_key(|(name, _)| *name);

    writeln!(out, "# HELP {metric} {help}").unwrap();
    writeln!(out, "# TYPE {metric} counter").unwrap();
    for (name, val) in &samples {
        let escaped = escape_label_value(name);
        writeln!(out, "{metric}{{{label_key}=\"{escaped}\"}} {val}").unwrap();
    }
    writeln!(out).unwrap();
}

/// Escape a Prometheus label value: \, ", and newline must be escaped.
fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

async fn query_control(socket_path: &Path, command: &str) -> Result<String, String> {
    // Wrap the whole connect+write+read sequence in a single deadline.
    // Old code used the synchronous `std::os::unix::net::UnixStream` +
    // blocking `BufRead::lines()` from inside an async hyper handler,
    // which parked a tokio worker thread until the daemon answered (or
    // forever, with no timeout) — slow / stuck scrapes silently
    // starved the runtime. Switching to `tokio::net::UnixStream` +
    // `AsyncBufReadExt` keeps the I/O cooperative; `tokio::time::timeout`
    // gives it a documented upper bound (see QUERY_TIMEOUT).
    let cmd = command.to_string();
    let path = socket_path.to_path_buf();
    tokio::time::timeout(QUERY_TIMEOUT, async move {
        let mut stream = UnixStream::connect(&path)
            .await
            .map_err(|e| format!("cannot connect to limpid: {}", e))?;

        stream
            .write_all(format!("{}\n", cmd).as_bytes())
            .await
            .map_err(|e| format!("cannot send command: {}", e))?;
        // Half-close the write side so the daemon sees EOF and starts
        // responding (mirrors the sync path's `shutdown(Write)`).
        let _ = stream.shutdown().await;

        let mut reader = BufReader::new(stream);
        let mut result = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => result.push_str(&line),
                Err(e) => {
                    // Surface the read error rather than treating
                    // mid-response failure as clean EOF. The previous
                    // `Err(_) => break` flow returned `Ok(result)`
                    // with whatever bytes had been collected so far,
                    // so `/health` would 200 with a truncated body —
                    // masking a broken control socket as healthy.
                    return Err(format!("control socket read failed: {}", e));
                }
            }
        }
        Ok::<_, String>(result)
    })
    .await
    .map_err(|_| {
        format!(
            "control socket timed out after {}s",
            QUERY_TIMEOUT.as_secs()
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpStream, UnixListener};

    /// Alarm counters observable in the stats JSON must be emitted as
    /// Prometheus samples so operators can build alerts on them. The
    /// output-side `events_wedged` (disk-queue fail-stop wedge) and
    /// `events_errored_unwritable` (sink-side DLQ-write failure) are
    /// documented as alarm signals in
    /// `docs/src/operations/error-log.md` and must appear in the
    /// scrape output alongside the pipeline-side alarm counter.
    #[test]
    fn output_alarm_counters_are_exported() {
        let json = r#"{
            "outputs": {
                "primary": {
                    "events_received": 100,
                    "events_injected": 0,
                    "events_written": 90,
                    "events_failed": 10,
                    "retries": 3,
                    "events_wedged": 1,
                    "events_errored_unwritable": 2
                }
            }
        }"#;
        let out = json_to_prometheus(json).unwrap();
        assert!(
            out.contains("limpid_output_events_wedged_total{output=\"primary\"} 1"),
            "expected events_wedged sample:\n{out}"
        );
        assert!(
            out.contains(
                "limpid_output_events_errored_unwritable_total{output=\"primary\"} 2"
            ),
            "expected sink-side events_errored_unwritable sample:\n{out}"
        );
    }

    #[tokio::test]
    async fn accept_loop_closes_connections_over_the_cap() {
        // Cap regression: MAX_CONCURRENT_CONNECTIONS idle connections
        // (opened but never sending a request) each hold a permit via
        // their serve_connection task; connection cap+1 must be
        // accepted and immediately closed, not queued behind them.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Control socket path deliberately nonexistent — /health would
        // 503, but the idle connections never send a request at all.
        let server = tokio::spawn(serve(
            listener,
            PathBuf::from("/nonexistent/control.sock"),
            CONNECTION_TIMEOUT,
        ));

        // Fill every slot with idle connections. Connect sequentially:
        // the accept loop acquires the permit synchronously right
        // after accept, so by the time connection N+1 is accepted,
        // connection N already holds its slot.
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_CONNECTIONS {
            held.push(TcpStream::connect(addr).await.unwrap());
        }

        // One over the cap: accepted, then dropped by the accept loop.
        // The client observes a prompt close (clean EOF or a reset).
        let mut over = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), over.read(&mut buf))
            .await
            .expect("over-cap connection must be closed promptly, not left hanging");
        match read {
            Ok(0) => {}  // clean FIN
            Err(_) => {} // RST — also an immediate close
            Ok(n) => panic!("expected close, got {} unexpected bytes", n),
        }

        server.abort();
        drop(held);
    }

    #[tokio::test]
    async fn idle_connections_release_their_permits_after_connection_timeout() {
        // Regression: without a whole-connection timeout, a slowloris
        // peer that opens a TCP connection and never sends a request
        // header would sit inside `serve_connection().await` forever
        // and hold its permit. Fill every slot with such peers, wait
        // past the (short) connection timeout, then require that a new
        // client can grab a slot and receive a valid HTTP response.
        // Uses a real (short) connection_timeout so real network I/O
        // and the timeout live in the same clock.
        use tokio::io::AsyncWriteExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        // 250 ms is long enough that a normal HTTP round trip isn't
        // race-timed out, short enough that the test finishes in ~1 s.
        let short_timeout = Duration::from_millis(250);
        let server = tokio::spawn(serve(listener, sock, short_timeout));

        // Fill every slot with idle connections. Each parked TCP
        // connection keeps its `serve_connection` future suspended
        // pre-fix; the timeout must free the slot on the fix side.
        let mut idle = Vec::new();
        for _ in 0..MAX_CONCURRENT_CONNECTIONS {
            idle.push(TcpStream::connect(addr).await.unwrap());
        }

        // Sanity check the pre-recycle state: one more connection is
        // accepted at the TCP layer, then immediately closed because
        // the cap holds. Give the loop a moment to run the drop.
        {
            let mut over = TcpStream::connect(addr).await.unwrap();
            let mut buf = [0u8; 1];
            let read = tokio::time::timeout(Duration::from_millis(500), over.read(&mut buf)).await;
            assert!(
                matches!(read, Ok(Ok(0)) | Ok(Err(_))),
                "over-cap connection must be closed while permits are held, got {read:?}"
            );
        }

        // Wait past the connection timeout so every `serve_connection`
        // future times out and its permit drops. A small margin over
        // the timeout absorbs scheduler jitter.
        tokio::time::sleep(short_timeout + Duration::from_millis(150)).await;

        // Recovery client: after the timeout window, a fresh
        // connection must reach `handle_request` and see a real HTTP
        // reply (503 for /health with no daemon on the other end is
        // fine — the point is we weren't refused at the cap).
        let mut recovery = TcpStream::connect(addr).await.unwrap();
        recovery
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write recovery request");
        let mut buf = [0u8; 12];
        let n = tokio::time::timeout(Duration::from_secs(2), recovery.read(&mut buf))
            .await
            .expect("recovery response must not block")
            .expect("recovery response read must succeed");
        assert!(n > 0, "recovery client must receive HTTP status bytes");
        assert!(
            buf[..n].starts_with(b"HTTP/1."),
            "recovery response must be HTTP, got: {:?}",
            &buf[..n]
        );

        server.abort();
        drop(idle);
    }

    #[tokio::test(start_paused = true)]
    async fn query_control_times_out_when_peer_stalls() {
        // Regression: with the previous synchronous code path, a
        // wedged limpid daemon (= accepted the connection but never
        // wrote a reply) parked a tokio worker thread forever. The
        // async path wraps the round trip in `tokio::time::timeout`,
        // so a stalled peer surfaces a bounded error instead.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("control.sock");

        // Stalled server: accept the connection, then sit on it
        // without ever writing. Without QUERY_TIMEOUT, the read
        // loop would suspend indefinitely.
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Hold the stream open. With paused time + no I/O ever
            // happening on this side, the future just yields.
            let _hold = stream;
            std::future::pending::<()>().await;
        });

        // Drive the timeout deterministically with virtual time.
        let call = query_control(&sock, "stats");
        tokio::pin!(call);

        // Confirm the call is not ready before the timeout window
        // has elapsed. One advance just past QUERY_TIMEOUT must
        // wake the timeout future and surface the error.
        tokio::time::advance(QUERY_TIMEOUT + Duration::from_secs(1)).await;
        let result = call.await;
        server.abort();

        let err = result.expect_err("stalled peer must surface as timeout Err");
        assert!(
            err.contains("timed out"),
            "expected timeout-flavoured error, got: {err}"
        );
    }
}

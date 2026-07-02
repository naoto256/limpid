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

/// Maximum concurrent HTTP connections served at once. Mirrors the
/// daemon's own control-socket cap (`MAX_CONTROL_CONNECTIONS` in
/// limpid's control.rs): a scrape endpoint needs one connection per
/// Prometheus server plus a little headroom, so 8 is ample, and the
/// cap keeps a misbehaving peer (or a slowloris-style dribble of idle
/// connections) from accumulating unbounded tasks. Excess connections
/// are closed immediately; the next scrape gets a fresh slot as soon
/// as an in-flight one finishes.
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

    serve(listener, cli.socket).await;
}

/// Accept loop, split from `main` so the connection-cap behaviour is
/// testable against an ephemeral listener.
async fn serve(listener: TcpListener, socket: PathBuf) {
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
        // rather than queueing it. A stalled scrape already has
        // QUERY_TIMEOUT bounding it, so slots recycle quickly; anything
        // that keeps all 8 busy is a misbehaving peer, and the honest
        // fix for the client is to retry the next scrape interval.
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
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await
                && !e.is_incomplete_message()
            {
                eprintln!("connection error: {}", e);
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
        let server = tokio::spawn(serve(listener, PathBuf::from("/nonexistent/control.sock")));

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

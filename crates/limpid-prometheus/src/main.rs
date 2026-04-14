//! limpid-prometheus: Prometheus exporter for limpid.
//!
//! Queries limpid's control socket (`stats json`) and converts the response
//! to Prometheus text exposition format.
//!
//! Usage:
//!   limpid-prometheus                                 # defaults
//!   limpid-prometheus --bind 0.0.0.0:9100             # custom bind
//!   limpid-prometheus --socket /path/to/control.sock  # custom socket

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use clap::Parser;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

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

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {}", e);
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let socket = cli.socket.clone();

        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let socket = socket.clone();
                async move { handle_request(req, &socket) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await
                && !e.is_incomplete_message()
            {
                eprintln!("connection error: {}", e);
            }
        });
    }
}

fn handle_request(
    req: Request<hyper::body::Incoming>,
    socket_path: &PathBuf,
) -> Result<Response<Full<Bytes>>, Infallible> {
    match req.uri().path() {
        "/health" => {
            let body = match query_control(socket_path, "health") {
                Ok(s) => s,
                Err(e) => e,
            };
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from(body)))
                .unwrap())
        }
        "/metrics" => {
            let body = match query_control(socket_path, "stats") {
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
            &mut out, "limpid_input_events_received_total",
            "Total events received by input.", "input", inputs, "events_received",
        );
        write_counter(
            &mut out, "limpid_input_events_invalid_total",
            "Total invalid events rejected by input.", "input", inputs, "events_invalid",
        );
    }

    if let Some(pipelines) = root.get("pipelines").and_then(|v| v.as_object()) {
        write_counter(
            &mut out, "limpid_pipeline_events_received_total",
            "Total events received by pipeline.", "pipeline", pipelines, "events_received",
        );
        write_counter(
            &mut out, "limpid_pipeline_events_finished_total",
            "Total events that finished pipeline processing.", "pipeline", pipelines, "events_finished",
        );
        write_counter(
            &mut out, "limpid_pipeline_events_dropped_total",
            "Total events explicitly dropped by pipeline.", "pipeline", pipelines, "events_dropped",
        );
        write_counter(
            &mut out, "limpid_pipeline_events_discarded_total",
            "Total events discarded due to processing errors.", "pipeline", pipelines, "events_discarded",
        );
    }

    if let Some(outputs) = root.get("outputs").and_then(|v| v.as_object()) {
        write_counter(
            &mut out, "limpid_output_events_written_total",
            "Total events successfully written by output.", "output", outputs, "events_written",
        );
        write_counter(
            &mut out, "limpid_output_events_failed_total",
            "Total events that failed to write after all retries.", "output", outputs, "events_failed",
        );
        write_counter(
            &mut out, "limpid_output_retries_total",
            "Total retry attempts by output.", "output", outputs, "retries",
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

fn query_control(socket_path: &PathBuf, command: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("cannot connect to limpid: {}", e))?;

    writeln!(stream, "{}", command)
        .map_err(|e| format!("cannot send command: {}", e))?;

    let _ = stream.shutdown(std::net::Shutdown::Write);

    let reader = std::io::BufReader::new(stream);
    let mut result = String::new();
    for line in reader.lines() {
        match line {
            Ok(text) => {
                result.push_str(&text);
                result.push('\n');
            }
            Err(_) => break,
        }
    }
    Ok(result)
}

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

use std::collections::{BTreeMap, BTreeSet};
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
use limpid_metrics_schema::{MetricFamily, MetricType, MetricsSnapshot};
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
    let root: MetricsSnapshot =
        serde_json::from_str(json).map_err(|e| format!("invalid json: {}", e))?;
    let families = parse_snapshot(&root)?;
    Ok(render_snapshot(&families))
}

type Labels = Vec<(String, String)>;

#[derive(Clone, Copy)]
enum ExpositionType {
    Counter,
    Gauge,
    Histogram,
}

impl ExpositionType {
    fn from_wire(value: MetricType) -> Self {
        match value {
            MetricType::Counter => Self::Counter,
            MetricType::Gauge => Self::Gauge,
            MetricType::Histogram => Self::Histogram,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

enum ExpositionSeries {
    Value {
        labels: Labels,
        value: u64,
    },
    Histogram {
        labels: Labels,
        buckets: Vec<(f64, u64)>,
        sum: f64,
        count: u64,
    },
}

impl ExpositionSeries {
    fn labels(&self) -> &Labels {
        match self {
            Self::Value { labels, .. } | Self::Histogram { labels, .. } => labels,
        }
    }
}

struct ExpositionFamily {
    name: String,
    metric_type: ExpositionType,
    help: String,
    series: Vec<ExpositionSeries>,
}

/// Validates and canonicalises a schema v1 snapshot into
/// families ready for exposition. A single validation failure
/// returns `Err`, suppressing the whole render rather than
/// emitting a partial body that omits or misrepresents metric
/// samples. Families and series are sorted for byte-stable
/// output; Prometheus text format has no display-order
/// semantics, so this is a reproducibility contract, not
/// Grafana display ordering.
fn parse_snapshot(root: &MetricsSnapshot) -> Result<Vec<ExpositionFamily>, String> {
    if root.schema != 1 {
        return Err("unsupported or missing metrics schema".to_string());
    }

    let mut names = BTreeSet::new();
    let mut families = Vec::with_capacity(root.metrics.len());
    for metric in &root.metrics {
        let name = metric.name().to_string();
        if name.is_empty() {
            return Err("metric family field name must not be empty".to_string());
        }
        if !is_legacy_metric_name(&name) {
            return Err(format!("invalid Prometheus metric name: {name:?}"));
        }
        if !names.insert(name.clone()) {
            return Err(format!("duplicate metric family: {name}"));
        }
        let metric_type = ExpositionType::from_wire(metric.metric_type());
        let help = metric.help().to_string();
        if help.is_empty() {
            return Err(format!("{name} field help must not be empty"));
        }

        let mut labelsets = BTreeSet::new();
        let mut family_label_keys = None;
        let mut series = Vec::new();
        match metric {
            MetricFamily::Counter { series: raw, .. } | MetricFamily::Gauge { series: raw, .. } => {
                series.reserve(raw.len());
                for raw in raw {
                    let labels = parse_labels(&raw.labels, &name)?;
                    series.push(ExpositionSeries::Value {
                        labels,
                        value: raw.value,
                    });
                }
            }
            MetricFamily::Histogram { series: raw, .. } => {
                series.reserve(raw.len());
                for raw in raw {
                    let labels = parse_labels(&raw.labels, &name)?;
                    let buckets = parse_buckets(&raw.buckets, &name)?;
                    if !raw.sum.is_finite() {
                        return Err(format!("metric {name} sum must be finite"));
                    }
                    series.push(ExpositionSeries::Histogram {
                        labels,
                        buckets,
                        sum: raw.sum,
                        count: raw.count,
                    });
                }
            }
        }
        for parsed in &series {
            if !labelsets.insert(parsed.labels().clone()) {
                return Err(format!("metric {name} contains a duplicate labelset"));
            }
            let label_keys = parsed
                .labels()
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            match &family_label_keys {
                Some(expected) if expected != &label_keys => {
                    return Err(format!(
                        "metric {name} series must use the same label-name set"
                    ));
                }
                None => family_label_keys = Some(label_keys),
                _ => {}
            }
        }
        validate_exported_labelsets(&series, metric_type, &name)?;
        series.sort_by(|left, right| left.labels().cmp(right.labels()));
        families.push(ExpositionFamily {
            name,
            metric_type,
            help,
            series,
        });
    }
    validate_histogram_namespaces(&families, &names)?;
    families.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(families)
}

fn validate_exported_labelsets(
    series: &[ExpositionSeries],
    metric_type: ExpositionType,
    metric_name: &str,
) -> Result<(), String> {
    let mut exported_labelsets = BTreeSet::new();
    for series in series {
        let exported_labels = match metric_type {
            ExpositionType::Histogram => map_histogram_labels(series.labels()),
            ExpositionType::Counter | ExpositionType::Gauge => series.labels().clone(),
        };
        if !exported_labelsets.insert(exported_labels) {
            return Err(format!(
                "metric {metric_name} contains duplicate exported labelsets"
            ));
        }
    }
    Ok(())
}

fn validate_histogram_namespaces(
    families: &[ExpositionFamily],
    declared_names: &BTreeSet<String>,
) -> Result<(), String> {
    for family in families {
        if matches!(family.metric_type, ExpositionType::Histogram) {
            for suffix in ["_bucket", "_sum", "_count"] {
                let derived = format!("{}{suffix}", family.name);
                if declared_names.contains(&derived) {
                    return Err(format!(
                        "histogram {} derived name collides with declared family {derived}",
                        family.name
                    ));
                }
            }
        }
    }
    Ok(())
}

fn parse_labels(labels: &BTreeMap<String, String>, metric_name: &str) -> Result<Labels, String> {
    let mut parsed = Vec::with_capacity(labels.len());
    for (key, value) in labels {
        if !is_legacy_label_name(key) {
            return Err(format!(
                "metric {metric_name} has invalid Prometheus label name: {key:?}"
            ));
        }
        parsed.push((key.clone(), value.clone()));
    }
    parsed.sort();
    Ok(parsed)
}

/// Prometheus text 0.0.4 metric-name grammar; the sidecar
/// emits legacy names unquoted, so anything outside this
/// grammar would break exposition. The daemon's registry is
/// intentionally unconstrained — the check lives here so an
/// invalid name returns Err for the whole snapshot before
/// rendering begins.
fn is_legacy_metric_name(name: &str) -> bool {
    matches!(name.as_bytes(), [first, rest @ ..]
        if (first.is_ascii_alphabetic() || matches!(first, b'_' | b':'))
            && rest
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':')))
}

/// Prometheus label-name grammar. Same wire-contract rationale
/// as `is_legacy_metric_name`, plus the `__` prefix is
/// reserved by Prometheus for internal labels and rejected
/// here rather than passed through the sidecar.
fn is_legacy_label_name(name: &str) -> bool {
    !name.starts_with("__")
        && matches!(name.as_bytes(), [first, rest @ ..]
            if (first.is_ascii_alphabetic() || *first == b'_')
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'))
}

fn parse_buckets(raw: &[(f64, u64)], metric_name: &str) -> Result<Vec<(f64, u64)>, String> {
    let mut buckets = Vec::with_capacity(raw.len());
    for &(bound, count) in raw {
        if !bound.is_finite() {
            return Err(format!("metric {metric_name} bucket bound must be finite"));
        }
        if let Some((previous_bound, previous_count)) = buckets.last() {
            if previous_bound >= &bound {
                return Err(format!(
                    "metric {metric_name} bucket bounds must be strictly increasing"
                ));
            }
            if previous_count > &count {
                return Err(format!(
                    "metric {metric_name} cumulative bucket counts must not decrease"
                ));
            }
        }
        buckets.push((bound, count));
    }
    Ok(buckets)
}

fn render_snapshot(families: &[ExpositionFamily]) -> String {
    let mut out = String::new();
    for family in families {
        writeln!(out, "# HELP {} {}", family.name, escape_help(&family.help)).unwrap();
        writeln!(
            out,
            "# TYPE {} {}",
            family.name,
            family.metric_type.as_str()
        )
        .unwrap();
        for series in &family.series {
            match series {
                ExpositionSeries::Value { labels, value } => {
                    write_sample(&mut out, &family.name, labels, *value);
                }
                ExpositionSeries::Histogram {
                    labels,
                    buckets,
                    sum,
                    count,
                } => {
                    let labels = map_histogram_labels(labels);
                    for (bound, bucket_count) in buckets {
                        let bucket_labels = with_bucket_bound(&labels, &bound.to_string());
                        write_sample(
                            &mut out,
                            &format!("{}_bucket", family.name),
                            &bucket_labels,
                            *bucket_count,
                        );
                    }
                    let infinite_labels = with_bucket_bound(&labels, "+Inf");
                    // The +Inf bucket count is the snapshot's total
                    // `count`, not clamped against the last finite
                    // bucket. Per-bucket and total counters are
                    // separately loaded `Relaxed` atomics; when
                    // snapshot loads interleave with `observe`, a
                    // transient last>total is a legitimate
                    // cross-atomic snapshot skew — clamping or
                    // rejecting would hide it and misrepresent
                    // observed values.
                    write_sample(
                        &mut out,
                        &format!("{}_bucket", family.name),
                        &infinite_labels,
                        *count,
                    );
                    write_sample(&mut out, &format!("{}_sum", family.name), &labels, *sum);
                    write_sample(&mut out, &format!("{}_count", family.name), &labels, *count);
                }
            }
        }
        writeln!(out).unwrap();
    }
    out
}

/// Sidecar-only fix for the `le` label reserved by Prometheus
/// histogram exposition. When a source series contains a
/// label named `le`, the entire `le`, `le_`, `le__`, …
/// underscore chain is shifted by one so the appended bucket
/// bound gets an unambiguous `le=` slot; the shift is
/// injective and applied consistently to bucket, sum, and
/// count samples. The daemon-side registry stays unconstrained
/// by this rule — only exposition rewrites.
fn map_histogram_labels(labels: &Labels) -> Labels {
    if !labels.iter().any(|(key, _)| key == "le") {
        return labels.clone();
    }
    let mut mapped = labels
        .iter()
        .map(|(key, value)| {
            let key = if key == "le"
                || key.strip_prefix("le").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte == b'_')
                }) {
                format!("{key}_")
            } else {
                key.clone()
            };
            (key, value.clone())
        })
        .collect::<Labels>();
    mapped.sort();
    mapped
}

fn with_bucket_bound(labels: &Labels, bound: &str) -> Labels {
    let mut labels = labels.clone();
    labels.push(("le".to_string(), bound.to_string()));
    labels.sort();
    labels
}

fn write_sample<T: std::fmt::Display>(out: &mut String, name: &str, labels: &Labels, value: T) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (key, value)) in labels.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            write!(out, "{key}=\"{}\"", escape_label_value(value)).unwrap();
        }
        out.push('}');
    }
    writeln!(out, " {value}").unwrap();
}

fn escape_help(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
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

    #[test]
    fn schema_v1_fixture_parses_through_the_shared_wire_dto() {
        let fixture = include_str!("../../limpid-metrics-schema/tests/fixtures/schema-v1.json");
        let shared: limpid_metrics_schema::MetricsSnapshot = serde_json::from_str(fixture).unwrap();
        let expected = json_to_prometheus(fixture).unwrap();
        let families = parse_snapshot(&shared).unwrap();
        assert_eq!(render_snapshot(&families), expected);
    }

    #[test]
    fn build_info_translates_through_the_generic_schema_v1_path() {
        let snapshot = serde_json::json!({
            "schema": 1,
            "metrics": [{
                "name": "limpid_build_info",
                "type": "gauge",
                "help": "Build information for the running limpid node.",
                "series": [{
                    "labels": {"node_id": "edge-a", "version": "0.7.15"},
                    "value": 1
                }]
            }]
        });

        assert_eq!(
            json_to_prometheus(&snapshot.to_string()).unwrap(),
            concat!(
                "# HELP limpid_build_info Build information for the running limpid node.\n",
                "# TYPE limpid_build_info gauge\n",
                "limpid_build_info{node_id=\"edge-a\",version=\"0.7.15\"} 1\n\n"
            )
        );
    }

    #[test]
    fn schema_v1_rejects_inconsistent_and_colliding_labelsets() {
        let cases = [
            (
                "counter label-name mismatch",
                serde_json::json!([{
                    "name": "counter_metric",
                    "type": "counter",
                    "help": "Counter.",
                    "series": [
                        {"labels": {"left": "one"}, "value": 1},
                        {"labels": {"right": "two"}, "value": 2}
                    ]
                }]),
            ),
            (
                "histogram label-name mismatch",
                serde_json::json!([{
                    "name": "histogram_metric",
                    "type": "histogram",
                    "help": "Histogram.",
                    "series": [
                        {"labels": {"left": "one"}, "buckets": [], "sum": 0.0, "count": 0},
                        {"labels": {"right": "two"}, "buckets": [], "sum": 0.0, "count": 0}
                    ]
                }]),
            ),
            (
                "histogram mapped labelset collision",
                serde_json::json!([{
                    "name": "histogram_metric",
                    "type": "histogram",
                    "help": "Histogram.",
                    "series": [
                        {"labels": {"le": "x"}, "buckets": [], "sum": 0.0, "count": 0},
                        {"labels": {"le_": "x"}, "buckets": [], "sum": 0.0, "count": 0}
                    ]
                }]),
            ),
        ];

        for (case, metrics) in cases {
            let snapshot = serde_json::json!({"schema": 1, "metrics": metrics});
            assert!(json_to_prometheus(&snapshot.to_string()).is_err(), "{case}");
        }

        let different_values = serde_json::json!({
            "schema": 1,
            "metrics": [{
                "name": "counter_metric",
                "type": "counter",
                "help": "Counter.",
                "series": [
                    {"labels": {"scope": "one"}, "value": 1},
                    {"labels": {"scope": "two"}, "value": 2}
                ]
            }]
        });
        assert!(json_to_prometheus(&different_values.to_string()).is_ok());
    }

    #[test]
    fn mapped_histogram_labelsets_are_deduplicated_defensively() {
        let series = [
            ExpositionSeries::Histogram {
                labels: vec![("le".to_string(), "x".to_string())],
                buckets: Vec::new(),
                sum: 0.0,
                count: 0,
            },
            ExpositionSeries::Histogram {
                labels: vec![("le_".to_string(), "x".to_string())],
                buckets: Vec::new(),
                sum: 0.0,
                count: 0,
            },
        ];
        assert!(
            validate_exported_labelsets(&series, ExpositionType::Histogram, "histogram_metric")
                .is_err()
        );
    }

    #[test]
    fn schema_v1_rejects_histogram_derived_family_name_collisions() {
        let histogram = |name: &str| {
            serde_json::json!({
                "name": name,
                "type": "histogram",
                "help": "Histogram.",
                "series": [{"labels": {}, "buckets": [], "sum": 0.0, "count": 0}]
            })
        };
        let counter = |name: &str| {
            serde_json::json!({
                "name": name,
                "type": "counter",
                "help": "Counter.",
                "series": [{"labels": {}, "value": 0}]
            })
        };
        let cases = [
            vec![histogram("latency"), counter("latency_bucket")],
            vec![counter("latency_sum"), histogram("latency")],
            vec![histogram("latency"), counter("latency_count")],
        ];

        for metrics in cases {
            let snapshot = serde_json::json!({"schema": 1, "metrics": metrics});
            assert!(json_to_prometheus(&snapshot.to_string()).is_err());
        }
    }

    #[test]
    fn schema_v1_validation_rejects_malformed_families_and_series() {
        let cases = [
            (
                "duplicate family name",
                r#"{"schema":1,"metrics":[{"name":"same","type":"counter","help":"One.","series":[]},{"name":"same","type":"gauge","help":"Two.","series":[]}]}"#,
            ),
            (
                "duplicate canonical labelset",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"counter","help":"Metric.","series":[{"labels":{"a":"one","b":"two"},"value":1},{"labels":{"b":"two","a":"one"},"value":2}]}]}"#,
            ),
            (
                "missing family field",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"counter","series":[]}]}"#,
            ),
            (
                "missing series field",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"counter","help":"Metric.","series":[{"labels":{}}]}]}"#,
            ),
            (
                "unsupported type",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"summary","help":"Metric.","series":[]}]}"#,
            ),
            (
                "equal histogram bounds",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[1.0,1],[1.0,2]],"sum":1.0,"count":2}]}]}"#,
            ),
            (
                "descending histogram bounds",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[2.0,1],[1.0,2]],"sum":1.0,"count":2}]}]}"#,
            ),
            (
                "decreasing cumulative counts",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[1.0,2],[2.0,1]],"sum":1.0,"count":2}]}]}"#,
            ),
            (
                "malformed bucket tuple",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[1.0]],"sum":1.0,"count":1}]}]}"#,
            ),
            (
                "non-u64 bucket count",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[1.0,1.5]],"sum":1.0,"count":1}]}]}"#,
            ),
            (
                "non-u64 total count",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[],"sum":1.0,"count":1.5}]}]}"#,
            ),
            (
                "non-finite bound represented as null",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[[null,1]],"sum":1.0,"count":1}]}]}"#,
            ),
            (
                "non-finite sum represented as null",
                r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[],"sum":null,"count":0}]}]}"#,
            ),
        ];

        for (case, json) in cases {
            assert!(json_to_prometheus(json).is_err(), "{case}");
        }

        let empty_histogram = r#"{"schema":1,"metrics":[{"name":"metric","type":"histogram","help":"Metric.","series":[{"labels":{},"buckets":[],"sum":0.0,"count":0}]}]}"#;
        assert!(json_to_prometheus(empty_histogram).is_ok());
    }

    #[test]
    fn schema_v1_ignores_unknown_fields_at_every_wire_level() {
        let baseline = serde_json::json!({
            "schema": 1,
            "metrics": [
                {
                    "name": "requests_total",
                    "type": "counter",
                    "help": "Requests.",
                    "series": [{"labels": {"route": "west"}, "value": 7}]
                },
                {
                    "name": "latency_seconds",
                    "type": "histogram",
                    "help": "Latency.",
                    "series": [{
                        "labels": {"route": "west"},
                        "buckets": [[0.5, 2], [1.0, 3]],
                        "sum": 1.75,
                        "count": 4
                    }]
                }
            ]
        });
        let expected = json_to_prometheus(&baseline.to_string()).unwrap();

        let mut root = baseline.clone();
        root["future_root"] = serde_json::json!({"version": 2});

        let mut family = baseline.clone();
        family["metrics"][0]["future_family"] = serde_json::json!(["ignored"]);

        let mut value_series = baseline.clone();
        value_series["metrics"][0]["series"][0]["future_value_series"] =
            serde_json::json!({"ignored": true});

        let mut histogram_series = baseline.clone();
        histogram_series["metrics"][1]["series"][0]["future_histogram_series"] =
            serde_json::json!("ignored");

        let translations = [
            ("root", root),
            ("family", family),
            ("value series", value_series),
            ("histogram series", histogram_series),
        ]
        .map(|(level, snapshot)| (level, json_to_prometheus(&snapshot.to_string())));

        for (level, translation) in translations {
            assert_eq!(
                translation.unwrap(),
                expected,
                "unknown {level} field must not change exposition"
            );
        }
    }

    #[test]
    fn prometheus_legacy_metric_name_grammar_is_enforced_before_rendering() {
        for name in ["a", "_", ":", "A0_:"] {
            let snapshot = serde_json::json!({
                "schema": 1,
                "metrics": [{
                    "name": name,
                    "type": "counter",
                    "help": "Valid.",
                    "series": [{"labels": {}, "value": 1}]
                }]
            });
            assert!(
                json_to_prometheus(&snapshot.to_string()).is_ok(),
                "{name:?}"
            );
        }

        for name in [
            "0bad",
            "bad name",
            "bad{",
            "bad=",
            "bad\"",
            "bad\nname",
            "é",
        ] {
            let snapshot = serde_json::json!({
                "schema": 1,
                "metrics": [
                    {
                        "name": "valid_metric",
                        "type": "counter",
                        "help": "Valid.",
                        "series": [{"labels": {}, "value": 1}]
                    },
                    {
                        "name": name,
                        "type": "counter",
                        "help": "Invalid.",
                        "series": [{"labels": {}, "value": 2}]
                    }
                ]
            });
            assert!(
                json_to_prometheus(&snapshot.to_string()).is_err(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn prometheus_legacy_label_name_grammar_is_enforced_for_every_metric_type() {
        for label in ["a", "_", "A0_"] {
            let snapshot = serde_json::json!({
                "schema": 1,
                "metrics": [{
                    "name": "valid_metric",
                    "type": "gauge",
                    "help": "Valid.",
                    "series": [{"labels": {label: "value"}, "value": 1}]
                }]
            });
            assert!(
                json_to_prometheus(&snapshot.to_string()).is_ok(),
                "{label:?}"
            );
        }

        let invalid = [
            "",
            "0bad",
            "bad:name",
            "bad name",
            "bad{",
            "bad=",
            "bad\"",
            "bad\nname",
            "é",
            "__x",
            "__",
        ];
        for (index, label) in invalid.into_iter().enumerate() {
            let (metric_type, series) = match index % 3 {
                0 => (
                    "counter",
                    serde_json::json!({"labels": {label: "value"}, "value": 1}),
                ),
                1 => (
                    "gauge",
                    serde_json::json!({"labels": {label: "value"}, "value": 1}),
                ),
                _ => (
                    "histogram",
                    serde_json::json!({
                        "labels": {label: "value"},
                        "buckets": [],
                        "sum": 0.0,
                        "count": 0
                    }),
                ),
            };
            let snapshot = serde_json::json!({
                "schema": 1,
                "metrics": [{
                    "name": "invalid_label_metric",
                    "type": metric_type,
                    "help": "Invalid.",
                    "series": [series]
                }]
            });
            assert!(
                json_to_prometheus(&snapshot.to_string()).is_err(),
                "{label:?}"
            );
        }
    }

    #[test]
    fn schema_v1_translation_is_generic_deterministic_and_prometheus_exact() {
        let snapshot = serde_json::json!({
            "schema": 1,
            "metrics": [
                {
                    "name": "metric_zeta_seconds",
                    "type": "histogram",
                    "help": "Latency distribution.",
                    "series": [{
                        "labels": {
                            "route": "west",
                            "le__": "source-two",
                            "az": "two",
                            "le": "source-zero",
                            "le_": "source-one"
                        },
                        "buckets": [[0.125, 17], [0.875, 37]],
                        "sum": 13.75,
                        "count": 31
                    }]
                },
                {
                    "name": "metric_yankee_seconds",
                    "type": "histogram",
                    "help": "Non-colliding histogram.",
                    "series": [{
                        "labels": {"zone": "north", "le_": "unchanged"},
                        "buckets": [[1.5, 3]],
                        "sum": 2.5,
                        "count": 3
                    }]
                },
                {
                    "name": "metric_alpha_total",
                    "type": "counter",
                    "help": "Count\\path\nnext",
                    "series": [
                        {"labels": {"z": "first", "le": "counter-two", "a": "two"}, "value": 2},
                        {"labels": {"z": "last", "a": "one", "le": "counter-one"}, "value": 1}
                    ]
                },
                {
                    "name": "metric_middle",
                    "type": "gauge",
                    "help": "Current depth.",
                    "series": [{
                        "labels": {"le": "gauge", "env": "prod\"east\\one\nline"},
                        "value": 9
                    }]
                }
            ]
        });

        let expected = r#"# HELP metric_alpha_total Count\\path\nnext
# TYPE metric_alpha_total counter
metric_alpha_total{a="one",le="counter-one",z="last"} 1
metric_alpha_total{a="two",le="counter-two",z="first"} 2

# HELP metric_middle Current depth.
# TYPE metric_middle gauge
metric_middle{env="prod\"east\\one\nline",le="gauge"} 9

# HELP metric_yankee_seconds Non-colliding histogram.
# TYPE metric_yankee_seconds histogram
metric_yankee_seconds_bucket{le="1.5",le_="unchanged",zone="north"} 3
metric_yankee_seconds_bucket{le="+Inf",le_="unchanged",zone="north"} 3
metric_yankee_seconds_sum{le_="unchanged",zone="north"} 2.5
metric_yankee_seconds_count{le_="unchanged",zone="north"} 3

# HELP metric_zeta_seconds Latency distribution.
# TYPE metric_zeta_seconds histogram
metric_zeta_seconds_bucket{az="two",le="0.125",le_="source-zero",le__="source-one",le___="source-two",route="west"} 17
metric_zeta_seconds_bucket{az="two",le="0.875",le_="source-zero",le__="source-one",le___="source-two",route="west"} 37
metric_zeta_seconds_bucket{az="two",le="+Inf",le_="source-zero",le__="source-one",le___="source-two",route="west"} 31
metric_zeta_seconds_sum{az="two",le_="source-zero",le__="source-one",le___="source-two",route="west"} 13.75
metric_zeta_seconds_count{az="two",le_="source-zero",le__="source-one",le___="source-two",route="west"} 31

"#;

        assert_eq!(json_to_prometheus(&snapshot.to_string()).unwrap(), expected);
    }

    #[test]
    fn schema_v1_translation_accepts_process_dimensions_generically() {
        let snapshot = serde_json::json!({
            "schema": 1,
            "metrics": [
                {
                    "name": "limpid_process_events_in_total",
                    "type": "counter",
                    "help": "Process invocations started.",
                    "series": [{
                        "labels": {
                            "step": "2",
                            "pipeline": "main",
                            "process_name": "leaf",
                            "process_path": "/dispatch/leaf"
                        },
                        "value": 7
                    }]
                },
                {
                    "name": "limpid_process_events_out_total",
                    "type": "counter",
                    "help": "Process invocations continued.",
                    "series": [{
                        "labels": {
                            "pipeline": "main",
                            "step": "2",
                            "process_path": "/dispatch/leaf",
                            "process_name": "leaf"
                        },
                        "value": 5
                    }]
                },
                {
                    "name": "limpid_process_events_dropped_total",
                    "type": "counter",
                    "help": "Process invocations dropped.",
                    "series": [{
                        "labels": {
                            "pipeline": "main",
                            "step": "2",
                            "process_path": "/dispatch/leaf",
                            "process_name": "leaf"
                        },
                        "value": 1
                    }]
                },
                {
                    "name": "limpid_process_events_errored_total",
                    "type": "counter",
                    "help": "Process invocations errored.",
                    "series": [{
                        "labels": {
                            "pipeline": "main",
                            "step": "2",
                            "process_path": "/dispatch/leaf",
                            "process_name": "leaf"
                        },
                        "value": 1
                    }]
                }
            ]
        });
        let expected = r#"# HELP limpid_process_events_dropped_total Process invocations dropped.
# TYPE limpid_process_events_dropped_total counter
limpid_process_events_dropped_total{pipeline="main",process_name="leaf",process_path="/dispatch/leaf",step="2"} 1

# HELP limpid_process_events_errored_total Process invocations errored.
# TYPE limpid_process_events_errored_total counter
limpid_process_events_errored_total{pipeline="main",process_name="leaf",process_path="/dispatch/leaf",step="2"} 1

# HELP limpid_process_events_in_total Process invocations started.
# TYPE limpid_process_events_in_total counter
limpid_process_events_in_total{pipeline="main",process_name="leaf",process_path="/dispatch/leaf",step="2"} 7

# HELP limpid_process_events_out_total Process invocations continued.
# TYPE limpid_process_events_out_total counter
limpid_process_events_out_total{pipeline="main",process_name="leaf",process_path="/dispatch/leaf",step="2"} 5

"#;
        assert_eq!(json_to_prometheus(&snapshot.to_string()).unwrap(), expected);
    }

    #[test]
    fn schema_v1_translation_rejects_legacy_unsupported_and_partial_snapshots() {
        let cases = [
            ("invalid json", "{"),
            ("legacy envelope", r#"{"inputs": {}}"#),
            ("unsupported schema", r#"{"schema": 2, "metrics": []}"#),
            ("missing metrics", r#"{"schema": 1}"#),
            (
                "malformed family after valid family",
                r#"{
                    "schema": 1,
                    "metrics": [
                        {
                            "name": "valid_arbitrary_total",
                            "type": "counter",
                            "help": "Valid.",
                            "series": [{"labels": {"scope": "one"}, "value": 1}]
                        },
                        {
                            "name": "broken_arbitrary_gauge",
                            "type": "gauge",
                            "help": "Broken.",
                            "series": [{"labels": {"scope": "two"}, "value": true}]
                        }
                    ]
                }"#,
            ),
        ];

        for (case, json) in cases {
            assert!(
                json_to_prometheus(json).is_err(),
                "{case} must fail without partial exposition"
            );
        }
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

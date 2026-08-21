//! limpidctl: control and debug CLI for limpid.
//!
//! Usage:
//!   limpidctl tap input <name> [--json]     Stream events from a named input
//!   limpidctl tap process <name> [--json]   Stream events after a named process
//!   limpidctl tap output <name> [--json]    Stream events from a named output
//!   limpidctl inject input <name> [--json]  Inject stdin lines into a named input
//!   limpidctl inject output <name> [--json] Inject stdin lines into a named output queue
//!   limpidctl list [--json]                 List pipelines and tap points
//!   limpidctl stats [--details|--raw|--json] Show pipeline/output metrics
//!   limpidctl health [--json]               Check daemon health
//!
//! Connects to limpid's control socket (default: /var/run/limpid/control.sock).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use limpid_metrics_schema::{
    DROPPED_ROOT_PROCESS_NAME, DROPPED_ROOT_STEP, EVENTS_DROPPED_TOTAL, MetricFamily, MetricType,
    MetricsSnapshot, PROCESS_PATH_ROOT,
};

const DEFAULT_SOCKET: &str = "/var/run/limpid/control.sock";

#[derive(Parser)]
#[command(name = "limpidctl", about = "Control and debug CLI for limpid")]
struct Cli {
    /// Control socket path
    #[arg(long, global = true, default_value = DEFAULT_SOCKET)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stream events from a tap point
    Tap {
        #[command(subcommand)]
        kind: TapKind,
    },
    /// List pipelines and tap points
    List {
        /// Output raw JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// Show pipeline/input/output metrics
    Stats {
        /// Output schema-v1 JSON instead of formatted text
        #[arg(long, conflicts_with_all = ["details", "raw"])]
        json: bool,
        /// Show expanded human-readable metrics
        #[arg(long, conflicts_with_all = ["json", "raw"])]
        details: bool,
        /// Show every metric family and series in the legacy text format
        #[arg(long, conflicts_with_all = ["json", "details"])]
        raw: bool,
    },
    /// Check daemon health
    Health {
        /// Output raw JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// Inject events into an input or output (reads from stdin, one per line)
    Inject {
        #[command(subcommand)]
        kind: InjectKind,
    },
    /// Manage LTP node identity material
    Ltp {
        #[command(subcommand)]
        command: LtpCommand,
    },
}

#[derive(Subcommand)]
enum LtpCommand {
    /// Generate a new Ed25519 node key without overwriting an existing path
    Keygen { path: PathBuf },
}

#[derive(Subcommand)]
enum InjectKind {
    /// Push raw lines or full-Event JSON into a named input's channel
    Input {
        name: String,
        /// Each stdin line is a full Event JSON (as emitted by `tap --json`)
        #[arg(long)]
        json: bool,
        /// Replay events at their original timing using each event's `received_at` field.
        /// Accepts `realtime` (= `1x`) or a factor like `10x` / `0.2x`.
        /// Defaults to `1x` when given without a value. Requires `--json`.
        #[arg(long, value_name = "FACTOR", num_args = 0..=1, default_missing_value = "1x")]
        replay_timing: Option<String>,
    },
    /// Push raw lines or full-Event JSON directly into a named output's queue
    Output {
        name: String,
        /// Each stdin line is a full Event JSON (as emitted by `tap --json`)
        #[arg(long)]
        json: bool,
        /// Replay events at their original timing using each event's `received_at` field.
        /// Accepts `realtime` (= `1x`) or a factor like `10x` / `0.2x`.
        /// Defaults to `1x` when given without a value. Requires `--json`.
        #[arg(long, value_name = "FACTOR", num_args = 0..=1, default_missing_value = "1x")]
        replay_timing: Option<String>,
    },
}

#[derive(Subcommand)]
enum TapKind {
    /// Stream events entering a named input
    Input {
        name: String,
        /// Stream full Event as JSON (one per line) instead of raw message
        #[arg(long)]
        json: bool,
    },
    /// Stream events after a named process
    Process {
        name: String,
        /// Stream full Event as JSON (one per line) instead of raw message
        #[arg(long)]
        json: bool,
    },
    /// Stream events from a named output
    Output {
        name: String,
        /// Stream full Event as JSON (one per line) instead of raw message
        #[arg(long)]
        json: bool,
    },
}

/// True when `name` is a valid limpid identifier: `[A-Za-z_][A-Za-z0-9_]*`,
/// matching the daemon-side DSL `ident` rule (see `dsl/limpid.pest`).
/// Input / process / output names can only ever be idents, so anything
/// else — whitespace, newlines, control characters — cannot name a
/// tap/inject target and would only serve to smuggle extra tokens (or
/// extra protocol lines) into the line-based control command.
/// Validating client-side yields a clear error before any bytes are
/// sent, instead of a confusing daemon-side parse failure.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Exit(2) — usage-error convention, same as the --replay-timing
/// checks — unless `name` is a valid identifier.
fn require_valid_name(name: &str) {
    if !is_valid_name(name) {
        eprintln!(
            "error: invalid name {:?}: expected an identifier ([A-Za-z_][A-Za-z0-9_]*), \
             matching the daemon's input/process/output name grammar",
            name
        );
        std::process::exit(2);
    }
}

/// The daemon's protocol-level failure shape: a plain-text line
/// starting with `error:` (server busy, command too long, unknown
/// tap point, ...). Returns the message after the prefix. JSON
/// responses (`{...}`) never match.
fn daemon_error_line(response: &str) -> Option<&str> {
    response.trim().strip_prefix("error:")
}

/// Exit(1) if `response` is a daemon `error:` line. Applied to every
/// query-style command (list / stats / health) so scripts can rely on
/// the exit code — mirrors the inject path's existing handling.
fn exit_on_daemon_error(response: &str) {
    if let Some(rest) = daemon_error_line(response) {
        eprintln!("error:{}", rest);
        std::process::exit(1);
    }
}

/// True when a `health` response reports a healthy daemon. Anything
/// else — unparseable body, missing field, non-"ok" status — counts
/// as unhealthy, so `limpidctl health` can gate scripts and probes
/// via its exit code instead of forcing them to parse the output.
fn health_is_ok(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(|s| s == "ok"))
        .unwrap_or(false)
}

fn main() {
    // Restore the default SIGPIPE disposition so writes to a closed
    // downstream pipe terminate the process via signal instead of
    // panicking from the stdio writer. Rust installs `SIG_IGN` for
    // SIGPIPE by default, which turns the broken-pipe condition into
    // an `EPIPE` that the println!/print! infrastructure escalates to
    // a panic — ugly for `limpidctl stats | head`. Matches what
    // ripgrep / fd / bat do.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Tap { kind } => {
            let (kind_str, name, json) = match kind {
                TapKind::Input { name, json } => ("input", name, json),
                TapKind::Process { name, json } => ("process", name, json),
                TapKind::Output { name, json } => ("output", name, json),
            };
            require_valid_name(&name);
            let command = if json {
                format!("tap {} {} json", kind_str, name)
            } else {
                format!("tap {} {}", kind_str, name)
            };
            run_tap(&cli.socket, &command);
        }
        Command::List { json } => {
            let response = query_command(&cli.socket, "list");
            exit_on_daemon_error(&response);
            if json {
                print!("{}", response);
            } else {
                format_list(&response);
            }
        }
        Command::Stats { json, details, raw } => {
            let response = query_command(&cli.socket, "stats");
            exit_on_daemon_error(&response);
            if json {
                print!("{}", response);
            } else if raw {
                format_stats_raw(&response);
            } else if details {
                format_stats_details(&response);
            } else {
                format_stats(&response);
            }
        }
        Command::Inject { kind } => {
            let (kind_str, name, json, replay_timing) = match kind {
                InjectKind::Input {
                    name,
                    json,
                    replay_timing,
                } => ("input", name, json, replay_timing),
                InjectKind::Output {
                    name,
                    json,
                    replay_timing,
                } => ("output", name, json, replay_timing),
            };
            require_valid_name(&name);
            let replay = match replay_timing {
                None => None,
                Some(spec) => {
                    if !json {
                        eprintln!(
                            "error: --replay-timing requires --json (raw line mode has no timestamps)"
                        );
                        std::process::exit(2);
                    }
                    match parse_replay_factor(&spec) {
                        Ok(f) => Some(f),
                        Err(e) => {
                            eprintln!("error: invalid --replay-timing value {:?}: {}", spec, e);
                            std::process::exit(2);
                        }
                    }
                }
            };
            let command = if json {
                format!("inject {} {} json", kind_str, name)
            } else {
                format!("inject {} {}", kind_str, name)
            };
            run_inject(&cli.socket, &command, replay);
        }
        Command::Health { json } => {
            let response = query_command(&cli.socket, "health");
            exit_on_daemon_error(&response);
            if json {
                print!("{}", response);
            } else {
                format_health(&response);
            }
            // Non-zero exit for anything but an explicit healthy
            // status, so probes (systemd, k8s, shell `&&` chains) can
            // use the exit code without parsing the body. Printed
            // output above is preserved either way.
            if !health_is_ok(&response) {
                std::process::exit(1);
            }
        }
        Command::Ltp { command } => match command {
            LtpCommand::Keygen { path } => match generate_node_key(&path) {
                Ok(public_key) => {
                    println!("{public_key}");
                    eprintln!("generated Ed25519 node key at {}", path.display());
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            },
        },
    }
}

const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn generate_node_key(path: &std::path::Path) -> Result<String, String> {
    use ring::signature::KeyPair as _;

    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
        .map_err(|_| "failed to generate Ed25519 private key".to_owned())?;
    let key_pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| "generated Ed25519 private key failed validation".to_owned())?;
    let private_pem = pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref()));
    write_new_private_key(path, private_pem.as_bytes())?;

    let mut spki =
        Vec::with_capacity(ED25519_SPKI_PREFIX.len() + key_pair.public_key().as_ref().len());
    spki.extend_from_slice(&ED25519_SPKI_PREFIX);
    spki.extend_from_slice(key_pair.public_key().as_ref());
    Ok(base64::engine::general_purpose::STANDARD.encode(spki))
}

fn write_new_private_key(path: &std::path::Path, contents: &[u8]) -> Result<(), String> {
    write_new_private_key_with(
        path,
        contents,
        |file, contents| {
            file.write_all(contents)?;
            file.sync_all()
        },
        || Ok(()),
        |parent| std::fs::File::open(parent)?.sync_all(),
    )
}

fn write_new_private_key_with<W, B, S>(
    path: &std::path::Path,
    contents: &[u8],
    write_and_sync: W,
    before_publish: B,
    sync_parent: S,
) -> Result<(), String>
where
    W: FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    B: FnOnce() -> std::io::Result<()>,
    S: FnOnce(&std::path::Path) -> std::io::Result<()>,
{
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".limpid-node-key.")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "cannot create temporary node key beside '{}': {error}",
                path.display()
            )
        })?;

    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| write_and_sync(temporary.as_file_mut(), contents))
        .map_err(|error| format!("failed to persist node key '{}': {error}", path.display()))?;
    before_publish()
        .map_err(|error| format!("failed to publish node key '{}': {error}", path.display()))?;

    let final_file = temporary.persist_noclobber(path).map_err(|error| {
        format!(
            "cannot publish node key '{}': {}",
            path.display(),
            error.error
        )
    })?;
    sync_parent(parent).map_err(|error| {
        format!(
            "node key '{}' was published but parent directory sync failed: {error}",
            path.display()
        )
    })?;
    drop(final_file);
    Ok(())
}

fn run_tap(socket: &PathBuf, command: &str) {
    let mut stream = connect(socket);
    if let Err(e) = writeln!(stream, "{}", command) {
        eprintln!("Failed to send command: {}", e);
        std::process::exit(1);
    }
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        match line {
            Ok(text) => println!("{}", text),
            Err(_) => break,
        }
    }
}

fn run_inject(socket: &PathBuf, command: &str, replay: Option<f64>) {
    let mut stream = connect(socket);
    if let Err(e) = writeln!(stream, "{}", command) {
        eprintln!("Failed to send command: {}", e);
        std::process::exit(1);
    }

    // Copy stdin line-by-line to the socket. When `replay` is set, gate each
    // line on the event's `timestamp` field so the daemon receives events at
    // their original (or scaled) cadence.
    let stdin = std::io::stdin();
    let stdin_lock = stdin.lock();
    let stdin_reader = BufReader::new(stdin_lock);
    let mut replay_state: Option<ReplayState> = replay.map(ReplayState::new);

    for line in stdin_reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to read stdin: {}", e);
                std::process::exit(1);
            }
        };
        // Skip blank lines without disturbing replay state — they carry no event.
        if line.trim().is_empty() {
            continue;
        }
        if let Some(state) = replay_state.as_mut() {
            match extract_timestamp(&line) {
                Ok(ts) => state.wait_for(ts),
                Err(e) => {
                    eprintln!("error: --replay-timing: {}", e);
                    std::process::exit(1);
                }
            }
        }
        if let Err(e) = writeln!(stream, "{}", line) {
            eprintln!("Failed to write to daemon: {}", e);
            std::process::exit(1);
        }
    }

    // Signal EOF to the daemon so it finalizes and sends the response.
    if let Err(e) = stream.shutdown(std::net::Shutdown::Write) {
        eprintln!("Failed to shut down write half: {}", e);
        std::process::exit(1);
    }

    // Read single-line response.
    let reader = BufReader::new(stream);
    let mut response = String::new();
    for line in reader.lines() {
        match line {
            Ok(text) => {
                if !response.is_empty() {
                    response.push('\n');
                }
                response.push_str(&text);
            }
            Err(_) => break,
        }
    }

    let trimmed = response.trim();
    if let Some(rest) = trimmed.strip_prefix("error:") {
        eprintln!("error:{}", rest);
        std::process::exit(1);
    }

    println!("{}", trimmed);
}

fn connect(socket: &PathBuf) -> UnixStream {
    match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to {:?}: {}", socket, e);
            eprintln!("Is limpid running?");
            std::process::exit(1);
        }
    }
}

fn query_command(socket: &PathBuf, command: &str) -> String {
    let mut stream = connect(socket);
    if let Err(e) = writeln!(stream, "{}", command) {
        eprintln!("Failed to send command: {}", e);
        std::process::exit(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let reader = BufReader::new(stream);
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
    result
}

fn format_health(json: &str) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => {
            print!("{}", json);
            return;
        }
    };

    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let uptime = v
        .get("uptime_seconds")
        .and_then(|u| u.as_u64())
        .unwrap_or(0);
    println!(
        "{} (uptime: {})",
        status.to_uppercase(),
        format_duration(uptime)
    );
}

fn format_duration(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

const PIPELINE_METRICS: [&str; 6] = [
    "limpid_pipeline_events_received_total",
    "limpid_pipeline_events_finished_total",
    "limpid_events_dropped_total",
    "limpid_pipeline_events_discarded_total",
    "limpid_pipeline_events_errored_total",
    "limpid_pipeline_events_errored_unwritable_total",
];
const INPUT_METRICS: [&str; 3] = [
    "limpid_input_events_received_total",
    "limpid_input_events_invalid_total",
    "limpid_input_events_injected_total",
];
const OUTPUT_METRICS: [&str; 7] = [
    "limpid_output_events_received_total",
    "limpid_output_events_injected_total",
    "limpid_output_events_written_total",
    "limpid_output_events_failed_total",
    "limpid_output_retries_total",
    "limpid_output_events_wedged_total",
    "limpid_output_events_errored_unwritable_total",
];
const PROCESS_METRICS: [&str; 4] = [
    "limpid_process_events_in_total",
    "limpid_process_events_out_total",
    DROPPED_EVENTS_METRIC,
    "limpid_process_events_errored_total",
];
const DROPPED_EVENTS_METRIC: &str = EVENTS_DROPPED_TOTAL;

type MetricValues = BTreeMap<String, u64>;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct ProcessIdentity {
    pipeline: String,
    step: usize,
    process_path: String,
    process_name: String,
}

impl ProcessIdentity {
    fn depth(&self) -> usize {
        self.process_path
            .bytes()
            .filter(|byte| *byte == b'/')
            .count()
    }
}

type ProcessMetricValues = BTreeMap<ProcessIdentity, u64>;

fn process_tree_names(identities: &[&ProcessIdentity]) -> Option<Vec<(String, String)>> {
    let mut active_ancestors: Vec<&ProcessIdentity> = Vec::new();
    for identity in identities {
        let depth = identity.depth();
        if depth == 0 {
            return None;
        }
        active_ancestors.truncate(depth - 1);
        if depth > 1 {
            let parent = active_ancestors.get(depth - 2)?;
            let (parent_path, _) = identity.process_path.rsplit_once('/')?;
            if parent.process_path != parent_path {
                return None;
            }
        }
        active_ancestors.push(*identity);
    }

    let depths = identities
        .iter()
        .map(|identity| identity.depth())
        .collect::<Vec<_>>();
    let is_last_sibling = depths
        .iter()
        .enumerate()
        .map(|(index, depth)| {
            depths[index + 1..]
                .iter()
                .find(|next_depth| **next_depth <= *depth)
                .is_none_or(|next_depth| *next_depth < *depth)
        })
        .collect::<Vec<_>>();

    let mut ancestor_is_last = Vec::new();
    let names = identities
        .iter()
        .enumerate()
        .map(|(index, identity)| {
            ancestor_is_last.truncate(identity.depth().saturating_sub(1));
            let mut rendered = String::new();
            for is_last in &ancestor_is_last {
                rendered.push_str(if *is_last { "   " } else { "│  " });
            }
            let mut detail_prefix = rendered.clone();
            rendered.push_str(if is_last_sibling[index] {
                "└─ "
            } else {
                "├─ "
            });
            detail_prefix.push_str(if is_last_sibling[index] {
                "   "
            } else {
                "│  "
            });
            rendered.push_str(&identity.process_name);
            ancestor_is_last.push(is_last_sibling[index]);
            (rendered, detail_prefix)
        })
        .collect();
    Some(names)
}

fn format_stats(json: &str) {
    let rendered = serde_json::from_str::<MetricsSnapshot>(json)
        .ok()
        .and_then(|snapshot| render_default_stats(&snapshot));
    match rendered {
        Some(rendered) => print!("{}", rendered),
        None => print!("{}", json),
    }
}

/// Renders the operator table (Pipelines, Inputs, Outputs) from a
/// schema v1 snapshot. Families outside the canonical 16 are
/// skipped here so the layout stays operator-runbook-shaped —
/// `--details` adds them in a separate human-readable section, while
/// `--raw` preserves the complete legacy family exposition. A canonical
/// family that fails validation returns `None`, so the caller falls
/// back to the raw response rather than emit a partial table that
/// omits or lies about a counter.
fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
    render_default_stats_inner(snapshot, true, false)
}

fn render_default_stats_inner(
    snapshot: &MetricsSnapshot,
    validate_process_families: bool,
    show_process_identity: bool,
) -> Option<String> {
    if snapshot.schema != 1 {
        return None;
    }
    let mut families = BTreeMap::<String, MetricValues>::new();
    let mut process_families = BTreeMap::<String, ProcessMetricValues>::new();

    for metric in &snapshot.metrics {
        let name = metric.name();
        if name == DROPPED_EVENTS_METRIC {
            if metric.metric_type() != MetricType::Counter
                || families.contains_key(name)
                || process_families.contains_key(name)
            {
                return None;
            }
            let series = metric.value_series()?;
            if series.is_empty() {
                return None;
            }
            let mut pipeline_values = MetricValues::new();
            let mut process_values = ProcessMetricValues::new();
            for item in series {
                if item.labels.get("process_path").map(String::as_str) == Some(PROCESS_PATH_ROOT) {
                    if item.labels.len() != 4 {
                        return None;
                    }
                    let pipeline = item.labels.get("pipeline")?.clone();
                    let step = item.labels.get("step")?;
                    let process_name = item.labels.get("process_name")?;
                    if step != DROPPED_ROOT_STEP || process_name != DROPPED_ROOT_PROCESS_NAME {
                        return None;
                    }
                    if pipeline_values.insert(pipeline, item.value).is_some() {
                        return None;
                    }
                } else if validate_process_families {
                    if item.labels.len() != 4 {
                        return None;
                    }
                    let process_path = item.labels.get("process_path")?.clone();
                    let process_name = item.labels.get("process_name")?.clone();
                    if limpid_metrics_schema::process_path_leaf(&process_path)? != process_name {
                        return None;
                    }
                    let identity = ProcessIdentity {
                        pipeline: item.labels.get("pipeline")?.clone(),
                        step: item.labels.get("step")?.parse().ok()?,
                        process_path,
                        process_name,
                    };
                    if process_values.insert(identity, item.value).is_some() {
                        return None;
                    }
                }
            }
            if pipeline_values.is_empty() {
                return None;
            }
            families.insert(name.to_owned(), pipeline_values);
            if validate_process_families && !process_values.is_empty() {
                process_families.insert(name.to_owned(), process_values);
            }
            continue;
        }
        if validate_process_families && PROCESS_METRICS.contains(&name) {
            if metric.metric_type() != MetricType::Counter || process_families.contains_key(name) {
                return None;
            }
            let series = metric.value_series()?;
            if series.is_empty() {
                return None;
            }
            let mut values = ProcessMetricValues::new();
            for item in series {
                if item.labels.len() != 4 {
                    return None;
                }
                let identity = ProcessIdentity {
                    pipeline: item.labels.get("pipeline")?.clone(),
                    step: item.labels.get("step")?.parse().ok()?,
                    process_path: item.labels.get("process_path")?.clone(),
                    process_name: item.labels.get("process_name")?.clone(),
                };
                if values.insert(identity, item.value).is_some() {
                    return None;
                }
            }
            process_families.insert(name.to_owned(), values);
            continue;
        }
        let Some(label_name) = known_metric_label(name) else {
            continue;
        };
        if metric.metric_type() != MetricType::Counter || families.contains_key(name) {
            return None;
        }
        let series = metric.value_series()?;
        if series.is_empty() {
            return None;
        }
        let mut values = MetricValues::new();
        for item in series {
            if item.labels.len() != 1 {
                return None;
            }
            let scope = item.labels.get(label_name)?;
            if values.insert(scope.clone(), item.value).is_some() {
                return None;
            }
        }
        families.insert(name.to_owned(), values);
    }

    validate_metric_group(&families, &PIPELINE_METRICS)?;
    validate_metric_group(&families, &INPUT_METRICS)?;
    validate_metric_group(&families, &OUTPUT_METRICS)?;
    let process_identities = if validate_process_families {
        validate_process_metric_group(&process_families)?
    } else {
        None
    };

    let mut rendered = String::new();
    writeln!(rendered, "Pipelines:").ok()?;
    for name in families.get(PIPELINE_METRICS[0])?.keys() {
        let received = metric_value(&families, PIPELINE_METRICS[0], name)?;
        let finished = metric_value(&families, PIPELINE_METRICS[1], name)?;
        let dropped = metric_value(&families, PIPELINE_METRICS[2], name)?;
        let discarded = metric_value(&families, PIPELINE_METRICS[3], name)?;
        let errored = metric_value(&families, PIPELINE_METRICS[4], name)?;
        let unwritable = metric_value(&families, PIPELINE_METRICS[5], name)?;
        // Both errored and errored_unwritable zero → compact row
        // (steady state). Either non-zero → the errored column is
        // shown (its value may be 0); the errored_unwritable column
        // is appended only when it is itself non-zero.
        if errored == 0 && unwritable == 0 {
            writeln!(
                rendered,
                "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded",
                name, received, finished, dropped, discarded
            )
            .ok()?;
        } else {
            write!(
                rendered,
                "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded  {:>8} errored",
                name, received, finished, dropped, discarded, errored
            )
            .ok()?;
            if unwritable > 0 {
                write!(rendered, "  {:>8} errored_unwritable", unwritable).ok()?;
            }
            rendered.push('\n');
        }
    }

    writeln!(rendered, "\nInputs:").ok()?;
    for name in families.get(INPUT_METRICS[0])?.keys() {
        writeln!(
            rendered,
            "  {:<24} {:>8} received  {:>8} invalid  {:>8} injected",
            name,
            metric_value(&families, INPUT_METRICS[0], name)?,
            metric_value(&families, INPUT_METRICS[1], name)?,
            metric_value(&families, INPUT_METRICS[2], name)?,
        )
        .ok()?;
    }

    writeln!(rendered, "\nOutputs:").ok()?;
    for name in families.get(OUTPUT_METRICS[0])?.keys() {
        let received = metric_value(&families, OUTPUT_METRICS[0], name)?;
        let injected = metric_value(&families, OUTPUT_METRICS[1], name)?;
        let written = metric_value(&families, OUTPUT_METRICS[2], name)?;
        let failed = metric_value(&families, OUTPUT_METRICS[3], name)?;
        let retries = metric_value(&families, OUTPUT_METRICS[4], name)?;
        let wedged = metric_value(&families, OUTPUT_METRICS[5], name)?;
        let unwritable = metric_value(&families, OUTPUT_METRICS[6], name)?;
        write!(
            rendered,
            "  {:<24} {:>8} received  {:>8} injected  {:>8} written  {:>8} failed  {:>8} retries",
            name, received, injected, written, failed, retries
        )
        .ok()?;
        // Alarm columns (`wedged`, `errored_unwritable`) print only
        // when non-zero — same rationale as the pipeline row above.
        if wedged > 0 {
            write!(rendered, "  {:>8} wedged", wedged).ok()?;
        }
        if unwritable > 0 {
            write!(rendered, "  {:>8} errored_unwritable", unwritable).ok()?;
        }
        rendered.push('\n');
    }
    if let Some(identities) = process_identities {
        writeln!(rendered, "\nProcesses:").ok()?;
        let identities = identities.keys().collect::<Vec<_>>();
        let mut process_rows = Vec::with_capacity(identities.len());
        let mut start = 0;
        while start < identities.len() {
            let pipeline = &identities[start].pipeline;
            let end = identities[start..]
                .iter()
                .position(|identity| identity.pipeline != *pipeline)
                .map_or(identities.len(), |offset| start + offset);
            process_rows.extend(
                identities[start..end]
                    .iter()
                    .copied()
                    .zip(process_tree_names(&identities[start..end])?),
            );
            start = end;
        }
        let name_width = process_rows
            .iter()
            .map(|(_, (name, _))| name.chars().count())
            .max()
            .unwrap_or(0);
        let mut current_pipeline = None;
        for (identity, (process_name, detail_prefix)) in process_rows {
            if current_pipeline != Some(identity.pipeline.as_str()) {
                writeln!(rendered, "  {}", identity.pipeline).ok()?;
                current_pipeline = Some(identity.pipeline.as_str());
            }
            writeln!(
                rendered,
                "  {:<name_width$} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored",
                process_name,
                process_metric_value(&process_families, PROCESS_METRICS[0], identity)?,
                process_metric_value(&process_families, PROCESS_METRICS[1], identity)?,
                process_metric_value(&process_families, PROCESS_METRICS[2], identity)?,
                process_metric_value(&process_families, PROCESS_METRICS[3], identity)?,
            )
            .ok()?;
            if show_process_identity {
                writeln!(
                    rendered,
                    "  {detail_prefix}step: {}  path: {}",
                    identity.step, identity.process_path
                )
                .ok()?;
            }
        }
    }
    Some(rendered)
}

fn validate_process_metric_group(
    families: &BTreeMap<String, ProcessMetricValues>,
) -> Option<Option<&ProcessMetricValues>> {
    if families.is_empty() {
        return Some(None);
    }
    let expected = families.get(PROCESS_METRICS[0])?;
    for name in PROCESS_METRICS {
        let values = families.get(name)?;
        if !values.keys().eq(expected.keys()) {
            return None;
        }
    }
    Some(Some(expected))
}

fn process_metric_value(
    families: &BTreeMap<String, ProcessMetricValues>,
    metric: &str,
    identity: &ProcessIdentity,
) -> Option<u64> {
    families.get(metric)?.get(identity).copied()
}

fn known_metric_label(name: &str) -> Option<&'static str> {
    if PIPELINE_METRICS.contains(&name) {
        Some("pipeline")
    } else if INPUT_METRICS.contains(&name) {
        Some("input")
    } else if OUTPUT_METRICS.contains(&name) {
        Some("output")
    } else {
        None
    }
}

fn validate_metric_group(families: &BTreeMap<String, MetricValues>, names: &[&str]) -> Option<()> {
    let expected = families.get(*names.first()?)?;
    for name in names {
        let values = families.get(*name)?;
        if !values.keys().eq(expected.keys()) {
            return None;
        }
    }
    Some(())
}

fn metric_value(
    families: &BTreeMap<String, MetricValues>,
    metric: &str,
    scope: &str,
) -> Option<u64> {
    families.get(metric)?.get(scope).copied()
}

struct DetailMetric {
    name: String,
    help: String,
    kind: DetailKind,
}

enum DetailKind {
    Counter(Vec<ValueDetail>),
    Gauge(Vec<ValueDetail>),
    Histogram(Vec<HistogramDetail>),
}

struct ValueDetail {
    labels: Vec<(String, String)>,
    value: u64,
}

struct HistogramDetail {
    labels: Vec<(String, String)>,
    buckets: Vec<(f64, u64)>,
    sum: f64,
    count: u64,
}

fn format_stats_details(json: &str) {
    let rendered = serde_json::from_str::<MetricsSnapshot>(json)
        .ok()
        .and_then(|snapshot| render_stats_details(&snapshot));
    match rendered {
        Some(rendered) => print!("{}", rendered),
        None => print!("{}", json),
    }
}

fn format_stats_raw(json: &str) {
    let rendered = serde_json::from_str::<MetricsSnapshot>(json)
        .ok()
        .and_then(|snapshot| render_stats_raw(&snapshot));
    match rendered {
        Some(rendered) => print!("{}", rendered),
        None => print!("{}", json),
    }
}

fn render_stats_details(snapshot: &MetricsSnapshot) -> Option<String> {
    if snapshot.schema != 1 {
        return None;
    }
    let mut metrics: Vec<DetailMetric> = snapshot
        .metrics
        .iter()
        .map(parse_detail_metric)
        .collect::<Option<_>>()?;
    // If a canonical family is present but the canonical set
    // fails default validation, `--details` also bails to the
    // raw fallback so no partial human view of the canonical
    // subset leaks. Well-formed non-canonical families are
    // intentionally unaffected — the fallback scopes to the
    // canonical subset only.
    let has_canonical_metrics = metrics.iter().any(|metric| is_default_metric(&metric.name));
    let (summary, has_process_summary) = if has_canonical_metrics {
        let base = render_default_stats_inner(snapshot, false, false)?;
        match render_default_stats_inner(snapshot, true, true) {
            Some(summary) => {
                let has_process_summary = summary.contains("\nProcesses:\n");
                (summary, has_process_summary)
            }
            None => (base, false),
        }
    } else {
        (String::new(), false)
    };
    metrics.sort_by(|left, right| left.name.cmp(&right.name));
    if metrics.windows(2).any(|pair| pair[0].name == pair[1].name) {
        return None;
    }

    let metrics = metrics;
    let mut rendered = summary;
    if !metrics.is_empty() {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        writeln!(rendered, "Metrics:").ok()?;
    }
    for metric in metrics {
        writeln!(rendered, "  {}", metric.name).ok()?;
        writeln!(rendered, "    type: {}", metric.kind.name()).ok()?;
        writeln!(rendered, "    help: {}", metric.help).ok()?;
        if PROCESS_METRICS.contains(&metric.name.as_str()) {
            let summary = if has_process_summary {
                if metric.name == DROPPED_EVENTS_METRIC {
                    "summarized in Pipelines and Processes above"
                } else {
                    "summarized in Processes above"
                }
            } else {
                "unavailable as a process summary; use --raw"
            };
            writeln!(rendered, "    series: {summary}").ok()?;
            continue;
        }
        writeln!(rendered, "    series:").ok()?;
        match metric.kind {
            DetailKind::Counter(series) | DetailKind::Gauge(series) => {
                for item in series {
                    render_detail_labels(&mut rendered, &item.labels)?;
                    writeln!(rendered, "        value: {}", item.value).ok()?;
                }
            }
            DetailKind::Histogram(series) => {
                for item in series {
                    render_detail_labels(&mut rendered, &item.labels)?;
                    writeln!(rendered, "        buckets:").ok()?;
                    for (bound, count) in item.buckets {
                        writeln!(rendered, "          <= {bound}: {count}").ok()?;
                    }
                    writeln!(rendered, "        sum: {}", item.sum).ok()?;
                    writeln!(rendered, "        count: {}", item.count).ok()?;
                }
            }
        }
    }
    Some(rendered)
}

fn is_default_metric(name: &str) -> bool {
    known_metric_label(name).is_some()
        || PROCESS_METRICS.contains(&name)
        || name == DROPPED_EVENTS_METRIC
}

fn render_detail_labels(rendered: &mut String, labels: &[(String, String)]) -> Option<()> {
    if labels.is_empty() {
        writeln!(rendered, "      - labels: none").ok()?;
        return Some(());
    }
    writeln!(rendered, "      - labels:").ok()?;
    for (key, value) in labels {
        writeln!(rendered, "          {key}: \"{}\"", value.escape_default()).ok()?;
    }
    Some(())
}

fn render_stats_raw(snapshot: &MetricsSnapshot) -> Option<String> {
    if snapshot.schema != 1 {
        return None;
    }
    let mut metrics: Vec<DetailMetric> = snapshot
        .metrics
        .iter()
        .map(parse_detail_metric)
        .collect::<Option<_>>()?;
    if metrics
        .iter()
        .any(|metric| known_metric_label(&metric.name).is_some())
        && render_default_stats_inner(snapshot, false, false).is_none()
    {
        return None;
    }
    metrics.sort_by(|left, right| left.name.cmp(&right.name));
    if metrics.windows(2).any(|pair| pair[0].name == pair[1].name) {
        return None;
    }

    let mut rendered = String::new();
    for metric in metrics {
        writeln!(rendered, "{}:", metric.name).ok()?;
        writeln!(rendered, "  type: {}", metric.kind.name()).ok()?;
        writeln!(rendered, "  help: {}", metric.help).ok()?;
        match metric.kind {
            DetailKind::Counter(series) | DetailKind::Gauge(series) => {
                for item in series {
                    writeln!(rendered, "  labels: {}", format_labels(&item.labels)).ok()?;
                    writeln!(rendered, "    value: {}", item.value).ok()?;
                }
            }
            DetailKind::Histogram(series) => {
                for item in series {
                    writeln!(rendered, "  labels: {}", format_labels(&item.labels)).ok()?;
                    write!(rendered, "    buckets:").ok()?;
                    for (bound, count) in item.buckets {
                        write!(rendered, " {} => {}", bound, count).ok()?;
                    }
                    rendered.push('\n');
                    writeln!(rendered, "    sum: {}", item.sum).ok()?;
                    writeln!(rendered, "    count: {}", item.count).ok()?;
                }
            }
        }
    }
    Some(rendered)
}

impl DetailKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Counter(_) => "counter",
            Self::Gauge(_) => "gauge",
            Self::Histogram(_) => "histogram",
        }
    }
}

fn parse_detail_metric(metric: &MetricFamily) -> Option<DetailMetric> {
    let name = metric.name().to_owned();
    let help = metric.help().to_owned();
    let kind = match metric.metric_type() {
        MetricType::Counter => DetailKind::Counter(parse_value_details(metric.value_series()?)?),
        MetricType::Gauge => DetailKind::Gauge(parse_value_details(metric.value_series()?)?),
        MetricType::Histogram => {
            DetailKind::Histogram(parse_histogram_details(metric.histogram_series()?)?)
        }
    };
    Some(DetailMetric { name, help, kind })
}

fn parse_value_details(series: &[limpid_metrics_schema::ValueSeries]) -> Option<Vec<ValueDetail>> {
    let mut parsed: Vec<ValueDetail> = series
        .iter()
        .map(|value| {
            Some(ValueDetail {
                labels: parse_labels(&value.labels),
                value: value.value,
            })
        })
        .collect::<Option<_>>()?;
    parsed.sort_by(|left, right| left.labels.cmp(&right.labels));
    if parsed
        .windows(2)
        .any(|pair| pair[0].labels == pair[1].labels)
    {
        return None;
    }
    Some(parsed)
}

fn parse_histogram_details(
    series: &[limpid_metrics_schema::HistogramSeries],
) -> Option<Vec<HistogramDetail>> {
    let mut parsed: Vec<HistogramDetail> = series
        .iter()
        .map(|value| {
            let buckets = value.buckets.clone();
            if buckets.iter().any(|(bound, _)| !bound.is_finite()) {
                return None;
            }
            if buckets.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                return None;
            }
            if buckets.windows(2).any(|pair| pair[0].1 > pair[1].1) {
                return None;
            }
            // Bucket counts and the total `count` are loaded from
            // independent atomics in the daemon, so observe ordering
            // permits a transient last-bucket count that exceeds the
            // total — do not reject on that inequality.
            let sum = value.sum;
            if !sum.is_finite() {
                return None;
            }
            Some(HistogramDetail {
                labels: parse_labels(&value.labels),
                buckets,
                sum,
                count: value.count,
            })
        })
        .collect::<Option<_>>()?;
    parsed.sort_by(|left, right| left.labels.cmp(&right.labels));
    if parsed
        .windows(2)
        .any(|pair| pair[0].labels == pair[1].labels)
    {
        return None;
    }
    Some(parsed)
}

fn parse_labels(value: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut labels: Vec<(String, String)> = value
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    labels.sort();
    labels
}

fn format_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return "{}".to_owned();
    }
    labels
        .iter()
        .map(|(key, value)| {
            let quoted = serde_json::to_string(value).expect("strings always serialize");
            format!("{}={}", key, quoted)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_list(json: &str) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => {
            print!("{}", json);
            return;
        }
    };

    let pipelines = match v.get("pipelines").and_then(|v| v.as_array()) {
        Some(p) => p,
        None => return,
    };

    for pipeline in pipelines {
        let name = pipeline.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        println!("{}:", name);

        if let Some(input) = pipeline.get("input").and_then(|v| v.as_str()) {
            println!("  input   {}", input);
        }

        if let Some(processes) = pipeline.get("processes").and_then(|v| v.as_array()) {
            for p in processes {
                if let Some(name) = p.as_str() {
                    println!("  process {}", name);
                }
            }
        }

        if let Some(outputs) = pipeline.get("outputs").and_then(|v| v.as_array()) {
            for o in outputs {
                if let Some(name) = o.as_str() {
                    println!("  output  {}", name);
                }
            }
        }

        println!();
    }
}

/// Parse a `--replay-timing` factor spec into a positive multiplier where
/// `1.0` means realtime, `10.0` means 10x faster, `0.2` means 5x slower.
fn parse_replay_factor(spec: &str) -> Result<f64, String> {
    let s = spec.trim();
    if s.eq_ignore_ascii_case("realtime") {
        return Ok(1.0);
    }
    // Strip a trailing `x` or `X` if present; either form is accepted.
    let num_str = s.strip_suffix(|c: char| c == 'x' || c == 'X').unwrap_or(s);
    let v: f64 = num_str.parse().map_err(|_| {
        format!(
            "expected `realtime` or a positive `<float>x` (got {:?})",
            spec
        )
    })?;
    if !v.is_finite() || v <= 0.0 {
        return Err(format!(
            "factor must be a finite positive number (got {:?})",
            spec
        ));
    }
    Ok(v)
}

/// Pull the top-level `received_at` field out of an Event JSON line
/// and parse it as i64 unix nanoseconds — matches `Event::to_json_value`
/// and OTLP `time_unix_nano`. Returns a clear error so callers can abort
/// — silently skipping would violate the "zero hidden behavior"
/// principle.
fn extract_timestamp(line: &str) -> Result<DateTime<Utc>, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("not valid JSON: {}", e))?;
    let nanos = v
        .get("received_at")
        .ok_or_else(|| "event has no top-level `received_at` field".to_string())?
        .as_i64()
        .ok_or_else(|| {
            "`received_at` is not an i64 (expected unix nanoseconds, matching \
             `Event::to_json_value` / OTLP `time_unix_nano`)"
                .to_string()
        })?;
    Ok(DateTime::<Utc>::from_timestamp_nanos(nanos))
}

/// Tracks the wall-clock anchor used to gate replay sleeps.
struct ReplayState {
    factor: f64,
    /// Wall-clock instant + event-time anchor of the first event we saw.
    anchor: Option<(Instant, DateTime<Utc>)>,
    /// Last event timestamp we processed; used to detect non-monotonic input.
    last_event_ts: Option<DateTime<Utc>>,
    /// Whether we've already emitted a catch-up warning (avoid per-event spam).
    catchup_warned: bool,
}

impl ReplayState {
    fn new(factor: f64) -> Self {
        Self {
            factor,
            anchor: None,
            last_event_ts: None,
            catchup_warned: false,
        }
    }

    /// Sleep until the wall-clock instant at which `event_ts` should be sent,
    /// based on the first event's timestamp and the speed factor. The first
    /// call sets the anchor and returns immediately.
    fn wait_for(&mut self, event_ts: DateTime<Utc>) {
        // Warn on out-of-order timestamps but flush through with no delay —
        // we don't reorder; the input JSONL's order wins.
        if let Some(last) = self.last_event_ts
            && event_ts < last
        {
            eprintln!(
                "warning: --replay-timing: event timestamp went backwards ({} < {}); flushing immediately",
                event_ts.to_rfc3339(),
                last.to_rfc3339()
            );
            self.last_event_ts = Some(event_ts);
            return;
        }
        self.last_event_ts = Some(event_ts);

        let (anchor_wall, anchor_event) = match self.anchor {
            Some(a) => a,
            None => {
                // First event becomes the anchor; send it immediately.
                self.anchor = Some((Instant::now(), event_ts));
                return;
            }
        };

        // Event-time delta since the anchor, scaled by speed factor.
        let event_delta = event_ts.signed_duration_since(anchor_event);
        let event_delta_secs = event_delta
            .num_microseconds()
            .map(|us| us as f64 / 1_000_000.0)
            // Fallback for huge gaps that overflow microsecond range.
            .unwrap_or_else(|| event_delta.num_milliseconds() as f64 / 1_000.0);
        let scaled_secs = event_delta_secs / self.factor;
        if !scaled_secs.is_finite() || scaled_secs <= 0.0 {
            return;
        }
        let target = anchor_wall + Duration::from_secs_f64(scaled_secs);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        } else if !self.catchup_warned {
            // We're already behind schedule on the very first lag — warn once
            // so the user knows replay isn't keeping up with the requested rate.
            let lag = now - target;
            eprintln!(
                "warning: --replay-timing: behind schedule by {:.3}s; replay will catch up by sending events without delay",
                lag.as_secs_f64()
            );
            self.catchup_warned = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_writes_a_signing_key_and_returns_matching_rfc8410_spki() {
        use ring::signature::KeyPair as _;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.pem");
        let public_key = generate_node_key(&path).unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        let document = pem::parse(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document.tag(), "PRIVATE KEY");
        let key_pair = ring::signature::Ed25519KeyPair::from_pkcs8(document.contents()).unwrap();
        let message = b"limpid ltp node identity";
        let signature = key_pair.sign(message);
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            key_pair.public_key().as_ref(),
        )
        .verify(message, signature.as_ref())
        .unwrap();

        let spki = base64::engine::general_purpose::STANDARD
            .decode(public_key)
            .unwrap();
        assert_eq!(&spki[..ED25519_SPKI_PREFIX.len()], &ED25519_SPKI_PREFIX);
        assert_eq!(
            &spki[ED25519_SPKI_PREFIX.len()..],
            key_pair.public_key().as_ref()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn keygen_never_overwrites_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.pem");
        std::fs::write(&path, b"existing material").unwrap();

        let error = generate_node_key(&path).unwrap_err();
        assert!(error.contains("cannot publish node key"));
        assert_eq!(std::fs::read(&path).unwrap(), b"existing material");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn keygen_cleans_temporary_files_after_write_and_file_sync_failures() {
        let dir = tempfile::tempdir().unwrap();
        let private_material = b"DO-NOT-LEAK-PRIVATE-MATERIAL";
        let write_path = dir.path().join("write-failed.pem");
        let write_error = write_new_private_key_with(
            &write_path,
            private_material,
            |_file, _contents| Err(std::io::Error::other("injected write failure")),
            || Ok(()),
            |_parent| Ok(()),
        )
        .unwrap_err();
        assert!(write_error.contains("injected write failure"));
        assert!(!write_error.contains(std::str::from_utf8(private_material).unwrap()));
        assert!(!write_path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let sync_path = dir.path().join("sync-failed.pem");
        let sync_error = write_new_private_key_with(
            &sync_path,
            private_material,
            |file, contents| {
                file.write_all(contents)?;
                Err(std::io::Error::other("injected file sync failure"))
            },
            || Ok(()),
            |_parent| Ok(()),
        )
        .unwrap_err();
        assert!(sync_error.contains("injected file sync failure"));
        assert!(!sync_error.contains(std::str::from_utf8(private_material).unwrap()));
        assert!(!sync_path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn keygen_publish_never_replaces_a_competing_final_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("competing.pem");
        let private_material = b"new private material";
        let preserved = b"competing path contents";

        let error = write_new_private_key_with(
            &path,
            private_material,
            |file, contents| {
                file.write_all(contents)?;
                file.sync_all()
            },
            || std::fs::write(&path, preserved),
            |_parent| Ok(()),
        )
        .unwrap_err();

        assert!(error.contains("cannot publish node key"));
        assert_eq!(std::fs::read(&path).unwrap(), preserved);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn keygen_parent_sync_failure_preserves_the_published_key() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("published.pem");
        let private_material = b"complete private material";

        let error = write_new_private_key_with(
            &path,
            private_material,
            |file, contents| {
                file.write_all(contents)?;
                file.sync_all()
            },
            || Ok(()),
            |_parent| Err(std::io::Error::other("injected parent sync failure")),
        )
        .unwrap_err();

        assert!(error.contains("published but parent directory sync failed"));
        assert_eq!(std::fs::read(&path).unwrap(), private_material);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn keygen_orders_file_sync_publish_and_parent_sync() {
        use std::cell::RefCell;
        use std::os::unix::fs::PermissionsExt;
        use std::rc::Rc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordered.pem");
        let private_material = b"ordered private material";
        let events = Rc::new(RefCell::new(Vec::new()));
        let write_events = Rc::clone(&events);
        let publish_events = Rc::clone(&events);
        let parent_events = Rc::clone(&events);

        write_new_private_key_with(
            &path,
            private_material,
            |file, contents| {
                assert!(!path.exists());
                assert_eq!(file.metadata()?.permissions().mode() & 0o7777, 0o600);
                file.write_all(contents)?;
                file.sync_all()?;
                write_events.borrow_mut().push("file-sync");
                Ok(())
            },
            || {
                assert!(!path.exists());
                publish_events.borrow_mut().push("before-publish");
                Ok(())
            },
            |_parent| {
                assert_eq!(std::fs::read(&path)?, private_material);
                parent_events.borrow_mut().push("parent-sync");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            events.borrow().as_slice(),
            ["file-sync", "before-publish", "parent-sync"]
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn canonical_shared_snapshot(extra_family: bool) -> limpid_metrics_schema::MetricsSnapshot {
        let mut metrics = PIPELINE_METRICS
            .iter()
            .map(|name| {
                let labels = if *name == DROPPED_EVENTS_METRIC {
                    serde_json::json!({
                        "pipeline": "route",
                        "step": "0",
                        "process_path": "/",
                        "process_name": ""
                    })
                } else {
                    serde_json::json!({"pipeline": "route"})
                };
                serde_json::json!({
                    "name": name,
                    "type": "counter",
                    "help": "Pipeline counter.",
                    "series": [{"labels": labels, "value": 1}]
                })
            })
            .chain(INPUT_METRICS.iter().map(|name| {
                serde_json::json!({
                    "name": name,
                    "type": "counter",
                    "help": "Input counter.",
                    "series": [{"labels": {"input": "ingress"}, "value": 2}]
                })
            }))
            .chain(OUTPUT_METRICS.iter().map(|name| {
                serde_json::json!({
                    "name": name,
                    "type": "counter",
                    "help": "Output counter.",
                    "series": [{"labels": {"output": "egress"}, "value": 3}]
                })
            }))
            .collect::<Vec<_>>();
        if extra_family {
            metrics.push(serde_json::json!({
                "name": "future_metric",
                "type": "gauge",
                "help": "Future metric.",
                "series": [{"labels": {"scope": "future"}, "value": 9}]
            }));
        }
        serde_json::from_value(serde_json::json!({"schema": 1, "metrics": metrics})).unwrap()
    }

    fn process_tree_snapshot() -> limpid_metrics_schema::MetricsSnapshot {
        let mut payload = serde_json::to_value(canonical_shared_snapshot(false)).unwrap();
        let identities = [
            ("compact", 2, "/root_one", "root_one"),
            ("compact", 3, "/root_two", "root_two"),
            ("compact", 4, "/root_two/child_a", "child_a"),
            (
                "compact",
                5,
                "/root_two/child_a/parse_paloalto_cef_normalize_severity",
                "parse_paloalto_cef_normalize_severity",
            ),
            ("compact", 6, "/root_two/child_b", "child_b"),
            ("compact", 7, "/root_two/child_b/leaf", "leaf"),
            ("compact", 8, "/duplicate_root", "duplicate_root"),
            ("compact", 9, "/duplicate_root", "duplicate_root"),
            ("full", 1, "/only_root", "only_root"),
        ];
        let process_series = || {
            identities
                .iter()
                .map(|(pipeline, step, process_path, process_name)| {
                    serde_json::json!({
                        "labels": {
                            "pipeline": pipeline,
                            "step": step.to_string(),
                            "process_path": process_path,
                            "process_name": process_name,
                        },
                        "value": step,
                    })
                })
                .collect::<Vec<_>>()
        };
        for name in [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
            "limpid_process_events_errored_total",
        ] {
            payload["metrics"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "name": name,
                    "type": "counter",
                    "help": "Process invocation fixture.",
                    "series": process_series(),
                }));
        }
        payload["metrics"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|family| family["name"] == DROPPED_EVENTS_METRIC)
            .unwrap()["series"]
            .as_array_mut()
            .unwrap()
            .extend(process_series());
        serde_json::from_value(payload).unwrap()
    }

    #[test]
    fn process_tree_names_render_siblings_and_ancestor_continuations() {
        let identities = [
            (1, "/first", "first"),
            (2, "/second", "second"),
            (3, "/second/child_a", "child_a"),
            (4, "/second/child_a/leaf", "leaf"),
            (5, "/second/child_b", "child_b"),
            (6, "/third", "third"),
        ]
        .map(|(step, process_path, process_name)| ProcessIdentity {
            pipeline: "pipeline".to_owned(),
            step,
            process_path: process_path.to_owned(),
            process_name: process_name.to_owned(),
        });
        assert_eq!(
            process_tree_names(&identities.iter().collect::<Vec<_>>()),
            Some(vec![
                ("├─ first".to_owned(), "│  ".to_owned()),
                ("├─ second".to_owned(), "│  ".to_owned()),
                ("│  ├─ child_a".to_owned(), "│  │  ".to_owned()),
                ("│  │  └─ leaf".to_owned(), "│  │     ".to_owned()),
                ("│  └─ child_b".to_owned(), "│     ".to_owned()),
                ("└─ third".to_owned(), "   ".to_owned()),
            ])
        );
    }

    #[test]
    fn process_tree_names_reject_false_parent_topologies() {
        let identities = |paths: &[&str]| {
            paths
                .iter()
                .enumerate()
                .map(|(index, path)| ProcessIdentity {
                    pipeline: "pipeline".to_owned(),
                    step: index + 1,
                    process_path: (*path).to_owned(),
                    process_name: path.rsplit('/').next().unwrap().to_owned(),
                })
                .collect::<Vec<_>>()
        };

        for paths in [
            &["/a", "/b/c"][..],
            &["/a", "/a/b/c"][..],
            &["/a", "/a/b", "/a/c/d"][..],
        ] {
            let identities = identities(paths);
            assert!(process_tree_names(&identities.iter().collect::<Vec<_>>()).is_none());
        }
    }

    #[test]
    fn false_process_topology_preserves_mode_specific_fallbacks() {
        let mut fixture = serde_json::to_value(process_tree_snapshot()).unwrap();
        for family in fixture["metrics"].as_array_mut().unwrap() {
            let Some(series) = family["series"].as_array_mut() else {
                continue;
            };
            for item in series {
                if item["labels"]["process_path"] == "/root_two/child_a" {
                    item["labels"]["process_path"] = "/missing/child_a".into();
                }
            }
        }
        let snapshot = serde_json::from_value(fixture).unwrap();

        assert!(render_default_stats(&snapshot).is_none());
        let details = render_stats_details(&snapshot).unwrap();
        assert!(!details.contains("\nProcesses:\n"));
        assert!(details.contains("series: unavailable as a process summary; use --raw"));
    }

    #[test]
    fn process_tree_is_byte_exact_grouped_and_dynamically_aligned() {
        let rendered = render_default_stats(&process_tree_snapshot()).unwrap();
        let processes = rendered
            .split_once("Processes:\n")
            .map(|(_, processes)| processes)
            .expect("process section");
        let expected = format!(
            concat!(
                "  compact\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
                "  full\n",
                "  {:<46} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored\n",
            ),
            "├─ root_one",
            2,
            2,
            2,
            2,
            "├─ root_two",
            3,
            3,
            3,
            3,
            "│  ├─ child_a",
            4,
            4,
            4,
            4,
            "│  │  └─ parse_paloalto_cef_normalize_severity",
            5,
            5,
            5,
            5,
            "│  └─ child_b",
            6,
            6,
            6,
            6,
            "│     └─ leaf",
            7,
            7,
            7,
            7,
            "├─ duplicate_root",
            8,
            8,
            8,
            8,
            "└─ duplicate_root",
            9,
            9,
            9,
            9,
            "└─ only_root",
            1,
            1,
            1,
            1,
        );
        assert_eq!(processes, expected);
        assert!(!processes.contains('\t'));
    }

    #[test]
    fn default_stats_renders_the_shared_snapshot_and_ignores_unknown_families() {
        let shared = canonical_shared_snapshot(true);
        let render: fn(&limpid_metrics_schema::MetricsSnapshot) -> Option<String> =
            render_default_stats;
        let rendered = render(&shared).unwrap();
        assert!(rendered.contains("Pipelines:"));
        assert!(rendered.contains("route"));
        assert!(rendered.contains("Inputs:"));
        assert!(rendered.contains("ingress"));
        assert!(rendered.contains("Outputs:"));
        assert!(rendered.contains("egress"));
        assert!(!rendered.contains("future_metric"));
    }

    #[test]
    fn default_stats_rejects_a_semantically_ambiguous_shared_snapshot() {
        let mut fixture = serde_json::to_value(canonical_shared_snapshot(false)).unwrap();
        let duplicate = fixture["metrics"][0]["series"][0].clone();
        fixture["metrics"][0]["series"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        let shared: limpid_metrics_schema::MetricsSnapshot =
            serde_json::from_value(fixture).unwrap();
        let render: fn(&limpid_metrics_schema::MetricsSnapshot) -> Option<String> =
            render_default_stats;
        assert!(render(&shared).is_none());
    }

    #[test]
    fn details_render_all_shared_wire_family_types() {
        let fixture = include_str!("../../limpid-metrics-schema/tests/fixtures/schema-v1.json");
        let shared: limpid_metrics_schema::MetricsSnapshot = serde_json::from_str(fixture).unwrap();
        let render: fn(&limpid_metrics_schema::MetricsSnapshot) -> Option<String> =
            render_stats_details;
        let rendered = render(&shared).unwrap();
        assert!(rendered.contains("requests_total"));
        assert!(rendered.contains("queue_depth"));
        assert!(rendered.contains("latency_seconds"));
    }

    #[test]
    fn valid_names_match_daemon_ident_grammar() {
        // Mirrors `ident = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_")*`
        // in the daemon's dsl/limpid.pest.
        assert!(is_valid_name("firewall"));
        assert!(is_valid_name("fw_01"));
        assert!(is_valid_name("_internal"));
        assert!(is_valid_name("A"));
    }

    #[test]
    fn invalid_names_rejected_before_send() {
        // Empty / leading digit — not idents.
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("1fw"));
        // Whitespace and newlines could smuggle extra tokens or extra
        // protocol lines into the line-based control command.
        assert!(!is_valid_name("fw json"));
        assert!(!is_valid_name("fw\ninject output x"));
        assert!(!is_valid_name("fw\t"));
        // Control characters and non-ASCII.
        assert!(!is_valid_name("fw\x07"));
        assert!(!is_valid_name("fw-01")); // dash is not in the grammar
        assert!(!is_valid_name("ファイアウォール"));
    }

    #[test]
    fn daemon_error_lines_detected() {
        assert_eq!(
            daemon_error_line("error: unknown tap point 'x'\n"),
            Some(" unknown tap point 'x'")
        );
        assert_eq!(
            daemon_error_line("error: control socket busy (too many concurrent connections)"),
            Some(" control socket busy (too many concurrent connections)")
        );
        // JSON responses never match.
        assert_eq!(daemon_error_line(r#"{"status":"ok"}"#), None);
        assert_eq!(daemon_error_line(""), None);
    }

    #[test]
    fn health_ok_requires_explicit_ok_status() {
        assert!(health_is_ok(r#"{"status":"ok","uptime_seconds":5}"#));
        // Anything else is unhealthy: wrong status, missing field,
        // wrong type, unparseable, empty.
        assert!(!health_is_ok(r#"{"status":"degraded"}"#));
        assert!(!health_is_ok(r#"{"uptime_seconds":5}"#));
        assert!(!health_is_ok(r#"{"status":1}"#));
        assert!(!health_is_ok("not json"));
        assert!(!health_is_ok(""));
    }

    #[test]
    fn parse_factor_accepts_realtime_aliases() {
        assert_eq!(parse_replay_factor("realtime").unwrap(), 1.0);
        assert_eq!(parse_replay_factor("REALTIME").unwrap(), 1.0);
        assert_eq!(parse_replay_factor("1x").unwrap(), 1.0);
        assert_eq!(parse_replay_factor("1X").unwrap(), 1.0);
        assert_eq!(parse_replay_factor("1").unwrap(), 1.0);
    }

    #[test]
    fn parse_factor_accepts_fractional_and_large() {
        assert!((parse_replay_factor("10x").unwrap() - 10.0).abs() < 1e-9);
        assert!((parse_replay_factor("0.2x").unwrap() - 0.2).abs() < 1e-9);
        assert!((parse_replay_factor("0.5").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn parse_factor_rejects_invalid() {
        assert!(parse_replay_factor("").is_err());
        assert!(parse_replay_factor("fast").is_err());
        assert!(parse_replay_factor("0x").is_err());
        assert!(parse_replay_factor("-1x").is_err());
        assert!(parse_replay_factor("nanx").is_err());
        assert!(parse_replay_factor("infx").is_err());
    }

    #[test]
    fn extract_timestamp_reads_i64_nanos_field() {
        // 2024-01-02T03:04:05.123456789Z in unix nanoseconds.
        let nanos: i64 = 1_704_164_645_123_456_789;
        let line = format!(
            r#"{{"received_at":{},"source":{{"ip":"127.0.0.1","port":514}},"ingress":"hi","egress":"hi"}}"#,
            nanos
        );
        let ts = extract_timestamp(&line).unwrap();
        assert_eq!(ts.timestamp_nanos_opt().unwrap(), nanos);
    }

    #[test]
    fn extract_timestamp_rejects_missing_or_malformed() {
        // Missing field
        let line = r#"{"ingress":"hi","source":{"ip":"127.0.0.1","port":514},"egress":"hi"}"#;
        assert!(extract_timestamp(line).is_err());
        // Wrong type: string (the old wire form; no longer accepted —
        // mirrors `Event::from_json`, which is also i64-only).
        let line = r#"{"received_at":"2024-01-02T03:04:05Z"}"#;
        assert!(extract_timestamp(line).is_err());
        // Wrong type: JSON number that doesn't fit i64 (float). Guards
        // against a silent truncation if someone wires up an f64 producer.
        let line = r#"{"received_at":1.5}"#;
        assert!(extract_timestamp(line).is_err());
        // Not JSON
        assert!(extract_timestamp("not json at all").is_err());
    }

    #[test]
    fn extract_timestamp_matches_canonical_event_json_shape() {
        // Fixture mirrors the full canonical shape emitted by
        // `OwnedEvent::to_json_value` (see `crates/limpid/src/event.rs`):
        // top-level `received_at` is i64 unix nanoseconds (OTLP
        // `time_unix_nano` parity), `source` is the v0.5.6+ object form
        // `{ip, port}`, and `ingress` / `egress` are plain JSON strings
        // (`bytes_to_json` emits the `$bytes_b64` marker only for
        // non-UTF-8 payloads). This is the regression guard for the
        // `tap --json | inject --json --replay-timing` round-trip.
        // The cross-crate contract itself is covered by
        // `crates/limpid/src/event.rs::from_json_round_trips_received_at_nanos`.
        let nanos: i64 = 1_704_164_645_123_456_789;
        let line = format!(
            r#"{{"received_at":{},"source":{{"ip":"127.0.0.1","port":514}},"ingress":"hi","egress":"hi"}}"#,
            nanos
        );
        let ts = extract_timestamp(&line).unwrap();
        assert_eq!(ts.timestamp_nanos_opt().unwrap(), nanos);
    }

    #[test]
    fn replay_state_first_event_is_immediate() {
        let mut s = ReplayState::new(1.0);
        let t0 = Utc::now();
        let start = Instant::now();
        s.wait_for(t0);
        // First event sets the anchor; should return well under 50ms.
        assert!(start.elapsed() < Duration::from_millis(50));
        assert!(s.anchor.is_some());
    }

    #[test]
    fn replay_state_scales_delta_by_factor() {
        // 10x speed: a 1-second event-time gap should sleep ~100ms.
        let mut s = ReplayState::new(10.0);
        let t0: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let t1: DateTime<Utc> = "2024-01-01T00:00:01Z".parse().unwrap();
        s.wait_for(t0);
        let start = Instant::now();
        s.wait_for(t1);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80) && elapsed < Duration::from_millis(300),
            "expected ~100ms sleep at 10x, got {:?}",
            elapsed
        );
    }

    #[test]
    fn replay_state_backwards_timestamp_does_not_sleep() {
        let mut s = ReplayState::new(1.0);
        let t0: DateTime<Utc> = "2024-01-01T00:00:10Z".parse().unwrap();
        let t_back: DateTime<Utc> = "2024-01-01T00:00:05Z".parse().unwrap();
        s.wait_for(t0);
        let start = Instant::now();
        s.wait_for(t_back);
        // Should flush immediately with a warning to stderr.
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn replay_state_catch_up_no_sleep_when_behind() {
        // factor=1000x makes the schedule effectively instantaneous so
        // by the time we hand it the next event we're already "behind."
        let mut s = ReplayState::new(1000.0);
        let t0: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let t1: DateTime<Utc> = "2024-01-01T00:00:00.000001Z".parse().unwrap();
        s.wait_for(t0);
        std::thread::sleep(Duration::from_millis(10));
        let start = Instant::now();
        s.wait_for(t1);
        assert!(start.elapsed() < Duration::from_millis(20));
    }
}

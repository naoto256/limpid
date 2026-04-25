//! limpidctl: control and debug CLI for limpid.
//!
//! Usage:
//!   limpidctl tap input <name> [--json]     Stream events from a named input
//!   limpidctl tap process <name> [--json]   Stream events after a named process
//!   limpidctl tap output <name> [--json]    Stream events from a named output
//!   limpidctl inject input <name> [--json]  Inject stdin lines into a named input
//!   limpidctl inject output <name> [--json] Inject stdin lines into a named output queue
//!   limpidctl list [--json]                 List pipelines and tap points
//!   limpidctl stats [--json]                Show pipeline/output metrics
//!   limpidctl health [--json]               Check daemon health
//!
//! Connects to limpid's control socket (default: /var/run/limpid/control.sock).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

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
        /// Output raw JSON instead of formatted text
        #[arg(long)]
        json: bool,
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
}

#[derive(Subcommand)]
enum InjectKind {
    /// Push raw lines or full-Event JSON into a named input's channel
    Input {
        name: String,
        /// Each stdin line is a full Event JSON (as emitted by `tap --json`)
        #[arg(long)]
        json: bool,
        /// Replay events at their original timing using each event's `timestamp` field.
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
        /// Replay events at their original timing using each event's `timestamp` field.
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
            let command = if json {
                format!("tap {} {} json", kind_str, name)
            } else {
                format!("tap {} {}", kind_str, name)
            };
            run_tap(&cli.socket, &command);
        }
        Command::List { json } => {
            let response = query_command(&cli.socket, "list");
            if json {
                print!("{}", response);
            } else {
                format_list(&response);
            }
        }
        Command::Stats { json } => {
            let response = query_command(&cli.socket, "stats");
            if json {
                print!("{}", response);
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
            if json {
                print!("{}", response);
            } else {
                format_health(&response);
            }
        }
    }
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

fn format_stats(json: &str) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => {
            print!("{}", json);
            return;
        }
    };

    let get = |m: &serde_json::Value, k: &str| m.get(k).and_then(|v| v.as_u64()).unwrap_or(0);

    // Pipelines first — the main concept.
    if let Some(pipelines) = v.get("pipelines").and_then(|v| v.as_object()) {
        let mut names: Vec<&String> = pipelines.keys().collect();
        names.sort();
        if !names.is_empty() {
            println!("Pipelines:");
            for name in &names {
                let m = &pipelines[*name];
                println!(
                    "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded",
                    name,
                    get(m, "events_received"),
                    get(m, "events_finished"),
                    get(m, "events_dropped"),
                    get(m, "events_discarded"),
                );
            }
        }
    }

    if let Some(inputs) = v.get("inputs").and_then(|v| v.as_object()) {
        let mut names: Vec<&String> = inputs.keys().collect();
        names.sort();
        if !names.is_empty() {
            println!("\nInputs:");
            for name in &names {
                let m = &inputs[*name];
                println!(
                    "  {:<24} {:>8} received  {:>8} invalid  {:>8} injected",
                    name,
                    get(m, "events_received"),
                    get(m, "events_invalid"),
                    get(m, "events_injected"),
                );
            }
        }
    }

    if let Some(outputs) = v.get("outputs").and_then(|v| v.as_object()) {
        let mut names: Vec<&String> = outputs.keys().collect();
        names.sort();
        if !names.is_empty() {
            println!("\nOutputs:");
            for name in &names {
                let m = &outputs[*name];
                println!(
                    "  {:<24} {:>8} received  {:>8} injected  {:>8} written  {:>8} failed  {:>8} retries",
                    name,
                    get(m, "events_received"),
                    get(m, "events_injected"),
                    get(m, "events_written"),
                    get(m, "events_failed"),
                    get(m, "retries"),
                );
            }
        }
    }
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
/// and parse it as RFC3339. Returns a clear error so callers can abort
/// — silently skipping would violate the "zero hidden behavior"
/// principle.
fn extract_timestamp(line: &str) -> Result<DateTime<Utc>, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("not valid JSON: {}", e))?;
    let ts = v
        .get("received_at")
        .ok_or_else(|| "event has no top-level `received_at` field".to_string())?
        .as_str()
        .ok_or_else(|| "`received_at` field is not a string".to_string())?;
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("`received_at` is not RFC3339 ({}): {:?}", e, ts))
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
    fn extract_timestamp_reads_rfc3339_field() {
        let line = r#"{"received_at":"2024-01-02T03:04:05Z","ingress":"hi","source":"127.0.0.1:514","egress":"hi"}"#;
        let ts = extract_timestamp(line).unwrap();
        assert_eq!(ts.to_rfc3339(), "2024-01-02T03:04:05+00:00");
    }

    #[test]
    fn extract_timestamp_rejects_missing_or_malformed() {
        // Missing field
        let line = r#"{"ingress":"hi","source":"127.0.0.1:514","egress":"hi"}"#;
        assert!(extract_timestamp(line).is_err());
        // Wrong type
        let line = r#"{"received_at":1234,"ingress":"hi"}"#;
        assert!(extract_timestamp(line).is_err());
        // Bad format
        let line = r#"{"received_at":"yesterday"}"#;
        assert!(extract_timestamp(line).is_err());
        // Not JSON
        assert!(extract_timestamp("not json at all").is_err());
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

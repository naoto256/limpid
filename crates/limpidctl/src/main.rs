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
    },
    /// Push raw lines or full-Event JSON directly into a named output's queue
    Output {
        name: String,
        /// Each stdin line is a full Event JSON (as emitted by `tap --json`)
        #[arg(long)]
        json: bool,
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
            let (kind_str, name, json) = match kind {
                InjectKind::Input { name, json } => ("input", name, json),
                InjectKind::Output { name, json } => ("output", name, json),
            };
            let command = if json {
                format!("inject {} {} json", kind_str, name)
            } else {
                format!("inject {} {}", kind_str, name)
            };
            run_inject(&cli.socket, &command);
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

fn run_inject(socket: &PathBuf, command: &str) {
    let mut stream = connect(socket);
    if let Err(e) = writeln!(stream, "{}", command) {
        eprintln!("Failed to send command: {}", e);
        std::process::exit(1);
    }

    // Copy stdin line-by-line to the socket.
    let stdin = std::io::stdin();
    let stdin_lock = stdin.lock();
    let stdin_reader = BufReader::new(stdin_lock);
    for line in stdin_reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to read stdin: {}", e);
                std::process::exit(1);
            }
        };
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
        Err(_) => { print!("{}", json); return; }
    };

    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("unknown");
    let uptime = v.get("uptime_seconds").and_then(|u| u.as_u64()).unwrap_or(0);
    println!("{} (uptime: {})", status.to_uppercase(), format_duration(uptime));
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
        Err(_) => { print!("{}", json); return; }
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
        Err(_) => { print!("{}", json); return; }
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

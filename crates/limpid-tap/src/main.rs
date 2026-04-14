//! limpid-tap: debug tool for tapping into limpid event streams.
//!
//! Usage:
//!   limpid-tap input <name>      Stream events from a named input
//!   limpid-tap process <name>    Stream events after a named process
//!   limpid-tap output <name>     Stream events from a named output
//!   limpid-tap --list            List pipelines and tap points
//!   limpid-tap --stats           Show pipeline/output metrics
//!   limpid-tap --health          Check daemon health
//!
//! Connects to limpid's control socket (default: /var/run/limpid/control.sock)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use clap::Parser;

const DEFAULT_SOCKET: &str = "/var/run/limpid/control.sock";

#[derive(Parser)]
#[command(name = "limpid-tap", about = "Debug tool for tapping limpid event streams")]
struct Cli {
    /// Tap target: input/process/output followed by name
    target: Vec<String>,

    /// List pipelines and tap points
    #[arg(long)]
    list: bool,

    /// Show pipeline/output metrics
    #[arg(long)]
    stats: bool,

    /// Check daemon health
    #[arg(long)]
    health: bool,

    /// Output raw JSON instead of formatted text
    #[arg(long)]
    json: bool,

    /// Control socket path
    #[arg(long, default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    let command = if cli.health {
        "health".to_string()
    } else if cli.list {
        "list".to_string()
    } else if cli.stats {
        "stats".to_string()
    } else if cli.target.len() == 2 {
        let kind = &cli.target[0];
        let name = &cli.target[1];
        match kind.as_str() {
            "input" | "process" | "output" => format!("tap {} {}", kind, name),
            _ => {
                eprintln!("Unknown tap type '{}'. Use: input, process, or output", kind);
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("Usage: limpid-tap <input|process|output> <name>");
        eprintln!("       limpid-tap --list | --stats | --health");
        std::process::exit(1);
    };

    // Tap commands stream indefinitely — don't use query_command
    let is_tap = command.starts_with("tap ");
    if is_tap {
        let mut stream = connect(&cli.socket);
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
    } else {
        let response = query_command(&cli.socket, &command);

        if cli.json {
            println!("{}", response);
        } else if cli.health {
            format_health(&response);
        } else if cli.stats {
            format_stats(&response);
        } else if cli.list {
            format_list(&response);
        }
    }
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

    if let Some(inputs) = v.get("inputs").and_then(|v| v.as_object()) {
        let mut names: Vec<&String> = inputs.keys().collect();
        names.sort();
        if !names.is_empty() {
            println!("Inputs:");
            for name in &names {
                let m = &inputs[*name];
                println!(
                    "  {:<24} {:>8} received  {:>8} invalid",
                    name,
                    m.get("events_received").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("events_invalid").and_then(|v| v.as_u64()).unwrap_or(0),
                );
            }
        }
    }

    if let Some(pipelines) = v.get("pipelines").and_then(|v| v.as_object()) {
        let mut names: Vec<&String> = pipelines.keys().collect();
        names.sort();
        if !names.is_empty() {
            println!("\nPipelines:");
            for name in &names {
                let m = &pipelines[*name];
                println!(
                    "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded",
                    name,
                    m.get("events_received").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("events_finished").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("events_dropped").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("events_discarded").and_then(|v| v.as_u64()).unwrap_or(0),
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
                    "  {:<24} {:>8} written  {:>8} failed  {:>8} retries",
                    name,
                    m.get("events_written").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("events_failed").and_then(|v| v.as_u64()).unwrap_or(0),
                    m.get("retries").and_then(|v| v.as_u64()).unwrap_or(0),
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

//! limpid: log pipelines, limpid as intent.
//!
//! Modes:
//!   limpid --config <file>            — run as daemon
//!   limpid --check --config <file>   — validate configuration and exit
//!   limpid --test <pipeline> [--input ..] — test a pipeline with sample data
//!   limpid --debug                    — enable per-process trace logging

mod check;
mod config;
mod control;
mod dsl;
mod event;
mod functions;
mod metrics;
mod modules;
mod pipeline;
mod queue;
mod runtime;
mod signal;
mod tap;
mod tls;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;

use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::pipeline::{CompiledConfig, run_pipeline};

#[derive(Parser)]
#[command(name = "limpid", about = "Log pipelines, limpid as intent.")]
struct Cli {
    /// Configuration file
    #[arg(long, default_value = "/etc/limpid/limpid.conf")]
    config: String,

    /// Check configuration and exit
    #[arg(long)]
    check: bool,

    /// Treat analyzer warnings as errors. Implies `--check`. With this
    /// flag set, any warning bumps the exit code to 2; without it,
    /// warnings are reported but only errors fail the check.
    #[arg(long)]
    strict_warnings: bool,

    /// Test a pipeline with sample input
    #[arg(long)]
    test_pipeline: Option<String>,

    /// Sample input for test mode (JSON)
    #[arg(long)]
    input: Option<String>,

    /// Enable debug trace logging
    #[arg(long)]
    debug: bool,
}

fn main() -> Result<()> {
    // Restore the default SIGPIPE disposition for the CLI-style modes
    // (`--check`, `--test-pipeline`) which write to stdout and may be
    // piped through `head`/`less`. Daemon mode doesn't write structured
    // stdout, so this is a no-op there. Without this, the Rust default
    // (`SIG_IGN`) turns a closed downstream pipe into an `EPIPE` that
    // the println! infrastructure escalates into a panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    // Initialize tracing
    if cli.debug {
        tracing_subscriber::fmt()
            .with_env_filter("limpid=trace")
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter("limpid=info")
            .init();
    }

    if cli.check || cli.strict_warnings {
        return run_check(&cli.config, cli.strict_warnings);
    }

    if let Some(ref pipeline_name) = cli.test_pipeline {
        return run_test(&cli.config, pipeline_name, cli.input.as_deref());
    }

    // Daemon mode
    run_daemon(&cli.config)
}

/// Daemon mode: start the tokio runtime and run the log pipeline.
fn run_daemon(config_path: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let config_file = Path::new(config_path).to_path_buf();
        let config = config::load_config(&config_file).context("configuration error")?;
        let compiled = CompiledConfig::from_config(config)?;

        let mut runtime = runtime::Runtime::start(compiled, config_file).await?;

        // Wait for signals
        loop {
            match signal::wait_for_signal().await? {
                signal::SignalAction::Shutdown => {
                    runtime.shutdown().await;
                    break;
                }
                signal::SignalAction::Reload => {
                    let file = runtime.config_file().to_path_buf();

                    // Phase 1: Snapshot current running config (in-memory, not from disk)
                    let old_config = runtime.compiled_config();

                    // Phase 2: Load and validate new config from disk
                    let new_compiled =
                        match config::load_config(&file).and_then(CompiledConfig::from_config) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::error!(
                                    "reload: invalid configuration: {} — keeping current",
                                    e
                                );
                                continue;
                            }
                        };

                    // Phase 3: Shutdown old, start new.
                    // Note: brief downtime occurs while ports are released and re-bound.
                    // UDP packets arriving during this window are lost.
                    // TCP clients will get connection refused but should retry.
                    // Future: SO_REUSEPORT or socket activation for zero-downtime reload.
                    tracing::warn!("reload: shutting down old runtime (brief downtime)");
                    runtime.shutdown().await;
                    match runtime::Runtime::start(new_compiled, file.clone()).await {
                        Ok(new_runtime) => {
                            runtime = new_runtime;
                            tracing::info!("configuration reloaded successfully");
                        }
                        Err(e) => {
                            tracing::error!("reload: failed to start new runtime: {}", e);
                            // Rollback with in-memory snapshot of previous config
                            match runtime::Runtime::start(old_config, file).await {
                                Ok(restored) => {
                                    runtime = restored;
                                    tracing::warn!("reload: rolled back to previous configuration");
                                }
                                Err(e2) => {
                                    tracing::error!(
                                        "reload: rollback also failed: {} — exiting",
                                        e2
                                    );
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    })
}

/// --check: validate configuration and exit.
///
/// Runs the parser + `CompiledConfig::from_config` (which surfaces syntax and
/// structural errors via `anyhow::Error`), then hands the compiled config to
/// the static analyzer in [`crate::check`]. Diagnostics are rendered with
/// rustc-style source snippets and carets when the analyzer attached a
/// span; spanless diagnostics fall back to a one-line `level: message`.
///
/// Exit codes:
/// - `0` — analyzer is clean
/// - `1` — at least one error-level diagnostic
/// - `2` — `--strict-warnings` set and at least one warning was emitted
///   (errors also exit 2 under `--strict-warnings` so CI sees a single
///   "non-zero means investigate" signal)
fn run_check(config_path: &str, strict_warnings: bool) -> Result<()> {
    let path = Path::new(config_path);
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read configuration file: {}", path.display()))?;
    let config = config::load_config(path).context("configuration error")?;
    let compiled = CompiledConfig::from_config(config)?;
    let mut registry = crate::modules::ModuleRegistry::new();
    crate::modules::register_builtins(&mut registry);
    compiled.validate(&registry)?;

    // Build a single-file SourceMap so the renderer can resolve span
    // → file:line:col + snippet. The parser tags every span with
    // file_id = 0 (the only id we register here).
    let mut source_map = crate::dsl::span::SourceMap::new();
    source_map.add_file(path.to_path_buf(), source);

    let diagnostics = check::analyze(&compiled, &source_map);

    let mut errors = 0usize;
    let mut warnings = 0usize;
    for diag in &diagnostics {
        match diag.level {
            check::Level::Error => errors += 1,
            check::Level::Warning => warnings += 1,
            check::Level::Info => {}
        }
        check::render::render_diagnostic(diag, &source_map);
    }

    if errors > 0 {
        if strict_warnings {
            std::process::exit(2);
        }
        std::process::exit(1);
    }
    if strict_warnings && warnings > 0 {
        eprintln!("note: --strict-warnings is promoting warnings to errors");
        std::process::exit(2);
    }

    println!("Configuration OK");
    println!(
        "  {} input(s), {} output(s), {} process(es), {} pipeline(s)",
        compiled.inputs.len(),
        compiled.outputs.len(),
        compiled.processes.len(),
        compiled.pipelines.len(),
    );

    Ok(())
}

/// --test-pipeline: run a sample event through a pipeline and display trace.
fn run_test(config_path: &str, pipeline_name: &str, input_json: Option<&str>) -> Result<()> {
    let config =
        config::load_config(Path::new(config_path)).context("failed to load configuration")?;
    let compiled = CompiledConfig::from_config(config)?;

    let pipeline_def = compiled.pipelines.get(pipeline_name).context(format!(
        "pipeline '{}' not found. Available: {}",
        pipeline_name,
        compiled
            .pipelines
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    ))?;

    let table_store = runtime::init_tables(&compiled)?;
    let mut func_registry = FunctionRegistry::new();
    functions::register_builtins(&mut func_registry, table_store);
    let mut registry = crate::modules::ModuleRegistry::new();
    crate::modules::register_builtins(&mut registry);

    // `registry` exists only so `compiled.validate` can be reused later
    // if input/output validation grows — process lookup no longer needs
    // it (Block 4 removed the native process layer).
    let _ = &registry;

    let event = build_test_event(input_json)?;
    let result = run_pipeline(pipeline_def, event, &compiled, &func_registry, None)?;

    // Display trace
    println!("=== Pipeline: {} ===", pipeline_name);
    for entry in &result.trace {
        let label = if entry.label.is_empty() {
            String::new()
        } else {
            format!("  {}", entry.label)
        };
        let detail = if entry.detail.is_empty() {
            String::new()
        } else {
            format!(" → {}", entry.detail)
        };
        println!("[{}]{}{}", entry.stage, label, detail);
    }

    if !result.outputs.is_empty() {
        println!();
        for (name, evt) in &result.outputs {
            println!(
                "[output]  → {}  egress: {}",
                name,
                String::from_utf8_lossy(&evt.egress)
            );
            if !evt.workspace.is_empty() {
                print!("  workspace: {:?}", evt.workspace);
            }
            println!();
        }
    }

    Ok(())
}

fn build_test_event(input_json: Option<&str>) -> Result<Event> {
    let default_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    if let Some(json_str) = input_json {
        let v: serde_json::Value =
            serde_json::from_str(json_str).context("failed to parse --input JSON")?;

        let ingress = v
            .get("ingress")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let source: SocketAddr = v
            .get("source")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                if s.contains(':') {
                    s.parse().ok()
                } else {
                    format!("{}:0", s).parse().ok()
                }
            })
            .unwrap_or(default_addr);

        let mut event = Event::new(Bytes::from(ingress), source);

        if let Some(workspace) = v.get("workspace").and_then(|v| v.as_object()) {
            for (k, val) in workspace {
                event.workspace.insert(k.clone(), val.clone());
            }
        }

        Ok(event)
    } else {
        Ok(Event::new(
            Bytes::from_static(b"<134>sample syslog message"),
            default_addr,
        ))
    }
}

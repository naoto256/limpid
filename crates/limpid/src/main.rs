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
mod error_log;
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

    /// Promote *unknown identifier* warnings (unresolved workspace
    /// keys, unknown function names) to errors. Implies `--check`.
    /// Orthogonal to `--strict-warnings`: this changes the diagnostic
    /// **level** for one category only, while `--strict-warnings`
    /// changes the **exit code** for any leftover warning. Combine
    /// both to make CI fail loudly on typos (exit 1) while still
    /// surfacing ambient warnings (exit 2 fallback).
    #[arg(long)]
    ultra_strict: bool,

    /// Render the pipeline flow graph to stdout after analysis.
    /// Accepts `mermaid` (default), `dot`, or `ascii`. The analyzer's
    /// diagnostics remain on stderr so the graph output can be piped
    /// into a file or viewer without losing the check report.
    #[arg(long, value_name = "FORMAT", num_args = 0..=1, default_missing_value = "mermaid")]
    graph: Option<String>,

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

    if cli.check || cli.strict_warnings || cli.ultra_strict || cli.graph.is_some() {
        return run_check(
            &cli.config,
            cli.strict_warnings,
            cli.ultra_strict,
            cli.graph.as_deref(),
        );
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
/// Pipeline:
/// 1. Load + parse the main config and recursively expand `include`
///    directives via [`config::load_config_with_source_map`]. Each file
///    (main + every included file) is registered with a distinct
///    `file_id` so spans resolve to the correct physical file when the
///    renderer draws snippet+caret.
/// 2. Compute and emit the **summary header**: `checking <path>: N
///    inputs, M outputs, …`. Counts come from the parsed AST so an
///    include glob that matches zero files is visible (the analyzer
///    won't report "OK" with 0 pipelines silently).
/// 3. Compile + validate (surfaces syntax / structural errors).
/// 4. Run the static analyzer; render every diagnostic with rustc-
///    style snippet + caret + optional `help:` line.
/// 5. Emit a footer:
///    - clean: `<path>: Configuration OK (… ; dataflow check passed)`
///    - warnings: `<path>: Configuration OK (… ; N warnings, dataflow check passed)`
///    - errors: `error: N errors found`
///
/// Exit codes:
/// - `0` — analyzer is clean (warnings allowed unless `--strict-warnings`)
/// - `1` — at least one error-level diagnostic (including warnings
///   promoted by `--ultra-strict`)
/// - `2` — `--strict-warnings` set and at least one warning was emitted
///   (errors also exit 2 under `--strict-warnings` so CI sees a single
///   "non-zero means investigate" signal)
///
/// `--ultra-strict` and `--strict-warnings` are orthogonal:
/// - `--strict-warnings` alone: exit code change only (2 on any warning)
/// - `--ultra-strict` alone: unknown-ident warnings become errors (exit 1)
/// - both: unknown idents exit 1; leftover non-ident warnings exit 2
fn run_check(
    config_path: &str,
    strict_warnings: bool,
    ultra_strict: bool,
    graph_format: Option<&str>,
) -> Result<()> {
    let path = Path::new(config_path);

    // Resolve the graph format up front so a bad `--graph=foo` fails
    // before we spend time parsing the config. The error message lists
    // the accepted formats.
    let graph_format = match graph_format {
        Some(raw) => Some(check::graph::GraphFormat::parse(Some(raw))?),
        None => None,
    };

    // Step 1: load + expand includes, building the multi-file SourceMap
    // alongside the parsed AST. Without this step include-based configs
    // would be analyzed as the top-level file only, walking 0 pipelines.
    let (config, source_map) = config::load_config_with_source_map(path)
        .with_context(|| format!("failed to load {}", path.display()))?;

    // Step 2: definition counts derived from the parsed AST. Emitted
    // before any diagnostics so the operator sees the scope of what
    // was checked even if errors follow.
    let counts = check::DefCounts::from_config(&config);
    let file_count = source_map.file_count();
    println!(
        "checking {}: {} file(s), {} input(s), {} output(s), {} process(es), {} pipeline(s)",
        path.display(),
        file_count,
        counts.inputs,
        counts.outputs,
        counts.processes,
        counts.pipelines,
    );

    // Step 3: compile + validate. Errors here are syntactic / structural
    // and bubble up via anyhow; the analyzer only runs on a valid AST.
    let compiled = CompiledConfig::from_config(config)?;
    let mut registry = crate::modules::ModuleRegistry::new();
    crate::modules::register_builtins(&mut registry);
    compiled.validate(&registry)?;

    // Step 4: run analyzer + render diagnostics. Under `--ultra-strict`
    // we post-process the diagnostics to promote unknown-ident warnings
    // (workspace miss, function-name typo, reserved-name typo) into
    // errors — a CI-friendly shortcut that doesn't require a separate
    // analyzer pass.
    let diagnostics = check::analyze(&compiled, &source_map);
    let diagnostics = if ultra_strict {
        check::promote_unknown_idents(diagnostics)
    } else {
        diagnostics
    };

    // If --graph was requested, emit the flow visualization to stdout
    // before diagnostics/footer land on stderr/stdout. Doing this first
    // keeps the graph self-contained: a pipe like `limpid --check
    // --graph=mermaid foo.conf > flow.mmd` captures only the graph,
    // with summary/analysis noise on the other streams.
    if let Some(fmt) = graph_format {
        print!("{}", check::graph::render_graph(&compiled, fmt));
    }

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

    // Step 5: footer. Error / warning / clean each have a distinct
    // shape; the clean path always names the file path so a glob
    // wrapper (`limpid --check etc/*.conf`) produces one verdict line
    // per config without losing which one is which.
    if errors > 0 {
        eprintln!(
            "error: {}: {} error(s) found ({} warning(s))",
            path.display(),
            errors,
            warnings,
        );
        // Errors always take precedence: exit 1 regardless of
        // `--strict-warnings`. CI that greps for "non-zero" still
        // catches it; anyone switching on the code sees a clean
        // "1 = at least one error" signal.
        std::process::exit(1);
    }
    if strict_warnings && warnings > 0 {
        eprintln!(
            "{}: {} warning(s) (dataflow check passed)",
            path.display(),
            warnings,
        );
        eprintln!("note: --strict-warnings is promoting warnings to errors");
        std::process::exit(2);
    }

    let warn_frag = if warnings > 0 {
        format!("{} warning(s); ", warnings)
    } else {
        String::new()
    };
    println!(
        "{}: Configuration OK ({} pipeline(s), {} process(es); {}dataflow check passed)",
        path.display(),
        counts.pipelines,
        counts.processes,
        warn_frag,
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
    functions::register_user_functions(&mut func_registry, &compiled);
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

    // Surface the dead-letter-queue record for operator inspection.
    // The actual file write happens only inside the daemon's runtime
    // layer (`process_event`); --test-pipeline prints the would-be
    // JSONL line so the same recipe (`jq -c '.event' | inject`) can
    // be rehearsed against the trace.
    if let Some(ref err_ctx) = result.errored {
        println!();
        println!("[error_log]  {}", err_ctx.to_jsonl());
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

        if let Some(workspace) = v.get("workspace")
            && let Ok(crate::dsl::Value::Object(map)) =
                crate::dsl::value_json::json_to_value(workspace)
        {
            for (k, val) in map {
                event.workspace.insert(k, val);
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

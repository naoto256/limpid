//! Control socket: Unix domain socket server for limpidctl and
//! other management tools.
//!
//! Protocol: line-based over Unix stream socket.
//! All responses except `tap` are JSON.
//!
//! Commands:
//!   health                      — {"status":"ok","uptime_seconds":N}
//!   stats                       — pipeline/input/output metrics (JSON)
//!   list                        — pipeline structure with tap points (JSON)
//!   tap <kind> <name>           — stream event messages (LF-delimited text)
//!   tap <kind> <name> json      — stream full Event JSON (one per line)
//!   inject <kind> <name>        — push raw lines (read to EOF, reply {"injected":N})
//!   inject <kind> <name> json   — push full Event JSON lines (skip invalid lines)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::dsl::ast::*;
use crate::event::Event;
use crate::metrics::MetricsRegistry;
use crate::pipeline::CompiledConfig;
use crate::queue::QueueSender;
use crate::tap::TapRegistry;

const DEFAULT_SOCKET_PATH: &str = "/var/run/limpid/control.sock";

/// Maximum command line length (bytes). Prevents OOM from malicious clients.
const MAX_COMMAND_LEN: usize = 4096;

/// Per-input inject target: event channel + metrics handle (for events_injected).
pub type InputInjectTarget = (mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>);

pub struct ControlServer {
    socket_path: PathBuf,
    tap: TapRegistry,
    metrics: Arc<MetricsRegistry>,
    config: Arc<CompiledConfig>,
    input_senders: Arc<HashMap<String, InputInjectTarget>>,
    output_senders: Arc<HashMap<String, QueueSender>>,
    started_at: Instant,
}

impl ControlServer {
    pub fn new(
        socket_path: Option<String>,
        tap: TapRegistry,
        metrics: Arc<MetricsRegistry>,
        config: Arc<CompiledConfig>,
        input_senders: HashMap<String, InputInjectTarget>,
        output_senders: Arc<HashMap<String, QueueSender>>,
        started_at: Instant,
    ) -> Self {
        Self {
            socket_path: PathBuf::from(
                socket_path.unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string()),
            ),
            tap,
            metrics,
            config,
            input_senders: Arc::new(input_senders),
            output_senders,
            started_at,
        }
    }

    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Remove stale socket — only if it's actually a socket (not a symlink)
        if self.socket_path.exists() {
            match std::fs::symlink_metadata(&self.socket_path) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        error!(
                            "control socket: {:?} is a symlink — refusing to remove",
                            self.socket_path
                        );
                        return;
                    }
                    let _ = std::fs::remove_file(&self.socket_path);
                }
                Err(e) => {
                    warn!("control socket: cannot stat {:?}: {}", self.socket_path, e);
                }
            }
        }

        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                error!(
                    "control socket: failed to bind {:?}: {}",
                    self.socket_path, e
                );
                return;
            }
        };

        // Restrict socket permissions to owner + group (0o660)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o660);
            if let Err(e) = std::fs::set_permissions(&self.socket_path, perms) {
                warn!("control socket: failed to set permissions: {}", e);
            }
        }

        info!("control socket listening on {:?}", self.socket_path);

        let tap = Arc::new(self.tap);
        let config = self.config;
        let started_at = self.started_at;
        let metrics = self.metrics;
        let input_senders = self.input_senders;
        let output_senders = self.output_senders;

        let mut conn_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        loop {
            conn_handles.retain(|h| !h.is_finished());

            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("control socket: shutting down");
                        for h in &conn_handles {
                            h.abort();
                        }
                        break;
                    }
                }

                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let tap = Arc::clone(&tap);
                            let metrics_reg = Arc::clone(&metrics);
                            let config = Arc::clone(&config);
                            let input_senders = Arc::clone(&input_senders);
                            let output_senders = Arc::clone(&output_senders);
                            conn_handles.push(tokio::spawn(async move {
                                handle_connection(stream, tap, metrics_reg, config, input_senders, output_senders, started_at).await;
                            }));
                        }
                        Err(e) => {
                            error!("control socket: accept error: {}", e);
                        }
                    }
                }
            }
        }

        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tap: Arc<TapRegistry>,
    metrics: Arc<MetricsRegistry>,
    config: Arc<CompiledConfig>,
    input_senders: Arc<HashMap<String, InputInjectTarget>>,
    output_senders: Arc<HashMap<String, QueueSender>>,
    started_at: Instant,
) {
    let (reader, mut writer) = stream.into_split();
    // Limit the FIRST line read to MAX_COMMAND_LEN bytes to prevent OOM,
    // then unwrap for streaming commands (inject) that need unbounded reads.
    let limited = reader.take(MAX_COMMAND_LEN as u64);
    let mut reader = BufReader::new(limited);

    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            debug!("control socket: read error: {}", e);
            return;
        }
    }

    if !line.ends_with('\n') {
        let _ = writer.write_all(b"error: command too long\n").await;
        return;
    }

    let cmd = line.trim();
    debug!("control socket: received command: {}", cmd);

    if let Some(inject_args) = cmd.strip_prefix("inject ") {
        let parts: Vec<&str> = inject_args.split_whitespace().collect();
        let (kind, name, json_mode) = match parts.as_slice() {
            [kind, name] if matches!(*kind, "input" | "output") => {
                (*kind, (*name).to_string(), false)
            }
            [kind, name, "json"] if matches!(*kind, "input" | "output") => {
                (*kind, (*name).to_string(), true)
            }
            _ => {
                let _ = writer
                    .write_all(b"error: expected 'inject <input|output> <name> [json]'\n")
                    .await;
                return;
            }
        };
        // Lift the per-connection byte cap — inject streams can be large.
        // Any bytes buffered past the first line remain intact inside
        // the BufReader and will be consumed by handle_inject.
        reader.get_mut().set_limit(u64::MAX);
        handle_inject(
            kind,
            &name,
            json_mode,
            reader,
            &mut writer,
            &input_senders,
            &output_senders,
        )
        .await;
        return;
    }

    if let Some(tap_args) = cmd.strip_prefix("tap ") {
        let tap_args = tap_args.trim();
        // Accept:
        //   "<kind> <name>"        → raw message mode
        //   "<kind> <name> json"   → full-Event JSON mode
        let parts: Vec<&str> = tap_args.split_whitespace().collect();
        let (tap_target, json_mode) = match parts.as_slice() {
            [kind, name] if matches!(*kind, "input" | "process" | "output") => {
                (format!("{} {}", kind, name), false)
            }
            [kind, name, "json"] if matches!(*kind, "input" | "process" | "output") => {
                (format!("{} {}", kind, name), true)
            }
            _ => {
                let _ = writer
                    .write_all(b"error: expected 'tap <input|process|output> <name> [json]'\n")
                    .await;
                return;
            }
        };
        match tap.subscribe(&tap_target).await {
            Some(subscription) => {
                handle_tap(&tap_target, subscription, &mut writer, json_mode).await;
            }
            None => {
                let _ = writer
                    .write_all(format!("error: unknown tap point '{}'\n", tap_target).as_bytes())
                    .await;
            }
        }
    } else {
        let response = match cmd {
            "health" => {
                let uptime = started_at.elapsed().as_secs();
                json!({"status": "ok", "uptime_seconds": uptime}).to_string()
            }
            "stats" => metrics.to_json(),
            "list" => build_list_json(&config),
            _ => json!({"error": format!("unknown command '{}'", cmd)}).to_string(),
        };
        let _ = writer.write_all(response.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
    }
}

/// Build JSON listing of pipelines with their tap points in flow order.
fn build_list_json(config: &CompiledConfig) -> String {
    let mut pipelines = Vec::new();

    let mut names: Vec<&String> = config.pipelines.keys().collect();
    names.sort();

    for name in names {
        let Some(pipeline_def) = config.pipelines.get(name) else {
            continue;
        };
        let mut inputs: Vec<String> = Vec::new();
        let mut processes = Vec::new();
        let mut outputs = Vec::new();

        collect_pipeline_tap_points(
            &pipeline_def.body,
            &mut inputs,
            &mut processes,
            &mut outputs,
        );

        let mut p = Map::new();
        p.insert("name".into(), Value::String(name.clone()));
        // Keep scalar `input` for single-input pipelines (backward-compatible payload),
        // emit `inputs` array when fan-in is in play.
        match inputs.len() {
            0 => {}
            1 => {
                p.insert("input".into(), Value::String(inputs.remove(0)));
            }
            _ => {
                p.insert(
                    "inputs".into(),
                    Value::Array(inputs.into_iter().map(Value::String).collect()),
                );
            }
        }
        p.insert(
            "processes".into(),
            Value::Array(processes.into_iter().map(Value::String).collect()),
        );
        p.insert(
            "outputs".into(),
            Value::Array(outputs.into_iter().map(Value::String).collect()),
        );
        pipelines.push(Value::Object(p));
    }

    json!({"pipelines": pipelines}).to_string()
}

/// Recursively walk pipeline statements to collect tap points in order.
fn collect_pipeline_tap_points(
    stmts: &[PipelineStatement],
    inputs: &mut Vec<String>,
    processes: &mut Vec<String>,
    outputs: &mut Vec<String>,
) {
    for stmt in stmts {
        match stmt {
            PipelineStatement::Input(names) => {
                for name in names {
                    if !inputs.contains(name) {
                        inputs.push(name.clone());
                    }
                }
            }
            PipelineStatement::ProcessChain(chain) => {
                for elem in chain {
                    match elem {
                        ProcessChainElement::Named(name, _) => {
                            if !processes.contains(name) {
                                processes.push(name.clone());
                            }
                        }
                        ProcessChainElement::Inline(_) => {
                            // Inline processes don't have tap points
                        }
                    }
                }
            }
            PipelineStatement::Output(name) => {
                if !outputs.contains(name) {
                    outputs.push(name.clone());
                }
            }
            PipelineStatement::If(chain) => {
                for (_, body) in &chain.branches {
                    let stmts: Vec<PipelineStatement> = body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
                if let Some(else_body) = &chain.else_body {
                    let stmts: Vec<PipelineStatement> = else_body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
            }
            PipelineStatement::Switch(_, arms) => {
                for arm in arms {
                    let stmts: Vec<PipelineStatement> = arm
                        .body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
            }
            PipelineStatement::Drop | PipelineStatement::Finish => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_inject(
    kind: &str,
    name: &str,
    json_mode: bool,
    mut reader: BufReader<tokio::io::Take<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    input_senders: &HashMap<String, InputInjectTarget>,
    output_senders: &HashMap<String, QueueSender>,
) {
    enum Target {
        Input(mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>),
        Output(QueueSender),
    }

    let target = match kind {
        "input" => match input_senders.get(name) {
            Some((tx, metrics)) => Target::Input(tx.clone(), Arc::clone(metrics)),
            None => {
                let _ = writer
                    .write_all(format!("error: unknown input '{}'\n", name).as_bytes())
                    .await;
                return;
            }
        },
        "output" => match output_senders.get(name) {
            Some(tx) => Target::Output(tx.clone()),
            None => {
                let _ = writer
                    .write_all(format!("error: unknown output '{}'\n", name).as_bytes())
                    .await;
                return;
            }
        },
        _ => {
            let _ = writer
                .write_all(b"error: inject kind must be 'input' or 'output'\n")
                .await;
            return;
        }
    };

    let default_source: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut injected: u64 = 0;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                debug!("control socket: inject read error: {}", e);
                break;
            }
        }

        // Strip trailing newline(s)
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }

        let event = if json_mode {
            match Event::from_json(trimmed) {
                Some(ev) => ev,
                None => {
                    warn!("inject {} '{}': skipping invalid JSON line", kind, name);
                    continue;
                }
            }
        } else {
            Event::new(Bytes::copy_from_slice(trimmed.as_bytes()), default_source)
        };

        let ok = match &target {
            Target::Input(tx, metrics) => {
                let sent = tx.send(event).await.is_ok();
                if sent {
                    metrics
                        .events_injected
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                sent
            }
            Target::Output(tx) => {
                let sent = tx.send(event).await;
                if sent && let Some(m) = tx.metrics() {
                    m.events_injected
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                sent
            }
        };
        if !ok {
            warn!("inject {} '{}': downstream channel closed", kind, name);
            break;
        }
        injected += 1;
    }

    let response = json!({ "injected": injected }).to_string();
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.write_all(b"\n").await;
}

async fn handle_tap(
    output_name: &str,
    mut subscription: crate::tap::TapSubscription,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    json_mode: bool,
) {
    // Skip the human-readable header in JSON mode so output is pure NDJSON
    // (safe to pipe to `jq` or `limpidctl inject --json`).
    if !json_mode {
        let _ = writer
            .write_all(format!("tapping '{}' — events will stream below\n", output_name).as_bytes())
            .await;
    }

    loop {
        match subscription.recv().await {
            Ok(event) => {
                let line = if json_mode {
                    event.to_json_string()
                } else {
                    String::from_utf8_lossy(&event.egress).into_owned()
                };
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if writer
                    .write_all(
                        format!("[warning: dropped {} events due to slow reader]\n", n).as_bytes(),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let _ = writer.write_all(b"[output closed]\n").await;
                break;
            }
        }
    }
}

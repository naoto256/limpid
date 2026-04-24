//! CLI-level integration tests for `limpid --check`.
//!
//! Drives the actual binary so the summary header / Configuration OK
//! footer / error footer shapes are observed end-to-end. Anything that
//! parses these lines (CI, ops dashboards) sees them through this same
//! path — bare unit tests on `run_check` would skip exit codes.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn limpid_bin() -> &'static str {
    env!("CARGO_BIN_EXE_limpid")
}

fn run_check(config: &std::path::Path) -> std::process::Output {
    Command::new(limpid_bin())
        .arg("--check")
        .arg("--config")
        .arg(config)
        .output()
        .expect("failed to spawn limpid")
}

#[test]
fn check_clean_emits_summary_and_configuration_ok() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("clean.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    // Summary header on top.
    assert!(
        stdout.contains("checking ") && stdout.contains("1 input(s)"),
        "stdout: {}",
        stdout
    );
    // Configuration OK footer with dataflow hint.
    assert!(
        stdout.contains("Configuration OK") && stdout.contains("dataflow check passed"),
        "stdout: {}",
        stdout
    );
}

#[test]
fn check_with_warning_still_exits_zero_and_mentions_warnings() {
    // `lower(workspace.count)` where count is bound as Int → analyzer
    // emits a warning but no error, so exit is 0.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("warn.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    assert!(
        out.status.success(),
        "should exit 0 without --strict-warnings"
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    // Configuration OK footer should reference the warning count.
    assert!(stdout.contains("Configuration OK"), "stdout: {}", stdout);
    assert!(stdout.contains("1 warning(s)"), "stdout: {}", stdout);
    // The warning itself was rendered to stderr.
    assert!(stderr.contains("warning"), "stderr: {}", stderr);
}

#[test]
fn check_with_error_emits_error_footer_and_exits_one() {
    // Output references workspace.nope which nothing produces → error.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("err.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.nope}" }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("error:") && stderr.contains("error(s) found"),
        "stderr: {}",
        stderr
    );
}

#[test]
fn check_strict_warnings_promotes_to_exit_two() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("strict.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#,
    )
    .unwrap();

    let out = Command::new(limpid_bin())
        .arg("--check")
        .arg("--strict-warnings")
        .arg("--config")
        .arg(&conf)
        .output()
        .expect("failed to spawn limpid");
    assert_eq!(out.status.code(), Some(2));

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("--strict-warnings"), "stderr: {}", stderr);
}

#[test]
fn check_expands_includes_in_summary() {
    // Includes are walked: file_count and definition counts both reflect
    // the expanded set, not just the top-level file.
    let dir = TempDir::new().unwrap();
    let inc_dir = dir.path().join("parts");
    fs::create_dir(&inc_dir).unwrap();
    fs::write(
        inc_dir.join("inputs.limpid"),
        r#"def input i1 { type tcp bind "0.0.0.0:514" }"#,
    )
    .unwrap();
    fs::write(
        inc_dir.join("outputs.limpid"),
        r#"def output o1 { type stdout template "x" }"#,
    )
    .unwrap();
    fs::write(
        inc_dir.join("pipelines.limpid"),
        r#"def pipeline p { input i1; output o1 }"#,
    )
    .unwrap();

    let main = dir.path().join("main.conf");
    fs::write(&main, r#"include "parts/*.limpid""#).unwrap();

    let out = run_check(&main);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    // 1 main + 3 included = 4 files; counts come from across all of them.
    assert!(stdout.contains("4 file(s)"), "stdout: {}", stdout);
    assert!(stdout.contains("1 input(s)"), "stdout: {}", stdout);
    assert!(stdout.contains("1 output(s)"), "stdout: {}", stdout);
    assert!(stdout.contains("1 pipeline(s)"), "stdout: {}", stdout);
    assert!(stdout.contains("Configuration OK"), "stdout: {}", stdout);
}

// ---------------------------------------------------------------------------
// --graph flag (Block 11-B)
// ---------------------------------------------------------------------------

fn run_check_with_graph(config: &std::path::Path, graph_arg: &str) -> std::process::Output {
    let mut cmd = Command::new(limpid_bin());
    cmd.arg("--check").arg("--config").arg(config);
    if graph_arg == "--graph" {
        cmd.arg("--graph");
    } else {
        cmd.arg(graph_arg);
    }
    cmd.output().expect("failed to spawn limpid")
}

fn graph_conf(dir: &TempDir) -> std::path::PathBuf {
    let conf = dir.path().join("g.conf");
    fs::write(
        &conf,
        r#"
def input a { type tcp bind "0.0.0.0:514" }
def input b { type udp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def process parse { workspace.x = "y" }
def pipeline p {
    input a, b
    process parse
    output o
}
"#,
    )
    .unwrap();
    conf
}

#[test]
fn graph_bare_flag_defaults_to_mermaid_on_stdout() {
    let dir = TempDir::new().unwrap();
    let conf = graph_conf(&dir);

    let out = run_check_with_graph(&conf, "--graph");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    // Graph lands on stdout alongside the summary / footer.
    assert!(stdout.contains("flowchart LR"), "stdout: {}", stdout);
    assert!(
        stdout.contains("subgraph p[\"pipeline p\"]"),
        "stdout: {}",
        stdout
    );
    assert!(stdout.contains("\"input a\""), "stdout: {}", stdout);
    assert!(stdout.contains("\"input b\""), "stdout: {}", stdout);
    assert!(stdout.contains("\"process parse\""), "stdout: {}", stdout);
    assert!(stdout.contains("\"output o\""), "stdout: {}", stdout);
}

#[test]
fn graph_dot_format() {
    let dir = TempDir::new().unwrap();
    let conf = graph_conf(&dir);

    let out = run_check_with_graph(&conf, "--graph=dot");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("digraph {"), "stdout: {}", stdout);
    assert!(stdout.contains("rankdir=LR;"), "stdout: {}", stdout);
    assert!(
        stdout.contains("subgraph cluster_p {"),
        "stdout: {}",
        stdout
    );
    assert!(stdout.contains(" -> "), "stdout: {}", stdout);
}

#[test]
fn graph_ascii_format() {
    let dir = TempDir::new().unwrap();
    let conf = graph_conf(&dir);

    let out = run_check_with_graph(&conf, "--graph=ascii");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("pipeline p"), "stdout: {}", stdout);
    assert!(stdout.contains("inputs: a, b"), "stdout: {}", stdout);
    assert!(stdout.contains("process parse"), "stdout: {}", stdout);
    assert!(stdout.contains("└─ "), "stdout: {}", stdout);
}

#[test]
fn graph_unknown_format_is_rejected() {
    let dir = TempDir::new().unwrap();
    let conf = graph_conf(&dir);

    let out = run_check_with_graph(&conf, "--graph=svg");
    assert!(!out.status.success(), "should fail");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("unknown graph format") && stderr.contains("mermaid, dot, ascii"),
        "stderr: {}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// --ultra-strict flag (Block 11-C)
// ---------------------------------------------------------------------------

fn run_with_flags(config: &std::path::Path, flags: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(limpid_bin());
    cmd.arg("--check").arg("--config").arg(config);
    for f in flags {
        cmd.arg(f);
    }
    cmd.output().expect("failed to spawn limpid")
}

#[test]
fn ultra_strict_promotes_unknown_function_to_error() {
    // Unknown function → warning by default. With --ultra-strict it
    // becomes an error and exit code is 1.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("us_fn.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { workspace.x = upperr(ingress) }
    output o
}
"#,
    )
    .unwrap();

    // Baseline: no flag → exit 0 (warning only).
    let out = run_with_flags(&conf, &[]);
    assert!(
        out.status.success(),
        "baseline should succeed: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --ultra-strict: promoted to error, exit 1.
    let out = run_with_flags(&conf, &["--ultra-strict"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ultra_strict_leaves_type_mismatch_as_warning() {
    // Type mismatch (lower on Int) is a TypeMismatch warning and must
    // NOT be promoted by --ultra-strict alone.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("us_ty.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#,
    )
    .unwrap();

    let out = run_with_flags(&conf, &["--ultra-strict"]);
    assert!(
        out.status.success(),
        "type warning must remain exit 0: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ultra_strict_plus_strict_warnings_mixed_case() {
    // Mixed: one unknown-ident warning (promoted to error → exit 1)
    // and one type-mismatch warning. With both --ultra-strict and
    // --strict-warnings, error precedence still wins so exit is 1.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("us_mixed.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process {
        workspace.a = upperr(ingress)
        workspace.b = lower(workspace.count)
    }
    output o
}
"#,
    )
    .unwrap();

    let out = run_with_flags(&conf, &["--ultra-strict", "--strict-warnings"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "error precedence: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn strict_warnings_without_ultra_strict_still_exits_two_on_type_warning() {
    // Regression: --strict-warnings without --ultra-strict keeps its
    // existing exit-2 behavior for any warning category.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("sw_only.conf");
    fs::write(
        &conf,
        r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#,
    )
    .unwrap();

    let out = run_with_flags(&conf, &["--strict-warnings"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn check_self_inclusion_is_rejected() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("main.conf");
    fs::write(&main, r#"include "main.conf""#).unwrap();

    let out = run_check(&main);
    assert!(!out.status.success(), "self-inclusion must fail");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("self-inclusion"), "stderr: {}", stderr);
}

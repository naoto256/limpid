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

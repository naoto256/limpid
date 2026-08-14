//! CLI-level integration tests for `limpid --check`.
//!
//! Drives the actual binary so the summary header / Configuration OK
//! footer / error footer shapes are observed end-to-end. Anything that
//! parses these lines (CI, ops dashboards) sees them through this same
//! path — bare unit tests on `run_check` would skip exit codes.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
fn check_accepts_explicit_or_omitted_node_id() {
    for (case, node_id) in [("explicit", "node_id \"edge-a\"\n"), ("omitted", "")] {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join(format!("node-id-{case}.conf"));
        fs::write(
            &conf,
            format!(
                r#"{node_id}def input i {{ type syslog_tcp bind "0.0.0.0:514" }}
def output o {{ type stdout }}
def pipeline p {{ input i; output o }}
"#
            ),
        )
        .unwrap();

        let out = run_check(&conf);
        assert!(
            out.status.success(),
            "{case} node_id must be accepted: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn assert_node_id_rejected(case: &str, node_id: &str, diagnostic: &str) {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join(format!("node-id-{case}.conf"));
    fs::write(
        &conf,
        format!(
            r#"{node_id}def input i {{ type syslog_tcp bind "0.0.0.0:514" }}
def output o {{ type stdout }}
def pipeline p {{ input i; output o }}
"#
        ),
    )
    .unwrap();

    let out = run_check(&conf);
    assert!(!out.status.success(), "{case} node_id must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(diagnostic),
        "{case} diagnostic must contain {diagnostic:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_empty_node_id_with_semantic_diagnostic() {
    assert_node_id_rejected("empty", "node_id \"\"\n", "node_id must be non-empty");
}

#[test]
fn check_rejects_non_string_node_id_with_semantic_diagnostic() {
    assert_node_id_rejected(
        "wrong-type",
        "node_id 42\n",
        "node_id requires a string value",
    );
}

#[test]
fn check_rejects_duplicate_node_id_with_semantic_diagnostic() {
    assert_node_id_rejected(
        "duplicate",
        "node_id \"one\"\nnode_id \"two\"\n",
        "duplicate node_id",
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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
    // Output references workspace.nope inside a template — should
    // surface as a dataflow error even though nested `host` is
    // schema-known (the workspace-reference walk runs on every value,
    // schema-owned or not).
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("err.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type syslog_tcp peer { host "${workspace.nope}" port 1 } }
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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
        r#"def input i1 { type syslog_tcp bind "0.0.0.0:514" }"#,
    )
    .unwrap();
    fs::write(
        inc_dir.join("outputs.limpid"),
        r#"def output o1 { type stdout }"#,
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
// --graph flag
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
def input a { type syslog_tcp bind "0.0.0.0:514" }
def input b { type syslog_udp bind "0.0.0.0:514" }
def output o { type stdout }
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
// --ultra-strict flag
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
    // A registry miss without a near match is still a warning by default.
    // With --ultra-strict it becomes an error and exit code is 1.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("us_fn.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p {
    input i
    process { workspace.x = totally_missing_helper(1) }
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning"), "stderr: {stderr}");
    assert!(
        stderr.contains("totally_missing_helper"),
        "stderr: {stderr}"
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
fn check_resolves_user_function_from_include_closure() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("helper.limpid"),
        "def function included_helper(value) { value }\n",
    )
    .unwrap();

    let conf = dir.path().join("included_fn.conf");
    fs::write(
        &conf,
        r#"
include "helper.limpid"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p {
    input i
    process { workspace.x = included_helper(1) }
    output o
}
"#,
    )
    .unwrap();

    let out = run_with_flags(&conf, &[]);
    assert!(
        out.status.success(),
        "included user function should resolve: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unknown function"),
        "included user function was falsely rejected: {stderr}"
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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
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
fn block_collection_overloads_pass_strict_static_checking() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("block_collections.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p {
    input i
    process {
        let fields = {alpha: 1, beta: 2}
        let mapped = map(fields) { |key, value| "${key}:${value}" }
        let selected = filter(fields) { |key, value| value > 0 }
        let found = find(fields) { |key, value| key == "alpha" }
        let total = reduce(fields, 0) { |acc, key, value| acc + value }

        let array_mapped = map([1, 2]) { |value| value + 1 }
        let array_selected = filter([1, 2]) { |value| value > 0 }
        let array_found = find([1, 2]) { |value| value == 2 }
        let array_total = reduce([1, 2], 0) { |acc, value| acc + value }
        let empty_mapped = map(null) { |value| value }
        let empty_filtered = filter(null) { |value| value }
        let empty_found = find(null) { |value| value != null }
        let empty_total = reduce(null, 0) { |acc, value| acc + value }

        workspace.object_shape = parse_kv("gamma=3", " ", selected)
        workspace.array_shape = append(array_selected, 3)
        workspace.results = [
            mapped, found, total,
            array_mapped, array_found, array_total,
            empty_mapped, empty_filtered, empty_found, empty_total,
        ]
    }
    output o
}
"#,
    )
    .unwrap();

    let out = run_with_flags(&conf, &["--strict-warnings"]);
    assert!(
        out.status.success(),
        "strict check failed: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Global block schema (v0.7.2)
// ---------------------------------------------------------------------------

#[test]
fn check_flags_unknown_key_in_control_block() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("ctrl.conf");
    fs::write(
        &conf,
        r#"
control { sockt "/tmp/limpid.sock" }
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();
    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("control") && stderr.contains("sockt") && stderr.contains("socket"),
        "stderr: {}",
        stderr
    );
}

#[test]
fn check_flags_unknown_key_in_table_entry() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("tbl.conf");
    fs::write(
        &conf,
        r#"
table { tenants { mx 1000 } }
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();
    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("table 'tenants'") && stderr.contains("mx") && stderr.contains("max"),
        "stderr: {}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// Property schema (v0.7.2)
// ---------------------------------------------------------------------------
//
// `output tcp` is the schema-driven pilot. These tests exercise the
// analyzer's wiring of `dsl::schema::validate` against the Module's
// declared `property_schema()`.

#[test]
fn check_accepts_correct_framing_enum_value_without_false_warning() {
    // Pre-schema, `framing non_transparent` (a perfectly valid enum
    // value) tripped `expr_types::check_unknown_ident` because the
    // bare ident is unbound in expression context. With the schema
    // owning that key, the expression walk is skipped — no warning.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("ok.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    peer { host "127.0.0.1" port 514 }
    framing non_transparent
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {}", stderr);
    assert!(
        !stderr.contains("non_transparent") && !stdout.contains("non_transparent"),
        "schema-owned enum value should not be flagged as unknown ident\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
}

#[test]
fn check_accepts_syslog_tcp_output_with_per_peer_named_profile() {
    // Per-peer TLS on syslog_tcp (the post-0.7.6 shape, replacing the
    // standalone `output syslog_tls` module).
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("tls-output.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    tls { my { ca "/etc/limpid/ca.pem" } }
    peer { host "h"; port 6514; tls my }
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {}", stderr);
}

#[test]
fn check_loudly_rejects_typoed_framing_with_did_you_mean() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("typo.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    peer { host "127.0.0.1" port 514 }
    framing non_trasnaprent
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("framing") && stderr.contains("non_transparent"),
        "expected did-you-mean for framing typo\nstderr: {}",
        stderr
    );
}

#[test]
fn check_rejects_unknown_property_key_with_did_you_mean() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("bad-key.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    per { host "127.0.0.1" port 514 }
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("unknown property 'per'") && stderr.contains("peer"),
        "expected unknown-key error with suggestion\nstderr: {}",
        stderr
    );
}

#[test]
fn check_rejects_wrong_value_type_on_typed_property() {
    // `port` declared as Int — passing a string is a TypeMismatch.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("bad-type.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    peer { host "127.0.0.1" port "five-fourteen" }
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("port") && stderr.contains("integer"),
        "expected port type-mismatch\nstderr: {}",
        stderr
    );
}

#[test]
fn check_reports_every_property_finding_in_one_run() {
    // Multiple schema findings on the same output should all surface
    // in a single --check invocation, not just the first one.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("many.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    per { host "h" port 1 }
    framing non_trasnaprent
}
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(stderr.contains("per"), "stderr missing per: {}", stderr);
    assert!(
        stderr.contains("framing"),
        "stderr missing framing: {}",
        stderr
    );
}

#[test]
fn check_loudly_rejects_unknown_module_type() {
    // The runtime would bail at `create_input` with "unknown input
    // type"; surfacing the same diagnostic at --check time means CI
    // catches the typo before the daemon ever starts.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("badtype.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcl bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("unknown type 'syslog_tcl'") && stderr.contains("syslog_tcp"),
        "expected unknown-type error with did-you-mean\nstderr: {}",
        stderr
    );
}

#[test]
fn check_rejects_include_path_matching_no_files() {
    // Pre-0.7.2 this passed --check silently; the broken include
    // surfaced only at runtime as an "unknown process" complaint with
    // no obvious tie back to the typo'd include line.
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("badinc.conf");
    fs::write(
        &conf,
        r#"
include "does/not/exist.limpid"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type stdout }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_check(&conf);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "stderr: {}", stderr);
    assert!(
        stderr.contains("matched no files") && stderr.contains("does/not/exist.limpid"),
        "expected loud zero-match diagnostic with the include path\nstderr: {}",
        stderr
    );
}

#[test]
fn check_self_inclusion_is_rejected() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("main.conf");
    fs::write(&main, r#"include "main.conf""#).unwrap();

    let out = run_check(&main);
    assert!(!out.status.success(), "self-inclusion must fail");
    let stderr = String::from_utf8(out.stderr).unwrap();
    // a24de11 unified "self-inclusion" with general cycle detection;
    // a file including itself is a cycle of length 1.
    assert!(stderr.contains("cycle"), "stderr: {}", stderr);
}

// ---- daemon startup analyzer enforcement -----------------------------
//
// `--check` and daemon startup share the same compile-validate-analyze
// spine via `compile_and_analyze` in `main.rs`. These tests pin that
// contract: an operator who skips `--check` and launches the daemon
// directly hits the same analyzer rejection (= no "valid for daemon,
// rejected by --check" asymmetry).

fn run_daemon_attempt(config: &std::path::Path) -> std::process::Output {
    // Drive `limpid <conf>` without `--check`. For analyzer-rejected
    // configs the process bails at `compile_and_analyze` (= before any
    // I/O) in milliseconds. A 5-second polling timeout guards against
    // a future regression where the daemon accepts a config we expect
    // to reject and proceeds to bind listeners: without this the test
    // would block the suite indefinitely. The poll interval is short
    // enough that the happy (= reject) path still finishes in tens of
    // ms.
    //
    // Concurrent pipe drain (not `wait_with_output` at the end): the
    // daemon's diagnostic renderer writes several hundred bytes of
    // analyzer diagnostic to stderr just before exit. On macOS the
    // default pipe buffer + Rust's stdio buffering interact in a way
    // that lets `try_wait()` observe `None` even after the child
    // process has finished writing — the child is really blocked in
    // `close(2)` cleanup on the parent side, but from `try_wait`'s
    // POV it looks like the process is still alive. Running
    // `wait_with_output` *after* the poll loop breaks a
    // `try_wait`-only observer because the reader never keeps up
    // with the writer. Draining stdout/stderr concurrently in
    // separate threads clears the pipe as fast as the child writes
    // it, so exit is observed as soon as the child actually closes.
    let mut child = Command::new(limpid_bin())
        .arg("--config")
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn limpid");

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut buf).ok();
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut buf).ok();
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "daemon did not exit within 5s for config {} — analyzer regression?",
                        config.display(),
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("try_wait error: {}", e),
        }
    };

    let stdout = stdout_handle
        .join()
        .expect("stdout drain thread must not panic");
    let stderr = stderr_handle
        .join()
        .expect("stderr drain thread must not panic");
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn daemon_startup_rejects_workspace_in_output_config() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("daemon_workspace.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type syslog_tcp peer { host "${workspace.tag}" port 1 } }
def pipeline p {
    input i
    process { syslog.parse(ingress) }
    output o
}
"#,
    )
    .unwrap();

    let out = run_daemon_attempt(&conf);
    assert!(
        !out.status.success(),
        "daemon must reject config that `--check` would reject; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("workspace.tag"),
        "expected analyzer-style diagnostic on daemon path; stderr: {}",
        stderr
    );
    assert!(
        stderr.contains("rejected by analyzer"),
        "expected `compile_and_analyze` bail message; stderr: {}",
        stderr
    );
}

#[test]
fn daemon_startup_rejects_egress_reference_in_output_config() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("daemon_egress.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/var/log/${egress}.log" }
def pipeline p { input i; output o }
"#,
    )
    .unwrap();

    let out = run_daemon_attempt(&conf);
    assert!(
        !out.status.success(),
        "daemon must reject pipeline-mutable refs; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("egress") && stderr.contains("pipeline-mutable state"),
        "expected pipeline-mutable diagnostic; stderr: {}",
        stderr
    );
}

#[test]
fn daemon_and_check_reject_same_workspace_config() {
    // Asymmetry sentinel: if `--check` produces an error message for
    // an output workspace reference but daemon startup happily
    // accepts the same file (or vice versa), this test fails — making
    // it impossible to silently regress the structural rule (= what
    // happens when a future analyzer rule is added but daemon path
    // forgets to re-run analyze).
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("symmetric.conf");
    fs::write(
        &conf,
        r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/var/log/${workspace.tenant}.log" }
def pipeline p {
    input i
    process { parse_json(ingress) }
    output o
}
"#,
    )
    .unwrap();

    let check_out = run_check(&conf);
    let check_stderr = String::from_utf8(check_out.stderr).unwrap();
    assert!(!check_out.status.success(), "--check must reject");

    let daemon_out = run_daemon_attempt(&conf);
    let daemon_stderr = String::from_utf8(daemon_out.stderr).unwrap();
    assert!(!daemon_out.status.success(), "daemon must also reject");

    // Both surfaces must mention the offending reference.
    assert!(
        check_stderr.contains("workspace.tenant") && daemon_stderr.contains("workspace.tenant"),
        "asymmetric rejection:\n--check stderr: {}\ndaemon stderr: {}",
        check_stderr,
        daemon_stderr,
    );
}

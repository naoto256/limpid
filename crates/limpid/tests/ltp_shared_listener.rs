//! Black-box contract tests for coalescing logical LTP inputs onto one listener.
//!
//! These tests intentionally drive the released CLI/runtime surface. They are
//! RED until the runtime owns LTP listener groups instead of letting every
//! logical `input ltp` bind independently.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde_json::Value;
use tempfile::TempDir;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn limpid_bin() -> &'static str {
    env!("CARGO_BIN_EXE_limpid")
}

fn free_tcp_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn secure_tempdir() -> TempDir {
    let dir = TempDir::new().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn write_identity(path: &Path) -> String {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    fs::write(
        path,
        pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut spki = ED25519_SPKI_PREFIX.to_vec();
    spki.extend_from_slice(pair.public_key().as_ref());
    base64::engine::general_purpose::STANDARD.encode(spki)
}

fn run_check(source: &str) -> std::process::Output {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("shared-listener.limpid");
    fs::write(&config, source).unwrap();
    Command::new(limpid_bin())
        .arg("--check")
        .arg("--config")
        .arg(config)
        .output()
        .unwrap()
}

fn ltp_input(name: &str, bind: &str, peer_id: &str, spki: &str, max: usize) -> String {
    format!(
        r#"def input {name} {{
    type ltp
    bind "{bind}"
    peer {{ node_id "{peer_id}" pubkey "{spki}" }}
    max_connections {max}
}}
def pipeline pipeline_{name} {{ input {name}; finish }}
"#
    )
}

#[test]
fn single_logical_input_keeps_repeatable_peer_syntax() {
    let source = format!(
        r#"node_key "not-read-by-check.pem"
def input shared {{
    type ltp
    bind "127.0.0.1:27514"
    peer {{ node_id "jump01" pubkey "{}" }}
    peer {{ node_id "jump02" pubkey "{}" }}
}}
def pipeline receive {{ input shared; finish }}
"#,
        write_identity(&TempDir::new().unwrap().path().join("jump01.pem")),
        write_identity(&TempDir::new().unwrap().path().join("jump02.pem")),
    );
    let output = run_check(&source);
    assert!(
        output.status.success(),
        "repeatable peers are a compatibility contract: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn listener_group_rejects_duplicate_node_id() {
    let first = write_identity(&TempDir::new().unwrap().path().join("first.pem"));
    let second = write_identity(&TempDir::new().unwrap().path().join("second.pem"));
    let bind = "127.0.0.1:27514";
    let source = format!(
        "node_key \"not-read-by-check.pem\"\n{}{}",
        ltp_input("from_a", bind, "duplicate", &first, 8),
        ltp_input("from_b", bind, "duplicate", &second, 8),
    );
    let output = run_check(&source);
    assert!(
        !output.status.success(),
        "duplicate group node_id must fail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("duplicate LTP peer node_id in listener group"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn listener_group_rejects_duplicate_spki() {
    let shared = write_identity(&TempDir::new().unwrap().path().join("shared.pem"));
    let bind = "127.0.0.1:27514";
    let source = format!(
        "node_key \"not-read-by-check.pem\"\n{}{}",
        ltp_input("from_a", bind, "jump01", &shared, 8),
        ltp_input("from_b", bind, "jump02", &shared, 8),
    );
    let output = run_check(&source);
    assert!(!output.status.success(), "duplicate group SPKI must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("duplicate LTP peer public key in listener group"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn listener_group_rejects_overlapping_nonidentical_binds() {
    let first = write_identity(&TempDir::new().unwrap().path().join("first.pem"));
    let second = write_identity(&TempDir::new().unwrap().path().join("second.pem"));
    for (wildcard, specific) in [
        ("0.0.0.0:27514", "127.0.0.1:27514"),
        ("[::]:27514", "[::1]:27514"),
    ] {
        let source = format!(
            "node_key \"not-read-by-check.pem\"\n{}{}",
            ltp_input("wildcard", wildcard, "jump01", &first, 8),
            ltp_input("specific", specific, "jump02", &second, 8),
        );
        let output = run_check(&source);
        assert!(!output.status.success(), "{wildcard} overlaps {specific}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("overlapping LTP listener binds"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn listener_group_accepts_distinct_specific_addresses() {
    let first = write_identity(&TempDir::new().unwrap().path().join("first.pem"));
    let second = write_identity(&TempDir::new().unwrap().path().join("second.pem"));
    let source = format!(
        "node_key \"not-read-by-check.pem\"\n{}{}",
        ltp_input("loopback_one", "127.0.0.1:27514", "jump01", &first, 8),
        ltp_input("loopback_two", "127.0.0.2:27514", "jump02", &second, 8),
    );
    let output = run_check(&source);
    assert!(
        output.status.success(),
        "distinct specific addresses must remain separate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn identical_bind_requires_matching_listener_settings() {
    let first = write_identity(&TempDir::new().unwrap().path().join("first.pem"));
    let second = write_identity(&TempDir::new().unwrap().path().join("second.pem"));
    let bind = "127.0.0.1:27514";
    let matching = format!(
        "node_key \"not-read-by-check.pem\"\n{}{}",
        ltp_input("from_a", bind, "jump01", &first, 8),
        ltp_input("from_b", bind, "jump02", &second, 8),
    );
    let output = run_check(&matching);
    assert!(
        output.status.success(),
        "matching max_connections must share: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mismatched = format!(
        "node_key \"not-read-by-check.pem\"\n{}{}",
        ltp_input("from_a", bind, "jump01", &first, 8),
        ltp_input("from_b", bind, "jump02", &second, 9),
    );
    let output = run_check(&mismatched);
    assert!(
        !output.status.success(),
        "mismatched max_connections must fail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("LTP listener max_connections mismatch"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_receiver_config(
    dir: &Path,
    bind: SocketAddr,
    server_key: &Path,
    jump01_spki: &str,
    jump02_spki: &str,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let config = dir.join("receiver.limpid");
    let control = dir.join("receiver.sock");
    let out01 = dir.join("jump01.log");
    let out02 = dir.join("jump02.log");
    fs::write(
        &config,
        format!(
            r#"node_id "receiver"
node_key {server_key:?}
control {{ socket {control:?} }}
def input ltp_jump01 {{
    type ltp
    bind "{bind}"
    peer {{ node_id "jump01" pubkey {jump01_spki:?} }}
    max_connections 8
}}
def input ltp_jump02 {{
    type ltp
    bind "{bind}"
    peer {{ node_id "jump02" pubkey {jump02_spki:?} }}
    max_connections 8
}}
def output out01 {{ type file path {out01:?} }}
def output out02 {{ type file path {out02:?} }}
def pipeline p01 {{ input ltp_jump01; output out01 }}
def pipeline p02 {{ input ltp_jump02; output out02 }}
"#
        ),
    )
    .unwrap();
    (config, control, out01, out02)
}

fn spawn_daemon(config: &Path, log: &Path) -> Child {
    let stdout = File::create(log.with_extension("stdout")).unwrap();
    let stderr = File::create(log).unwrap();
    Command::new(limpid_bin())
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap()
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn stop_daemon(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn identical_bind_starts_one_listener_without_partial_eaddrinuse() {
    let dir = secure_tempdir();
    let server_key = dir.path().join("server.pem");
    let jump01_key = dir.path().join("jump01.pem");
    let jump02_key = dir.path().join("jump02.pem");
    let _server_spki = write_identity(&server_key);
    let jump01_spki = write_identity(&jump01_key);
    let jump02_spki = write_identity(&jump02_key);
    let (config, control, _, _) = write_receiver_config(
        dir.path(),
        free_tcp_addr(),
        &server_key,
        &jump01_spki,
        &jump02_spki,
    );
    let log = dir.path().join("receiver.stderr");
    let mut child = spawn_daemon(&config, &log);
    wait_for_path(&control);
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        child.try_wait().unwrap().is_none(),
        "daemon exited during startup"
    );
    stop_daemon(&mut child);

    let mut startup_log = fs::read_to_string(&log).unwrap();
    startup_log.push_str(&fs::read_to_string(log.with_extension("stdout")).unwrap());
    assert!(
        !startup_log.contains("Address already in use") && !startup_log.contains("EADDRINUSE"),
        "same-bind inputs must be coalesced before startup, not partially started: {startup_log}"
    );
}

#[test]
fn spec_fixture_pins_connection_and_listener_lifecycle_fail_closed_cases() {
    let spec: Value =
        serde_json::from_str(include_str!("fixtures/ltp_shared_listener_contract.json")).unwrap();
    assert_eq!(spec["version"], 1);
    assert_eq!(spec["peer_cardinality"], "repeatable");
    assert_eq!(
        spec["connection_cases"],
        serde_json::json!([
            "declared_spki_and_matching_hello_dispatches",
            "unknown_spki_is_rejected",
            "wrong_hello_node_id_is_rejected",
            "hello_timeout_releases_group_capacity",
            "protocol_error_isolated_to_connection"
        ])
    );
    assert_eq!(
        spec["lifecycle_cases"],
        serde_json::json!([
            "group_capacity_is_listener_wide",
            "bind_failure_aborts_startup_before_partial_service",
            "shutdown_closes_listener_and_all_connections"
        ])
    );
    assert_eq!(spec["e2e"]["binds"], 1);
    assert_eq!(
        spec["e2e"]["logical_inputs"],
        serde_json::json!(["ltp_jump01", "ltp_jump02"])
    );
    assert_eq!(spec["e2e"]["independent_input_metrics"], true);
    assert_eq!(spec["e2e"]["independent_pipeline_delivery"], true);
}

fn read_stats(socket: &Path) -> Value {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.write_all(b"stats\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

fn metric_value(stats: &Value, family: &str, labels: &[(&str, &str)]) -> u64 {
    let metric = stats["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["name"] == family)
        .unwrap_or_else(|| panic!("missing metric family {family}"));
    metric["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|series| {
            labels
                .iter()
                .all(|(key, value)| series["labels"][key] == *value)
        })
        .unwrap_or_else(|| panic!("missing series {family} {labels:?}"))["value"]
        .as_u64()
        .unwrap()
}

#[test]
fn shared_listener_prepopulates_independent_logical_input_metrics() {
    let dir = secure_tempdir();
    let server_key = dir.path().join("server.pem");
    let jump01_key = dir.path().join("jump01.pem");
    let jump02_key = dir.path().join("jump02.pem");
    let _server_spki = write_identity(&server_key);
    let jump01_spki = write_identity(&jump01_key);
    let jump02_spki = write_identity(&jump02_key);
    let (config, control, _, _) = write_receiver_config(
        dir.path(),
        free_tcp_addr(),
        &server_key,
        &jump01_spki,
        &jump02_spki,
    );
    let log = dir.path().join("receiver.stderr");
    let mut child = spawn_daemon(&config, &log);
    wait_for_path(&control);
    let stats = read_stats(&control);
    stop_daemon(&mut child);

    assert_eq!(
        metric_value(
            &stats,
            "limpid_input_events_received_total",
            &[("input", "ltp_jump01")]
        ),
        0
    );
    assert_eq!(
        metric_value(
            &stats,
            "limpid_input_events_received_total",
            &[("input", "ltp_jump02")]
        ),
        0
    );
}

fn write_sender_config(
    dir: &Path,
    node_id: &str,
    node_key: &Path,
    server_spki: &str,
    source_bind: SocketAddr,
    ltp_endpoint: SocketAddr,
) -> (PathBuf, PathBuf) {
    let config = dir.join(format!("{node_id}.limpid"));
    let control = dir.join(format!("{node_id}.sock"));
    fs::write(
        &config,
        format!(
            r#"node_id "{node_id}"
node_key {node_key:?}
control {{ socket {control:?} }}
def input source {{ type syslog_tcp bind "{source_bind}" }}
def output relay {{
    type ltp
    peer {{ node_id "receiver" pubkey {server_spki:?} endpoint "{ltp_endpoint}" }}
}}
def pipeline send {{ input source; output relay }}
"#
        ),
    )
    .unwrap();
    (config, control)
}

fn send_one_syslog_frame(address: SocketAddr, marker: &[u8]) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut stream = loop {
        match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out connecting to {address}: {error}"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    stream.write_all(marker).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

fn wait_for_exact_file(path: &Path, expected: &[u8]) -> bool {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        if fs::read(path).is_ok_and(|bytes| bytes == expected) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn two_peers_on_one_port_route_to_independent_inputs_pipelines_and_metrics() {
    let dir = secure_tempdir();
    let server_key = dir.path().join("server.pem");
    let jump01_key = dir.path().join("jump01.pem");
    let jump02_key = dir.path().join("jump02.pem");
    let server_spki = write_identity(&server_key);
    let jump01_spki = write_identity(&jump01_key);
    let jump02_spki = write_identity(&jump02_key);
    let ltp_addr = free_tcp_addr();
    let jump01_source = free_tcp_addr();
    let jump02_source = free_tcp_addr();
    let (receiver_config, receiver_control, out01, out02) = write_receiver_config(
        dir.path(),
        ltp_addr,
        &server_key,
        &jump01_spki,
        &jump02_spki,
    );
    let (jump01_config, jump01_control) = write_sender_config(
        dir.path(),
        "jump01",
        &jump01_key,
        &server_spki,
        jump01_source,
        ltp_addr,
    );
    let (jump02_config, jump02_control) = write_sender_config(
        dir.path(),
        "jump02",
        &jump02_key,
        &server_spki,
        jump02_source,
        ltp_addr,
    );

    let mut receiver = spawn_daemon(&receiver_config, &dir.path().join("receiver.stderr"));
    wait_for_path(&receiver_control);
    let mut jump01 = spawn_daemon(&jump01_config, &dir.path().join("jump01.stderr"));
    let mut jump02 = spawn_daemon(&jump02_config, &dir.path().join("jump02.stderr"));
    wait_for_path(&jump01_control);
    wait_for_path(&jump02_control);

    let marker01 = b"<13>shared-listener-jump01";
    let marker02 = b"<13>shared-listener-jump02";
    send_one_syslog_frame(jump01_source, marker01);
    send_one_syslog_frame(jump02_source, marker02);
    let mut expected01 = marker01.to_vec();
    expected01.push(b'\n');
    let mut expected02 = marker02.to_vec();
    expected02.push(b'\n');
    let delivered01 = wait_for_exact_file(&out01, &expected01);
    let delivered02 = wait_for_exact_file(&out02, &expected02);
    let stats = read_stats(&receiver_control);

    stop_daemon(&mut jump01);
    stop_daemon(&mut jump02);
    stop_daemon(&mut receiver);

    assert!(delivered01, "jump01 event did not reach its bound pipeline");
    assert!(delivered02, "jump02 event did not reach its bound pipeline");
    for (input, pipeline) in [("ltp_jump01", "p01"), ("ltp_jump02", "p02")] {
        assert_eq!(
            metric_value(
                &stats,
                "limpid_input_events_received_total",
                &[("input", input)]
            ),
            1,
            "logical input metric must be independent"
        );
        assert_eq!(
            metric_value(
                &stats,
                "limpid_pipeline_events_received_total",
                &[("pipeline", pipeline)]
            ),
            1,
            "pipeline routing must follow the authenticated logical input"
        );
    }
}

#[test]
fn fixture_file_is_secret_free_and_consumed() {
    let mut fixture = String::new();
    File::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ltp_shared_listener_contract.json"
    ))
    .unwrap()
    .read_to_string(&mut fixture)
    .unwrap();
    assert!(!fixture.contains("PRIVATE KEY"));
    assert!(!fixture.contains("pubkey"));
    assert!(fixture.contains("unknown_spki_is_rejected"));
}

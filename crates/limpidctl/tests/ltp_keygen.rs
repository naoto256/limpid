use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

use base64::Engine as _;
use ring::signature::KeyPair as _;

const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn run_keygen(path: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_limpidctl"))
        .arg("ltp")
        .arg("keygen")
        .arg(path)
        .output()
        .expect("run limpidctl ltp keygen")
}

#[test]
fn keygen_stdout_is_exactly_the_matching_spki_base64_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.pem");
    let output = run_keygen(&path);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.ends_with('\n'));
    assert_eq!(stdout.lines().count(), 1);
    let spki = base64::engine::general_purpose::STANDARD
        .decode(stdout.trim_end())
        .unwrap();
    assert_eq!(&spki[..ED25519_SPKI_PREFIX.len()], &ED25519_SPKI_PREFIX);

    let document = pem::parse(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(document.tag(), "PRIVATE KEY");
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(document.contents()).unwrap();
    assert_eq!(
        &spki[ED25519_SPKI_PREFIX.len()..],
        pair.public_key().as_ref()
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("generated Ed25519 node key at")
    );
}

#[test]
fn keygen_rejects_existing_dangling_and_missing_parent_paths_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing.pem");
    std::fs::write(&existing, b"preserve me").unwrap();
    let output = run_keygen(&existing);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read(&existing).unwrap(), b"preserve me");

    let dangling = dir.path().join("dangling.pem");
    symlink(dir.path().join("missing-target.pem"), &dangling).unwrap();
    let output = run_keygen(&dangling);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        std::fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let missing_parent = dir.path().join("missing").join("node.pem");
    let output = run_keygen(&missing_parent);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!missing_parent.exists());
    assert!(!missing_parent.parent().unwrap().exists());
}

#[test]
fn keygen_removes_the_created_file_when_persisting_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write-failure.pem");
    let mut command = Command::new(env!("CARGO_BIN_EXE_limpidctl"));
    command.arg("ltp").arg("keygen").arg(&path);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            Ok(())
        });
    }
    let output = command.output().expect("run limited keygen");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to persist node key"));
    assert!(
        !path.exists(),
        "failed subprocess must not leave key residue"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "failed subprocess must not leave a temporary file"
    );
}

#[test]
fn keygen_enforces_mode_0600_even_under_a_restrictive_umask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restrictive-umask.pem");
    let mut command = Command::new(env!("CARGO_BIN_EXE_limpidctl"));
    command.arg("ltp").arg("keygen").arg(&path);
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o777);
            Ok(())
        });
    }
    let output = command.output().expect("run keygen with restrictive umask");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[test]
fn keygen_publishes_a_bare_relative_path_in_the_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_limpidctl"));
    command
        .current_dir(dir.path())
        .arg("ltp")
        .arg("keygen")
        .arg("node.pem");

    let output = command.output().expect("run relative-path keygen");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = dir.path().join("node.pem");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

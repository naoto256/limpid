use std::process::Command;

#[test]
fn version_reports_package_identity_and_exits() {
    let output = Command::new(env!("CARGO_BIN_EXE_limpid-prometheus"))
        .arg("--version")
        .output()
        .expect("run version command");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::str::from_utf8(&output.stdout).unwrap().trim_end(),
        concat!("limpid-prometheus ", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

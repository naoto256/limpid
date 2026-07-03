//! Links libsystemd when the `journal` feature is enabled.
//!
//! The reader itself is a hand-written FFI binding
//! (`src/modules/input/journal_sys.rs`, MIT OR Apache-2.0); this
//! script only resolves the runtime library it dynamically links
//! against — the same `libsystemd-dev` package the removed
//! `rust-systemd`/`libsystemd-sys` (LGPL-2.1-or-later) dependency
//! required at build time.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_JOURNAL");

    if std::env::var_os("CARGO_FEATURE_JOURNAL").is_none() {
        return;
    }

    if let Err(e) = pkg_config::probe_library("libsystemd") {
        panic!(
            "journal feature requires libsystemd-dev (pkg-config could not find `libsystemd`): {e}"
        );
    }
}

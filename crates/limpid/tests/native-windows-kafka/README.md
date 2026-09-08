# Native Windows Kafka dependency tests

Run `cargo test --locked --manifest-path crates/limpid/tests/native-windows-kafka/Cargo.toml --lib`
from the repository root on Windows with MSVC Build Tools, CMake and native
Windows Perl (including `IPC::Cmd`) available. Git's reduced MSYS Perl is not
sufficient for the vendored OpenSSL build. No broker installation is required:
librdkafka's mock cluster listens on loopback and ephemeral ports.

Set `CARGO_TARGET_DIR` to a short absolute path before building (for example,
`$env:CARGO_TARGET_DIR = 'C:/build/limpid-kafka'` in PowerShell). The default
nested harness target can make OpenSSL object paths exceed MSVC's path limit;
this fails with C1083 even when the Rust sources and native prerequisites are
valid. The CI job also uses a short target directory.

These tests verify librdkafka's compiled SSL/PLAIN/SCRAM capabilities and binary
payload transmission. They do not validate TLS/SASL handshakes, the limpid Kafka
output actor, pipeline routing or recovery/ACK semantics. Those tests are still
required before announcing Windows support.

The Windows dependency feature set matches the daemon's Windows declaration.
The lock pins the same `rdkafka`, `rdkafka-sys` and `openssl-sys` versions as the
root lock; other test-harness dependencies may differ. Keep these native library
versions aligned when changing the root lock.

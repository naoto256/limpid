# Native Windows stdout boundary tests

On Windows, run `cargo test --locked --manifest-path crates/limpid/tests/native-windows-stdout/Cargo.toml --lib`
from the worktree root. Tests use temporary files and anonymous pipes; they do
not change the process stdout handle or require administrative privileges.
The harness is gated by `cfg(windows)`; a successful command on another OS does not verify these native tests.

The harness includes the production Windows transport source. The daemon's
stdout output selects this transport on Windows and retains its common event
framing, metrics, retry and ACK/DLQ code. A dedicated writer and cancellation
supervisor hold an owned stdout handle and serialize entire frames. A dropped
caller requests cancellation; the supervisor keeps the frame guard until the
writer reports actual completion. Cancellation cannot target another pool job
or a later stdout frame. This also works after the caller's Tokio runtime exits.

These are native transport tests, not full daemon/queue/ACK tests. Console
display depends on the Windows console mode; exact byte preservation is tested
with redirection. Service stdout absence, full queue shutdown/replay and the
existing Unix actor regressions still need their integration environments.

# Installation

## Building from source

```bash
# Clone the repository
git clone https://github.com/naoto256/limpid.git
cd limpid

# Build all binaries
cargo build --release -p limpid -p limpidctl -p limpid-prometheus

# Binaries are in target/release/
ls target/release/limpid target/release/limpidctl target/release/limpid-prometheus
```

### Optional features

```bash
# systemd journal support (Linux only)
sudo apt install libsystemd-dev
cargo build --release -p limpid --features journal

# Kafka output support
cargo build --release -p limpid --features kafka

# Both
cargo build --release -p limpid --features journal,kafka
```

## Installing the .deb packages

```bash
cargo install cargo-deb

# Main daemon (includes limpidctl)
cargo deb -p limpid
sudo dpkg -i target/debian/limpid_*.deb

# Prometheus exporter (optional)
cargo deb -p limpid-prometheus
sudo dpkg -i target/debian/limpid-prometheus_*.deb
```

See [Packaging](./operations/packaging.md) for details on package contents and configuration.

## Quick start

### 1. Create a configuration

```bash
sudo mkdir -p /etc/limpid/{inputs,outputs,processes,pipelines}
```

**/etc/limpid/limpid.conf:**
```
include "inputs/*.limpid"
include "outputs/*.limpid"
include "pipelines/*.limpid"

control {
    socket "/var/run/limpid/control.sock"
    error_log "/var/log/limpid/error_log.jsonl"
}
```

> Glob `include` directives require at least one matching file — limpid
> rejects a zero-match glob at load time to catch typos. Add
> `include "processes/*.limpid"` *after* you create your first process
> snippet under `processes/`. The same applies to any other empty
> directory.

`error_log` is the daemon-wide recovery sink: payloads that retry-exhaust or fail the shutdown flush are persisted there as JSONL instead of being silently dropped. Recommended whenever a pipeline uses retry or batched outputs; `limpid --check --strict-warnings` will warn if a recovery-dependent config omits it. See [Error Log (DLQ)](./operations/error-log.md) for the format and rotation guidance.

**/etc/limpid/inputs/syslog.limpid:**
```
def input syslog {
    type syslog_udp
    bind "0.0.0.0:514"
}
```

**/etc/limpid/outputs/archive.limpid:**
```
def output archive {
    type file
    path "/var/log/limpid/syslog.log"
}
```

**/etc/limpid/pipelines/main.limpid:**
```
def pipeline main {
    input syslog
    output archive
}
```

### 2. Validate

```bash
limpid --check --config /etc/limpid/limpid.conf
# checking /etc/limpid/limpid.conf: 3 file(s), 1 input(s), 1 output(s), 0 process(es), 1 pipeline(s)
# /etc/limpid/limpid.conf: Configuration OK (1 pipeline(s), 0 process(es); dataflow check passed)
```

### 3. Test with sample data

```bash
limpid --test-pipeline main --config /etc/limpid/limpid.conf \
  --input '{"ingress": "<134>Apr 15 10:30:00 myhost sshd[1234]: Accepted publickey"}'
```

### 4. Start the daemon

```bash
sudo systemctl start limpid
sudo systemctl status limpid

# Send a test message
logger "hello from limpid"

# Check the log
tail /var/log/limpid/syslog.log
```

### 5. Monitor

```bash
# Real-time event stream
sudo limpidctl tap output archive

# Metrics
sudo limpidctl stats
```

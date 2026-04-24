# Summary

[Introduction](./introduction.md)
[Design Principles](./design-principles.md)

# Getting Started

- [Installation](./getting-started.md)

# Configuration

- [Main Configuration](./configuration.md)
- [Inputs](./inputs/README.md)
  - [syslog_udp](./inputs/syslog-udp.md)
  - [syslog_tcp](./inputs/syslog-tcp.md)
  - [syslog_tls](./inputs/syslog-tls.md)
  - [tail](./inputs/tail.md)
  - [journal](./inputs/journal.md)
  - [unix_socket](./inputs/unix-socket.md)
- [Outputs](./outputs/README.md)
  - [file](./outputs/file.md)
  - [http](./outputs/http.md)
  - [kafka](./outputs/kafka.md)
  - [tcp](./outputs/tcp.md)
  - [udp](./outputs/udp.md)
  - [unix_socket](./outputs/unix-socket.md)
  - [stdout](./outputs/stdout.md)
- [Processing](./processing/README.md)
  - [Expression Functions](./processing/functions.md)
  - [String Templates](./processing/templates.md)
  - [User-defined Processes](./processing/user-defined.md)
- [Pipelines](./pipelines/README.md)
  - [Routing](./pipelines/routing.md)
  - [drop and finish](./pipelines/drop-finish.md)
  - [Examples](./pipelines/examples.md)

# Operations

- [CLI](./operations/cli.md)
- [Debug Tap](./operations/tap.md)
- [Metrics](./operations/metrics.md)
- [Packaging](./operations/packaging.md)
- [systemd](./operations/systemd.md)
- [Migrating from rsyslog](./operations/migration.md)
- [Upgrading to 0.3](./operations/upgrade-0.3.md)

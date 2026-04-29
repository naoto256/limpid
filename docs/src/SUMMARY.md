# Summary

[Introduction](./introduction.md)
[Design Principles](./design-principles.md)

# Getting Started

- [Installation](./getting-started.md)
- [Tutorial](./tutorial.md)

# Configuration

- [DSL Syntax Basics](./dsl-syntax.md)
- [Main Configuration](./configuration.md)
- [Inputs](./inputs/README.md)
  - [syslog_udp](./inputs/syslog-udp.md)
  - [syslog_tcp](./inputs/syslog-tcp.md)
  - [syslog_tls](./inputs/syslog-tls.md)
  - [tail](./inputs/tail.md)
  - [journal](./inputs/journal.md)
  - [unix_socket](./inputs/unix-socket.md)
  - [otlp_http](./inputs/otlp-http.md)
  - [otlp_grpc](./inputs/otlp-grpc.md)
- [Outputs](./outputs/README.md)
  - [file](./outputs/file.md)
  - [http](./outputs/http.md)
  - [kafka](./outputs/kafka.md)
  - [tcp](./outputs/tcp.md)
  - [udp](./outputs/udp.md)
  - [unix_socket](./outputs/unix-socket.md)
  - [stdout](./outputs/stdout.md)
  - [otlp](./outputs/otlp.md)
- [Processing](./processing/README.md)
  - [User-defined Processes](./processing/user-defined.md)
  - [Process Design Guide](./processing/design-guide.md)
- [Functions](./functions/README.md)
  - [Built-in Functions](./functions/expression-functions.md)
  - [User-defined Functions](./functions/user-defined.md)
- [Pipelines](./pipelines/README.md)
  - [Routing](./pipelines/routing.md)
  - [drop, finish, and error](./pipelines/drop-finish-error.md)
  - [Examples](./pipelines/examples.md)
  - [Multi-host Pipeline Example](./pipelines/multi-host.md)
- [Snippet Library](./snippets/README.md)

# Protocol Notes

- [OTLP — design rationale](./otlp.md)

# Operations

- [CLI](./operations/cli.md)
- [Debug Tap](./operations/tap.md)
- [Schema Validation](./operations/schema-validation.md)
- [Metrics](./operations/metrics.md)
- [Error Log (DLQ)](./operations/error-log.md)
- [Packaging](./operations/packaging.md)
- [systemd](./operations/systemd.md)
- [Migrating from rsyslog](./operations/migration.md)

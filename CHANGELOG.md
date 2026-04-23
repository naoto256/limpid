# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and
runtime shape converge. After 1.0, changes will follow semver strictly.

## [Unreleased]

## [0.2.2] - 2026-04-24

### Added

- `limpidctl inject --replay-timing[=<factor>]` — replays events at their
  original timing using each event's top-level `timestamp` field. Accepts
  `realtime` (= `1x`) or a factor like `10x` / `0.2x`. Defaults to `1x`
  when given without a value. Requires `--json`.

### Documentation

- `docs/src/operations/tap.md` — cadence-faithful replay section with
  examples (default / 10x / 0.2x / realtime), `--json` requirement, and
  the explicit failure cases (missing or unparseable timestamp, invalid
  factor, backwards timestamp, wall-clock catch-up) so there is no
  hidden behaviour.
- `docs/src/operations/cli.md` — `--replay-timing` entry in the CLI
  quick reference.

## [0.2.1] - 2026-04-18

### Fixed

- `--test-pipeline` now loads `table { ... }` global blocks from the
  configuration. Previously it constructed an empty `TableStore`, which
  caused pipelines using `table_lookup` / `table_upsert` / `table_delete`
  to emit "unknown table" warnings in test mode only.

## [0.2.0] - 2026-04-17

### Added

- `limpidctl inject <input|output> <name>` — pushes raw lines into a
  named input's event channel, or directly into an output's queue
  (bypassing pipelines entirely). Symmetric with `limpidctl tap`.
- `inject --json` — pushes full Event JSON (as emitted by `tap --json`),
  enabling `tap → inject` roundtrip for replay use cases.
- Control protocol: `inject <kind> <name> [json]`, EOF-terminated.
- Per-inject metrics: `events_injected` (for inputs and outputs) and
  `events_received` (for outputs).
- Prometheus exporter: three new counters (input injected, output
  injected, output received).

### Changed

- `limpidctl stats` output restructured to **Pipelines → Inputs →
  Outputs** ordering with updated counter set.

### Fixed

- `.gitignore` patterns to exclude common secrets layouts.
- `fold_by_precedence`: guard against empty operator lists.
- `tap.rs`: best-effort comment / error-path fixes surfaced by the
  v0.2.0 audit pass.

## [0.1.0] - 2026-04-17

Initial public release. Rust + tokio log pipeline daemon replacing
rsyslog / syslog-ng / fluentd with a single readable DSL (`def input`,
`def process`, `def output`, `def pipeline`). Includes syslog (UDP/TCP/
TLS) / tail / journal / unix socket inputs; file / HTTP / Kafka / TCP /
UDP / unix socket / stdout outputs; in-DSL expression language with
parsers (JSON / KV / CEF / syslog), regex, string templates, tables
with TTL, GeoIP; control socket (`limpidctl tap`, `stats`, `health`);
hot reload via `SIGHUP` with automatic rollback; per-output disk-backed
queues.

[Unreleased]: https://github.com/naoto256/limpid/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/naoto256/limpid/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/naoto256/limpid/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/naoto256/limpid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naoto256/limpid/releases/tag/v0.1.0

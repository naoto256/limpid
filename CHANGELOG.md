# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and
runtime shape converge. After 1.0, changes will follow semver strictly.

## [Unreleased]

## [0.4.0] - 2026-04-24

Testability release. Builds the static analyzer and observability
tooling on top of the DSL finalised in v0.3.0. No DSL breaking changes
— `limpid --check` does more, pipelines behave the same.

### Added — `limpid --check` static analyzer

- Full type-aware analyzer lives in `crates/limpid/src/check/` and
  runs whenever `limpid --check <config>` is invoked. It replaces the
  former "syntax OK" pass with real dataflow and type checking.
- Phase 2 type checking: `FieldType` + `Bindings` thread structural
  types through pipelines; function argument / return type signatures
  (`FunctionSig`), assignment type conflicts, operator type checks, and
  parser-function return shapes are all verified.
- Parser functions (`parse_json`, `parse_kv`, `syslog.parse`,
  `cef.parse`, `regex_parse`) declare the workspace keys they produce
  via `ParserInfo`; downstream references to those keys are verified.
- Phase 3 UX: diagnostics are rendered rustc-style with source snippet
  + caret, "did you mean" Levenshtein suggestions for unknown
  identifiers / functions, and clear summary + footer lines.
- Expr-level span: diagnostics carry precise source spans from
  expression nodes (not just statements), so the caret points at the
  offending sub-expression (`lower(workspace.count)` → carets the arg).
- `include "<glob>";` in configs is expanded by the analyzer with a
  cycle-safe source map, and summary counts (input / output / process /
  pipeline) are emitted per check.
- Footer: clean configs end with
  `<path>: Configuration OK (N pipeline(s), M process(es); dataflow check passed)`;
  configs with warnings include the warning count; configs with errors
  exit 1 with `error: N error(s) found`.

### Added — CLI flags

- `--strict-warnings`: promotes warning count to exit-2 (diagnostic
  level stays warning). CI-friendly switch for "warnings are failures."
- `--ultra-strict`: promotes **unknown-identifier** warnings to errors
  (exit 1). Distinct axis from `--strict-warnings` — this one changes
  the diagnostic level, not just the exit code. The two flags compose:
  unknown idents become errors, other warnings can still trigger
  exit-2. Category is tagged via `DiagKind`; `UnknownIdent` is the
  currently promoted class.
- `--graph[=<format>]`: emits a structural view of every pipeline to
  stdout. Formats: `mermaid` (default, GitHub-renderable),
  `dot` (Graphviz), `ascii` (terminal-only tree). Analyzer output stays
  on stderr so `--graph | pbcopy` etc. works cleanly.

### Added — documentation

- `docs/src/operations/schema-validation.md` — operations guide for
  schema validation. Covers the design decision to not ship an in-tree
  validator, the `limpidctl tap --json | <validator>` recipe (OCSF /
  ECS / custom JSON Schema), and the alternatives that were rejected
  (in-tree validator, DSL schema annotations, runtime per-event
  checking). Cross-linked from `operations/tap.md`.

### Changed — internals

- `Module::schema()` removed. Input / output modules no longer declare
  a data contract: they are I/O-pure (bytes in / bytes out) and have
  nothing to advertise. Schema information is carried by
  `FunctionSig` / `ParserInfo` on the function registry, which is where
  the analyzer looks. `modules/schema.rs` now only exports the
  `FieldType` / `FieldSpec` vocabulary.
- AST `Expr` became a wrapper struct (`Expr { kind: ExprKind, span }`)
  to carry per-expression spans without rewriting every pattern match.
- Unused `name_span` / `key_span` fields on def / property AST nodes
  (left as `#[allow(dead_code)]` placeholders) were removed; they can
  come back if a future analyzer phase needs them.
- Diagnostic category is routed via `DiagKind` enum (`UnknownIdent` /
  `TypeMismatch` / `Dataflow` / `Other`) instead of message-string
  heuristics, so category rendering and `--ultra-strict` promotion
  share the same source of truth.

### Security / hardening

- Snippet renderer sanitises ASCII control bytes (0x00–0x1F minus `\t`,
  and 0x7F) to `?` before writing the source line to stderr. Prevents
  ANSI OSC/CSI injection through config contents displayed in a
  reviewer's terminal.
- `include "<glob>";` is now confined to the config's root directory.
  Absolute paths and `..` traversal outside that root are rejected with
  a clear error. Prevents an include line from silently pulling in
  arbitrary files (`/etc/passwd`, `~/.ssh/*` etc.) or from leaking the
  first bytes of such files via a pest parse error.

### Documentation fixes

- `limpidctl check` references in operations / pipelines / processing
  docs corrected to `limpid --check` (check lives in the daemon binary,
  not the CLI tool — this was the Block 1 decision during v0.3.0
  restructure, but the docs had drifted).

## [0.3.0] - 2026-04-24

DSL stabilization release. This is a broad pre-1.0 breaking change that
settles the Event model, function namespaces, and core shape so that
future work (analyzer polish, snippet library, transport expansion) can
build on a final-form DSL without further surface-level churn.

### Breaking — Event model renamed

- `Event.raw` → `Event.ingress` (immutable bytes received on this hop)
- `Event.message` → `Event.egress` (bytes written on the wire by the output)
- `Event.fields` → `Event.workspace` (pipeline-local scratch namespace)
- `tap --json` / `inject --json` key names follow the rename; existing
  dumped replay files need `sed` (see `docs/src/operations/upgrade-0.3.md`)

### Breaking — Event core is now schema-agnostic

- `Event.facility` / `Event.severity` removed. These were syslog-specific
  metadata masquerading as pipeline-wide state; in a world where OTLP /
  OCSF / vendor JSON are first-class citizens, they do not belong in the
  Event core.
- DSL assignments `facility = N` / `severity = N` are now "unknown
  assignment target" errors. The PRI byte is constructed explicitly via
  the new `syslog.set_pri(egress, facility, severity)` function.
- `syslog.extract_pri(bytes)` returns the numeric PRI for reading.

### Breaking — Native process layer removed

- `modules/process/` is gone in its entirety. Pipeline statements like
  `process parse_syslog` no longer resolve to built-ins — schema-specific
  parsers are DSL functions (`syslog.parse(ingress)` etc.) invoked as
  statements inside an inline `process { ... }` block, and format
  primitives (`parse_json`, `parse_kv`, `regex_replace`) are flat DSL
  functions.
- `prepend_source` / `prepend_timestamp` have no direct replacement; the
  upgrade guide shows the `+` / `strftime` rewrite.

### Added — dot-namespaced function call syntax

- `<namespace>.<fn>(args)` grammar. Schema-specific functions declare their
  identity in the name. `parse_syslog(raw)` / `parse_cef(raw)` /
  `strip_pri(msg)` become `syslog.parse(ingress)` / `cef.parse(ingress)` /
  `syslog.strip_pri(egress)`. Flat primitives (JSON/KV/regex/hash/table)
  keep the bare-name form.
- New functions: `syslog.set_pri`, `syslog.extract_pri`, `regex_parse`,
  `hostname()`.

### Added — `regex_parse(target, pattern)`

- Named-capture extraction with dotted capture names producing nested
  objects: `(?P<date.month>\\w{3})` merges into `workspace.date.month`.
  Returns `Object` (bare-statement merges into `workspace`) or `null`.
- `regex_extract` remains as the single-value extractor.

### Added — `let` bindings

- `let x = <expr>` inside a `def process { ... }` body. Process-local
  scratch that keeps `workspace` clean of intermediate values. Bare-ident
  resolution is `LocalScope → Event metadata → error`.

### Added — pipeline fan-in

- `input a, b, c;` accepts multiple comma-separated inputs feeding the
  same pipeline body. Motivation: HA syslog (two redundant feeds running
  the same dedup / transform pipeline) no longer requires copy-pasting
  the pipeline twice.

### Added — `${expr}` template interpolation + string `+`

- `"prefix-${workspace.foo}-suffix"` interpolates any DSL expression.
  Old `%{name}` shorthand in `format()` has been removed; placeholders
  must be either reserved event names (`ingress`, `egress`, `source`,
  `timestamp`, `severity`, `facility`) or explicit `workspace.xxx` /
  `let`-bound names.
- `+` operator concatenates strings (falls back to arithmetic for
  numeric operands).

### Added — `strftime`, `hostname`

- `strftime(timestamp, format, tz?)` formats an RFC 3339 timestamp.
- `hostname()` returns the daemon's system hostname; portable configs
  can use `"${hostname()}"` in templates instead of hardcoding.

### Added — `output file` path templates via DSL evaluator

- `output file { path "/var/log/${source}/${strftime(timestamp, \"%Y-%m-%d\")}.log" }`
  evaluates the DSL expression per event instead of going through the
  legacy string template.

### Added — Design Principles page

- `docs/src/design-principles.md` publishes the five principles that
  govern limpid's scope (zero hidden behavior, I/O purity, domain
  knowledge as DSL snippets, only `egress` crosses hops, schema
  identity via namespaces).

### Added — developer / example docs

- `docs/src/processing/design-guide.md` — process design guide for
  contributors writing snippet library entries.
- `docs/src/pipelines/multi-host.md` — end-to-end worked example of a
  edge-host → relay → AMA multi-host pipeline, highlighting how
  the `tap` / `inject` primitives and the RFC 5424 hop contract turn a
  distributed pipeline into something you can reason about from one
  config.

### Changed — function code organization

- `crates/limpid/src/functions/` is now a tree of one-file-per-function
  modules: `primitives/` (flat), `syslog/` (dot namespace), `cef/`
  (dot namespace). The old `mod.rs` megafile is gone.
- Module trait introduced (`crates/limpid/src/modules/mod.rs`):
  `Module: Sized { fn schema() -> ModuleSchema; fn from_properties(...) }`.
  Replaces the former `FromProperties`. `schema()` is unused in-tree
  today but reserved for the upcoming analyzer (v0.4.0).

### Changed — hardening

- `limpid` and `limpidctl` restore `SIG_DFL` for SIGPIPE, so piped
  output (`limpidctl stats | head`) exits cleanly instead of panicking.
- `output http`: emits a `WARN` log when `verify false` disables TLS
  certificate validation, and the setting is documented as
  debugging-only.
- Control socket (`/var/run/limpid/control.sock`): max 8 concurrent
  connections, max 16 MiB per inject stream, max 4 KiB per command line.
- `syslog_tls` certificate and key loading moved off the async runtime
  via `spawn_blocking` to avoid stalling the reactor at startup.
- `fmt: cargo fmt --all` applied once across the tree so subsequent
  diffs are free of cosmetic noise.

### Internal refactors

- `<PRI>` header parsing consolidated into a single `parse_leading_pri`
  helper (was duplicated across `strip_pri`, `extract_pri`, `set_pri`).
- `values_equal` merged into `values_match` as the single equality
  routine for both `==`/`!=` and `switch` arms.
- TCP and Unix-socket outputs share a `PersistentConn` trait encoding
  the common "connect on first write, reconnect on broken pipe" pattern.
- `tls::build_client_config` (speculative dead code) removed; TLS client
  support will be reintroduced when an output needs it.

### Removed

- `modules/process/` (entire directory) and the `ModuleRegistry`
  process API (`register_process` / `call_process` / `process_names` /
  `ProcessFn`).
- `%{name}` shorthand in `format()` templates.
- `FromProperties` trait (absorbed into `Module`).

### Migration

See `docs/src/operations/upgrade-0.3.md` for end-to-end migration
recipes including `sed` snippets for the Event model rename, the
function rename table, and worked examples of replacing every removed
native process with its DSL function equivalent.

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

[Unreleased]: https://github.com/naoto256/limpid/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/naoto256/limpid/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/naoto256/limpid/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/naoto256/limpid/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/naoto256/limpid/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/naoto256/limpid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naoto256/limpid/releases/tag/v0.1.0

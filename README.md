# limpid

[![CI](https://github.com/naoto256/limpid/actions/workflows/ci.yml/badge.svg)](https://github.com/naoto256/limpid/actions/workflows/ci.yml)
[![Release](https://github.com/naoto256/limpid/actions/workflows/release.yml/badge.svg)](https://github.com/naoto256/limpid/actions/workflows/release.yml)
[![GitHub release](https://img.shields.io/github/v/release/naoto256/limpid?sort=semver&display_name=tag)](https://github.com/naoto256/limpid/releases/latest)
[![Dependencies](https://deps.rs/repo/github/naoto256/limpid/status.svg)](https://deps.rs/repo/github/naoto256/limpid)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Log pipelines, limpid as intent.**

- *Found out what your pipeline dropped only because the destination's dashboard went quiet?*
- *Paged at 3 a.m. because a config typo crashed the daemon — and there's no rollback?*
- *Waiting weeks on a plugin release because a vendor added a field?*

limpid is for you.

It is a log pipeline daemon where most of the work is *picking which
pieces to use*.

Suppose you want to ship FortiGate firewall logs to a security data
lake in OCSF format. With limpid, that is just chaining three things:

```limpid
def pipeline fortigate_to_security_lake {
    input   fortigate_syslog
    process parse_fortigate_cef | compose_ocsf
    output  security_lake
}
```

The flow is right there in the config. Bytes arrive on `fortigate_syslog`;
`parse_fortigate_cef` extracts structured fields into the canonical
`workspace.limpid.*` intermediate; `compose_ocsf` dispatches on
`workspace.limpid.class_uid` and emits the matching OCSF JSON; the
result leaves through `security_lake`. No hidden behavior. No plugin
to install. No separate "transform" config.

In limpid, anything you want to do to a log on its way from input to
output is achieved by freely combining `process`es.

### So what is a `process`?

A reusable chunk of pipeline logic — small, named, drop-in. You write
them yourself, or you include them from the snippet library (a curated
collection introduced in **v0.7.0** and expanded across the 0.7.x
line: 24 vendor parsers (SIEM + OSS NDR), 2
transport parsers, the OCSF 1.3.0 27-class composer, and shared
helper functions; full list in
[Snippet Library](docs/src/snippets/README.md)). Here is what an OCSF
Detection Finding composer leaf looks like under the hood:

```limpid
def process compose_ocsf_detection_finding {
    let activity = workspace.limpid.activity_id
    egress = to_json(null_omit({
        class_uid:    2004,                     // Detection Finding
        category_uid: 2,                        // Findings
        activity_id:  activity,
        type_uid:     2004 * 100 + activity,
        time:         coalesce(workspace.limpid.time, received_at),
        severity_id:  workspace.limpid.severity_id,
        // ...
    }))
}
```

Each `def process` is one small responsibility — parse one vendor,
shape one schema, drop one class of events. A pipeline is a chain of
them, separated by `|`, written in the same DSL whether you authored
the piece yourself or pulled it from the library.

The day you need to ship Cisco ASA logs to the same destination, you
write `parse_cisco_asa` and reuse `compose_ocsf_finding` unchanged. The
day you want to drop debug-level events before they leave, you slot in
a `drop_debug` ahead of the chain. The day a vendor adds a field, you
edit the parser snippet and `SIGHUP`. Each change is a swap, an
insertion, or an edit on one named piece — never a rewrite of the
whole pipeline.

## Why this is different

A few we have already covered:

- **Composable pieces.** Pipelines are chains of small named processes
  — `parse_fortigate_cef | compose_ocsf | route_by_severity`. Each
  piece is one responsibility, swappable, and reusable across
  pipelines.

- **Durable recovery sink, built in.** `control { error_log "..." }`
  persists payloads that retry-exhaust or fail the shutdown flush —
  operators get the recovery guarantee without writing their own DLQ
  wiring. `--check --strict-warnings` enforces it on configs that
  need it.

- **Visible flow.** Read the config and you know what the pipeline
  does. No implicit parsers that fire because input "looks like JSON".
  No magic defaults. No plugin runtime layer that translates between
  versions.

- **Vendor parsers in your hands.** Vendor-specific logic (CEF
  parsing, FortiGate quirks, OCSF schema mapping) lives in `.limpid`
  snippets you edit on your timeline. A vendor adds a field — you fix
  it in one file and `SIGHUP`. No Ruby plugin ABI, no Rust recompile,
  no waiting on the daemon team.

And here is the half that should make you grin — daily operations the
alternatives simply cannot match, the kind of thing that changes how
you live with a log pipeline:

- **You can watch the pipeline work, live.** `limpidctl tap output
  security_lake --json` and events stream out as they leave for the
  destination — body, attributes, source IP, the whole Event. No pause,
  no traffic duplication, no second tool. Every pipeline is its own
  debugger.

  ```
  $ limpidctl tap output security_lake --json | jq -c '{src: .source, sev: .workspace.cef.severity_level, class: .workspace.ocsf.class_uid}'
  {"src":{"ip":"10.0.0.21","port":51234},"sev":3,"class":200401}
  {"src":{"ip":"10.0.0.21","port":51234},"sev":7,"class":200401}
  {"src":{"ip":"10.0.0.22","port":42100},"sev":2,"class":200401}
  ...
  ```


- **Edit. Save. Reload. Mistake? It rolls back.** `SIGHUP` validates
  the new config first. A typo, an unknown identifier, a missing
  include — the daemon refuses the new config, prints a diagnostic,
  keeps the existing runtime intact. A *valid* reload tears down the
  old runtime and rebinds — brief downtime for *new* connections; the
  old runtime drains established TCP/HTTP/gRPC connections and disk
  queues persist across the cycle (memory queues and in-flight events
  are best-effort drained). Iterating on production pipelines stops
  being scary.

- **Yesterday's traffic, today's config.** Capture an hour of real
  events with `tap --json`; edit the pipeline; replay through `inject
  --json`. Pipeline changes get validated against actual production
  shapes — not synthetic fixtures, not staging that drifted six months
  ago.

- **Mistyped a workspace field?** `limpid --check` catches it before
  the daemon starts: rustc-style diagnostic, line and column, *"did you
  mean `workspace.severity`?"*. No "deploy and find out". No 3am page
  from a config typo that compiled fine and silently dropped half the
  events.

  ```
  $ limpid --check --config /etc/limpid/limpid.conf
  error: unknown identifier `workspace.severty`
    --> /etc/limpid/limpid.conf:34:26
     |
  34 |     if workspace.severty == "high" {
     |        ^^^^^^^^^^^^^^^^^^ help: did you mean `workspace.severity`?
     |
     = note: defined in process `parse_fortigate` at line 12

  error: aborting due to 1 previous error
  ```


These come from [five design principles](docs/src/design-principles.md)
— *zero hidden behavior*, *I/O is dumb transport*, *only `egress`
crosses hops*, *atomic events through the pipeline*, and *safety and
operational transparency* — that are stated, defended, and held in
place by the analyzer.

## Quick start

```bash
cargo build --release -p limpid -p limpidctl -p limpid-prometheus

limpid --check --config /etc/limpid/limpid.conf     # static analysis
limpid --config /etc/limpid/limpid.conf             # run the daemon
```

Other useful flags during config development:

- `--check --strict-warnings` — promote analyzer warnings to errors
  (for example, missing `control { error_log }` on configs that depend
  on recovery).
- `--check --ultra-strict` — promote unknown-identifier warnings to
  errors. This is the only opt-in lint upgrade today; it is *not* a
  generic style-level fail-on-warn. For "any warning fails CI" use
  `--strict-warnings`.
- `--graph[=mermaid|dot|ascii]` — render the resolved pipeline graph
  for review or for pasting into a PR description.
- `--test-pipeline <name> --input '<json>'` — run a single Event
  through one named pipeline without binding any sockets.

See the [Getting Started guide](docs/src/getting-started.md) for
installation, .deb packaging, and systemd integration.

## What's in the box

### Inputs
`syslog_udp` · `syslog_tcp` (with optional TLS / mTLS) · `tail` ·
`journal` · `unix_socket` · `otlp_http` · `otlp_grpc`

### Outputs
`syslog_udp` · `syslog_tcp` (with optional per-peer TLS / mTLS) ·
`file` · `http` (with per-peer TLS / mTLS, round-robin across peers) ·
`kafka` (with optional TLS / mTLS / SASL) · `unix_socket` ·
`stdout` · `otlp_http` / `otlp_grpc` (with per-peer TLS / mTLS,
round-robin across peers)

### Upgrading from earlier versions

The `output tcp` and `output udp` modules have been renamed to
`output syslog_tcp` and `output syslog_udp` to match the input-side
naming. Configs using the old type names are rejected at startup; no
alias is retained.

The DSL surface for these outputs also changed: the top-level
`address` property carrying a `host:port` string (and `host` + `port`)
is removed in favour of `peer { host port }` (single destination) or
`peers { peer { ... } ... }` (round-robin across multiple
destinations). See `CHANGELOG.md` for the full diff and
`docs/src/outputs/syslog-tcp.md` (and `-udp.md`) for the new shape.

`output syslog_tcp` accepts a per-peer `tls` block (inline or
named-profile reference) so plaintext and TLS destinations can share
one peer list. Default port flips per peer: 6514 (RFC 5425) when `tls`
is set on that peer, 514 (RFC 6587) otherwise. Optional client mTLS
via the same block (`cert` / `key` / `ca`). The standalone
`output syslog_tls` module that shipped briefly in 0.7.4 is removed in
0.7.6 — see `CHANGELOG.md` for migration.

`input syslog_tcp` accepts the same optional `tls { cert key ca }`
block for TLS termination on the listener side (`ca` enables mTLS
client-cert verification). Default port flips with the block: 6514
when TLS is configured, 514 otherwise. The standalone
`input syslog_tls` module is removed in 0.7.6 — see `CHANGELOG.md`
for migration. (`input otlp_http` gains the same `tls { ... }` block
in the same release; `input otlp_grpc` already had it.)

`output kafka` gains optional `tls { ca cert key }` and
`sasl { mechanism username password_file }` blocks. With both
configured the producer talks SASL/SCRAM (or PLAIN) over TLS — the
standard production combination. `cert + key` in `tls` enables mTLS;
`password_file` (not inline `password`) is the only supported way to
pass SASL credentials, matching the on-disk-with-chmod-600 pattern
already used for TLS private keys. No per-peer rotation: librdkafka's
`brokers` bootstrap list already handles broker discovery and leader
failover internally. Building with `--features kafka` now requires
`libssl-dev` and `libsasl2-dev` on Debian/Ubuntu (the cmake-built
librdkafka links against them at compile time).

The single `output otlp { protocol grpc | http_* }` module is split
in 0.7.6 into two transport-specific modules: `output otlp_http`
(keeps the `protocol http_protobuf|http_json` selector) and
`output otlp_grpc` (no `protocol`, gRPC is one wire format). The
single top-level `endpoint` property is replaced by a
`peers { peer { endpoint tls{...} } ... }` block on both modules —
flushes round-robin through the peers with per-peer cooldown on
failure, the standard production shape. Per-peer `tls { ca cert key }`
enables mTLS for either transport. See `CHANGELOG.md` for the
migration table.

`output http` gets the same treatment in 0.7.6 — the top-level `url`
property is replaced by `peer { url tls{...} }` (single destination
shorthand) or `peers { peer { url tls{...} } ... }` (round-robin
across multiple endpoints). Per-peer `tls { ca cert key }` enables
mTLS to the target. The `verify`, `method`, `content_type`,
`compress`, `headers`, `batch_size`, `batch_timeout` properties stay
top-level — they apply across all peers. See `CHANGELOG.md` for the
migration table.

### Snippets

Curated parser / composer / filter library, installed under
`/usr/share/limpid/snippets/` and `include`-able by absolute path.
Introduced in **v0.7.0**, with the transport layer split out as its
own snippet category in **v0.7.1** and the vendor lineup expanded
across the 0.7.x line:

- **Transport parsers (2, v0.7.1)** — `parse_syslog` (RFC 3164 /
  5424 syslog wire) · `parse_journald` (systemd journald JSON).
  These populate `workspace.<transport>.*` and feed any vocabulary
  parser downstream via an inline bridge.
- **Vendor parsers (24)** — security devices / cloud audit:
  `parse_fortigate_cef` · `parse_fortigate_syslog` ·
  `parse_paloalto_cef` · `parse_paloalto_syslog` · `parse_asa` ·
  `parse_cloudtrail` · `parse_juniper_srx_sd_syslog` (Junos
  structured-data) · `parse_juniper_srx_syslog` (Junos
  unstructured RT_IDP) · `parse_checkpoint_leef` (LEEF 2.0 /
  QRadar) · `parse_checkpoint_syslog` (Check Point Syslog
  Exporter, real-corpus verified) · `parse_nsp` (Trellix
  Network Security Platform, real-traffic verified). OSS NDR:
  `parse_suricata` (EVE JSON) · `parse_zeek_default` /
  `parse_zeek_soc` / `parse_zeek_full` (Zeek 8 / 20 / 43
  protocol scripts, nested-superset scopes, with `_native` /
  `_flat` convenience variants for raw Zeek vs Filebeat-flat
  upstream). Server / host vocabulary: `parse_openssh` ·
  `parse_sudo` · `parse_combined_log` (Apache / Nginx) ·
  `parse_postfix` · `parse_winevent_json` · `parse_sysmon` ·
  `parse_bind` · `parse_auditd` (7 OCSF classes, real-corpus
  verified). Vendor-neutral: `parse_ocsf`.
- **Composers (3)** — `compose_ocsf` (OCSF 1.3.0 priority set, 27
  classes, dispatched by `workspace.limpid.class_uid`) ·
  `compose_rfc5424` (journald → RFC 5424 wire, v0.7.1) ·
  `compose_replayable` (replay-shape capture).
- **Filters (1)** — `filter_openssh_journal` (drops PAM session
  double-count noise from journald sshd streams).

Each parser writes to the canonical `workspace.limpid.*`
intermediate; `compose_ocsf` reads from it and emits OCSF JSON to
`egress`. Two `include` lines + a two-stage pipeline gets vendor
logs into a SIEM / data lake in OCSF form. Full reference:
[Snippet Library](docs/src/snippets/README.md).

### Functions

There are several types of expression functions you can call from
inside a `process` body:

- **Generic parsers** — `parse_json` · `parse_kv` · `csv_parse` ·
  `nest_dotted_keys` (lift flat dotted keys from Zeek / Filebeat-style
  inputs into a nested object)
- **Regex** — `regex_match` · `regex_extract` · `regex_parse` ·
  `regex_replace`
- **String predicates** — `contains` · `starts_with` · `ends_with`
- **String manipulation** — `lower` · `upper` · `strftime` · `strptime`
- **Datetime parsers** — `parse_datetime_rfc3339` · `parse_datetime_rfc2822`
- **Type coercion** — `to_int` · `to_json` · `to_bytes` · `to_string`
- **Fallback / shaping** — `coalesce` · `null_omit`
- **Collections** — `map` · `filter` · `find` · `reduce` · `first` ·
  `last` · `concat` · `distinct` · `sum` · `max` · `min` ·
  `entitle` · `path` · `append` · `prepend` · `len` · `is_array`
- **Hashing** — `md5` · `sha1` · `sha256`
- **Tables / enrichment** — `table_lookup` · `table_upsert` ·
  `table_delete` · `geoip`
- **Environment** — `hostname` · `version` · `timestamp`
- **Syslog** — `syslog.parse` · `syslog.strip_pri` · `syslog.set_pri` ·
  `syslog.extract_pri`
- **CEF** — `cef.parse`
- **OTLP** — `otlp.encode_resourcelog_protobuf` ·
  `otlp.decode_resourcelog_protobuf` · `otlp.encode_resourcelog_json` ·
  `otlp.decode_resourcelog_json`

Full reference: [Built-in Functions](docs/src/functions/expression-functions.md)
· [String interpolation](docs/src/dsl-syntax.md#string-interpolation).

## Performance

A single core handles **~168k events/sec** on the heaviest realistic
DSL workload — full OCSF Authentication compose with `to_json`
serialization, single-pipeline single-input, channel-direct injection.
Lighter shapes scale up from there:

| Pipeline shape                              | events/sec/core |
|---------------------------------------------|----------------:|
| passthrough                                 |             312k |
| `syslog.parse(ingress)`                     |             305k |
| parse + 2× regex + if/else                  |             115k |
| **OCSF compose + to_json (heaviest)**       |         **168k** |

Multi-pipeline configurations scale across cores via Tokio's
multi-thread runtime: 4 independent pipelines (each its own input,
process chain, and output) reach ~459k events/sec aggregate on the
OCSF compose workload — 2.7× the single-pipeline number on a 16-core
host with no application-level work-stealing or pinning.

The numbers come from the v0.6.0 perf milestone (per-event bump arena,
direct `serde::Serialize` for the runtime `Value` tree, static-literal
hash-key interning, and a boundary refactor that eliminated the
hot-path `BorrowedEvent::to_owned()` at every output sink) and the
v0.6.1 follow-up (per-worker bump-arena recycling, lifting the macOS
`xzm` zone-lock contention that capped multi-pipeline scaling). Real
I/O (`__sendto`) and tokio scheduling are now the dominant categories
on the flame graph; allocation collapsed from 43% at v0.5.7 to 15% on
the single-pipeline path. See the [CHANGELOG](CHANGELOG.md) for the
cumulative breakdown.

## Compared to rsyslog / fluentd / Vector

A capability snapshot versus the established log forwarders. Where a
cell says "—" the capability is absent; where it says something else,
that is roughly how that tool addresses the same axis.

| | rsyslog | fluentd | Vector | **limpid** |
|---|---|---|---|---|
| **Pre-deploy config check** | — | — | `vector validate` | rustc-style type checker |
| **Live event tap (any hop)** | — | — | `vector tap` | `limpidctl tap` |
| **Replay captured traffic** | — | — | — | `inject --json` |
| **Hot reload safety** | SIGHUP, no rollback | SIGHUP, fragile | SIGHUP, validates first | SIGHUP atomic, rollback on failure |
| **Vendor parsers** | C modules | Ruby plugins | DSL transforms (VRL) | DSL snippets (`include`-able) |
| **OTLP first-class** | — | plugin | yes | yes (input + output, 3 transports) |
| **Runtime** | C | Ruby + C | Rust | Rust |

The point is not that the alternatives are bad — they have decades of
hardened, large-scale deployment behind them. The point is that limpid
is built for a different default: pipelines that are *legible*,
*verifiable*, and *operable* without a second tool.

## Documentation

- [Introduction](docs/src/introduction.md) ·
  [Design Principles](docs/src/design-principles.md)
- [Getting Started](docs/src/getting-started.md) ·
  [Configuration](docs/src/configuration.md)
- [Inputs](docs/src/inputs/README.md) ·
  [Outputs](docs/src/outputs/README.md) ·
  [Processing](docs/src/processing/README.md)
- [Process Design Guide](docs/src/processing/design-guide.md) ·
  [User-defined Processes](docs/src/processing/user-defined.md)
- [Functions](docs/src/functions/README.md) ·
  [Built-in Functions](docs/src/functions/expression-functions.md) ·
  [User-defined Functions](docs/src/functions/user-defined.md)
- [Pipelines](docs/src/pipelines/README.md) ·
  [Routing](docs/src/pipelines/routing.md) ·
  [`drop`, `finish`, and `error`](docs/src/pipelines/drop-finish-error.md) ·
  [Examples](docs/src/pipelines/examples.md) ·
  [Multi-host Pipeline Example](docs/src/pipelines/multi-host.md)
- [CLI](docs/src/operations/cli.md) ·
  [Debug Tap](docs/src/operations/tap.md) ·
  [Schema Validation](docs/src/operations/schema-validation.md) ·
  [Metrics](docs/src/operations/metrics.md) ·
  [Packaging](docs/src/operations/packaging.md) ·
  [systemd](docs/src/operations/systemd.md)
- [OTLP — design rationale](docs/src/otlp.md)
- [Migrating from rsyslog](docs/src/operations/migration.md)

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.

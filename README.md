# limpid

[![CI](https://github.com/naoto256/limpid/actions/workflows/ci.yml/badge.svg)](https://github.com/naoto256/limpid/actions/workflows/ci.yml)
[![Release](https://github.com/naoto256/limpid/actions/workflows/release.yml/badge.svg)](https://github.com/naoto256/limpid/actions/workflows/release.yml)
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
    process parse_fortigate | compose_ocsf_finding
    output  security_lake
}
```

The flow is right there in the config. Bytes arrive on `fortigate_syslog`;
`parse_fortigate` extracts structured fields; `compose_ocsf_finding`
shapes those fields into an OCSF Detection Finding; the result leaves
through `security_lake`. No hidden behavior. No plugin to install. No
separate "transform" config.

In limpid, anything you want to do to a log on its way from input to
output is achieved by freely combining `process`es.

### So what is a `process`?

A reusable chunk of pipeline logic — small, named, drop-in. You write
them yourself, or you include them from the snippet library (a curated
collection that debuts in **v0.7.0**: 11 vendor parsers, the OCSF
1.3.0 27-class composer, and the replay-shape composer; full list in
[Snippet Library](docs/src/snippets/README.md)). Here is what an OCSF
Detection Finding composer leaf looks like under the hood:

```limpid
def process compose_ocsf_finding {
    workspace.ocsf = {
        category_uid: 2,                        // Findings
        class_uid:    200401,                   // Detection Finding
        time:         workspace.cef.rt,
        severity_id:  workspace.cef.severity_level,
        // ...
    }
    egress = to_json(workspace.ocsf)
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
  — `parse_fortigate | compose_ocsf_finding | route_by_severity`. Each
  piece is one responsibility, swappable, and reusable across
  pipelines.

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


- **Edit. Save. Reload. Mistake? It rolls back.** `SIGHUP` swaps the
  config atomically. A typo, an unknown identifier, a missing include —
  the daemon refuses the new config, prints a diagnostic, keeps the
  previous one running. Iterating on production pipelines stops being
  scary.

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

See the [Getting Started guide](docs/src/getting-started.md) for
installation, .deb packaging, and systemd integration.

## What's in the box

### Inputs
`syslog_udp` · `syslog_tcp` · `syslog_tls` · `tail` · `journal` ·
`unix_socket` · `otlp_http` · `otlp_grpc`

### Outputs
`file` · `http` · `kafka` · `tcp` · `udp` · `unix_socket` · `stdout` ·
`otlp`

### Functions

There are several types of expression functions you can call from
inside a `process` body:

- **Generic parsers** — `parse_json` · `parse_kv` · `csv_parse`
- **Regex** — `regex_match` · `regex_extract` · `regex_parse` ·
  `regex_replace`
- **String predicates** — `contains` · `starts_with` · `ends_with`
- **String manipulation** — `lower` · `upper` · `strftime` · `strptime`
- **Type coercion** — `to_int` · `to_json` · `to_bytes` · `to_string`
- **Fallback / shaping** — `coalesce` · `null_omit`
- **Collections** — `len` · `find_by` · `append` · `prepend`
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
- [Migrating from rsyslog](docs/src/operations/migration.md) ·
  [Upgrading to 0.3](docs/src/operations/upgrade-0.3.md)

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.

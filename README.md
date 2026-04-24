# limpid

**Log pipelines, limpid as intent.**

A log pipeline daemon that replaces rsyslogd, syslog-ng, and fluentd with a single, readable DSL. You declare inputs, processes, outputs, and pipelines — and the config reads like what it does.

## Why limpid?

- **One readable DSL** — inputs, outputs, processing, and routing in the same language, no template files or per-module YAML
- **Type-aware `--check`** — rustc-style diagnostics with "did you mean" suggestions and a Mermaid/DOT flow graph (v0.4.0)
- **Observability-first** — `tap`, `inject`, and `--test-pipeline` let you watch every hop of the pipeline
- **Five [design principles](docs/src/design-principles.md)** — starting from *zero hidden behaviour*; codified in v0.3.0 and upheld by the analyzer
- **Hot reload with rollback** — `SIGHUP` swaps configuration atomically; a failed reload keeps the old one running
- **pre-1.0, shape-stable** — the DSL shape converged in v0.3.0 and has been breaking-change-free since

## Quick start

```bash
cargo build --release -p limpid -p limpidctl -p limpid-prometheus

limpid --check --config /etc/limpid/limpid.conf     # static analysis
limpid --config /etc/limpid/limpid.conf             # run the daemon
```

See the [Getting Started guide](docs/src/getting-started.md) for installation, packaging, and systemd integration.

## What it looks like

```
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/${source}/${strftime(timestamp, "%Y-%m-%d", "local")}.log"
}

def output siem {
    type http
    url "https://es:9200/_bulk"
    batch_size 100
}

def pipeline security {
    input fw
    process {
        cef.parse(ingress)
    }
    // CEF severity 0-3 = High/Very-High → forward to SIEM
    if workspace.cef_severity_level != null and workspace.cef_severity_level <= 3 {
        output siem
    }
    output archive
}
```

There is no separate "process layer": what v0.2 expressed as built-in processes (`parse_cef`, `prepend_source`, …) is now a direct function call inside an inline `process { … }` block, or a user-defined `def process`. See the [Process Design Guide](docs/src/processing/design-guide.md) and [pipeline examples](docs/src/pipelines/examples.md) for the idioms.

## What's in the box

### Inputs
`syslog_udp` · `syslog_tcp` · `syslog_tls` · `tail` · `journal` · `unix_socket`

### Outputs
`file` · `http` · `kafka` · `tcp` · `udp` · `unix_socket` · `stdout`

### Expression primitives (flat)
`parse_json` · `parse_kv` · `regex_extract` · `regex_match` · `regex_replace` · `regex_parse` · `strftime` · `format` · `contains` · `lower` · `upper` · `to_json` · `md5` · `sha1` · `sha256` · `table_lookup` · `table_upsert` · `table_delete` · `geoip`

### Schema-specific functions (dot-namespaced)
`syslog.parse` · `syslog.strip_pri` · `syslog.set_pri` · `syslog.extract_pri` · `cef.parse`

Full reference: [Expression Functions](docs/src/processing/functions.md) · [String Templates](docs/src/processing/templates.md).

## Check your config

```
limpid --check --config config.limpid
limpid --check --config config.limpid --strict-warnings  # warnings → exit 2
limpid --check --config config.limpid --ultra-strict     # unknown idents → errors
limpid --check --config config.limpid --graph            # Mermaid flow graph on stdout
limpid --check --config config.limpid --graph=dot        # Graphviz DOT instead
```

The analyzer does type inference, dataflow analysis, and Levenshtein-based suggestions, and prints rustc-style snippet-with-caret diagnostics. See [Schema Validation](docs/src/operations/schema-validation.md) and [CLI](docs/src/operations/cli.md) for details.

## Documentation

- [Introduction](docs/src/introduction.md) · [Design Principles](docs/src/design-principles.md)
- [Getting Started](docs/src/getting-started.md) · [Configuration](docs/src/configuration.md)
- [Inputs](docs/src/inputs/README.md) · [Outputs](docs/src/outputs/README.md) · [Processing](docs/src/processing/README.md)
- [Process Design Guide](docs/src/processing/design-guide.md) · [Expression Functions](docs/src/processing/functions.md) · [String Templates](docs/src/processing/templates.md) · [User-defined Processes](docs/src/processing/user-defined.md)
- [Pipelines](docs/src/pipelines/README.md) · [Routing](docs/src/pipelines/routing.md) · [`drop` and `finish`](docs/src/pipelines/drop-finish.md) · [Examples](docs/src/pipelines/examples.md) · [Multi-host Pipeline Example](docs/src/pipelines/multi-host.md)
- [CLI](docs/src/operations/cli.md) · [Debug Tap](docs/src/operations/tap.md) · [Schema Validation](docs/src/operations/schema-validation.md) · [Metrics](docs/src/operations/metrics.md) · [Packaging](docs/src/operations/packaging.md) · [systemd](docs/src/operations/systemd.md)
- [Migrating from rsyslog](docs/src/operations/migration.md) · [Upgrading to 0.3](docs/src/operations/upgrade-0.3.md)

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

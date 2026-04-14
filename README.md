# limpid

**Log pipelines, limpid as intent.**

A log pipeline daemon that replaces rsyslogd, syslog-ng, and fluentd with a single, readable DSL. You define inputs, processes, outputs, and pipelines — and the config reads like what it does.

## Why limpid?

- **One DSL for everything** — inputs, routing, transforms, outputs, all in the same language
- **Pipelines you can read** — no template strings, no regex escapes in config, no hidden behavior
- **Hot reload** — `SIGHUP` reloads configuration with automatic rollback on failure
- **Debug tap** — stream events from any input, process, or output in real time

## Quick start

```bash
cargo build --release -p limpid -p limpid-tap -p limpid-prometheus
limpid --check --config /etc/limpid/limpid.conf
limpid --config /etc/limpid/limpid.conf
```

See the [Getting Started guide](docs/src/getting-started.md) for full installation instructions.

## What it looks like

```
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/${source}/${date}.log"
}

def output siem {
    type http
    url "https://es:9200/_bulk"
    batch_size 100
}

def pipeline security {
    input fw
    process parse_cef
    if severity <= 3 {
        output siem
    }
    output archive
}
```

## Modules

### Inputs

`syslog_udp` · `syslog_tcp` · `syslog_tls` · `tail` · `journal` · `unix_socket`

### Outputs

`file` · `http` · `kafka` · `tcp` · `udp` · `unix_socket` · `stdout`

### Processes

`parse_cef` · `parse_json` · `parse_syslog` · `parse_kv` · `strip_pri` · `prepend_source` · `prepend_timestamp` · `regex_replace`

### Functions

`contains` · `lower` · `upper` · `regex_match` · `regex_extract` · `regex_replace` · `format` · `to_json` · `md5` · `sha1` · `sha256` · `lookup` · `geoip`

## Documentation

- [Getting Started](docs/src/getting-started.md)
- [Configuration](docs/src/configuration.md)
- [Inputs](docs/src/inputs/README.md) · [Outputs](docs/src/outputs/README.md) · [Processing](docs/src/processing/README.md)
- [Pipelines](docs/src/pipelines/README.md) · [drop and finish](docs/src/pipelines/drop-finish.md) · [Examples](docs/src/pipelines/examples.md)
- [CLI](docs/src/operations/cli.md) · [Debug Tap](docs/src/operations/tap.md) · [Metrics](docs/src/operations/metrics.md) · [Packaging](docs/src/operations/packaging.md) · [systemd](docs/src/operations/systemd.md)
- [Migrating from rsyslog](docs/src/operations/migration.md)

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

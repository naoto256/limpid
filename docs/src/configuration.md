# Main Configuration

Reference for the main configuration file: include directives, global blocks, and DSL syntax. For a walked-through build-up of a working pipeline, see the [Tutorial](./tutorial.md).

All limpid configuration is written in `.limpid` files using a custom DSL. No TOML, no YAML, no XML.

## limpid.conf

The main configuration file is specified via `--config`:

```bash
limpid --config /etc/limpid/limpid.conf
```

It contains `include` directives and global blocks (`geoip`, `control`, `table`). Module and pipeline definitions live in included files by convention, though nothing prevents putting them in the main file. The DSL surface for those definitions (literals, `def`, blocks, `${}` interpolation) is documented in [DSL Syntax Basics](./dsl-syntax.md).

## Include directives

```
include "inputs/*.limpid"          // glob
include "outputs/ama.limpid"       // single file
include "/usr/share/limpid/snippets/parsers/fortigate.limpid"   // shipped snippet
```

Rules:

- Relative paths resolve against the **including file's** directory.
- Absolute paths are rejected, **except** under `/usr/share/limpid/snippets/` (the shipped snippet library — see [Snippet Library](./snippets/README.md)).
- Nested includes are supported — an included file may itself contain `include` directives. The same file is loaded only once even if multiple parents reference it (diamond-safe). Cycles are detected and reported as a parse error.
- Glob patterns are supported.

## Global blocks

### control

Configures the runtime control surface — the Unix socket used by `limpidctl` and the Prometheus exporter, plus the optional dead-letter queue file.

```
control {
    socket    "/var/run/limpid/control.sock"
    error_log "/var/log/limpid/errored.jsonl"
}
```

| Property | Default | Effect |
|----------|---------|--------|
| `socket` | `/var/run/limpid/control.sock` | Unix socket path consumed by `limpidctl` and `limpid-prometheus`. |
| `error_log` | *(unset)* | JSONL file appended to when a `process` raises a runtime error. When unset, the runtime falls back to a structured `tracing::error!` line so the failure data is never silently lost. See [Error Log (DLQ)](./operations/error-log.md) for the record format and replay recipe. |

The whole block is optional — daemon starts with the defaults if it's omitted.

### table

Defines in-memory key-value tables for use with `table_lookup()`, `table_upsert()`, and `table_delete()`.

```
table {
    // Static table loaded from file (read-write)
    asset {
        load "/etc/limpid/tables/asset.json"
    }

    // Dynamic table with TTL and size limit
    seen {
        max 100000
        ttl 3600
    }
}
```

| Property | Description |
|----------|-------------|
| `load` | Load initial data from a JSON or CSV file. Loaded entries have no TTL. |
| `max` | Maximum number of entries. Oldest evicted when exceeded. Default: no limit. |
| `ttl` | Default TTL in seconds for `table_upsert`. Default: no expiry. |

Tables are in-memory only. There is no `table_save` — files are initial seeds, and dynamic data is lost on restart.

See [table functions](./processing/functions.md#table-functions) for usage.

### geoip

Enables the `geoip()` expression function for IP geolocation.

```
geoip {
    database "/usr/share/GeoIP/GeoLite2-City.mmdb"
}
```

Requires a [MaxMind GeoLite2 City database](https://dev.maxmind.com/geoip/geolite2-free-geolocation-data).

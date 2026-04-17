# Main Configuration

All limpid configuration is written in `.limpid` files using a custom DSL. No TOML, no YAML, no XML.

## limpid.conf

The main configuration file is specified via `--config`:

```bash
limpid --config /etc/limpid/limpid.conf
```

It contains `include` directives and global settings:

```
include "inputs/*.limpid"
include "outputs/*.limpid"
include "processes/*.limpid"
include "pipelines/*.limpid"

geoip {
    database "/usr/share/GeoIP/GeoLite2-City.mmdb"
}

control {
    socket "/var/run/limpid/control.sock"
}

table {
    asset {
        load "/etc/limpid/tables/asset.json"
    }
    seen {
        max 100000
        ttl 3600
    }
}
```

## Include directives

`include` loads additional `.limpid` files. Glob patterns are supported.

```
include "inputs/*.limpid"          // all .limpid files in inputs/
include "outputs/ama.limpid"       // a specific file
```

Paths are resolved relative to the main config file's directory. Include directives are only allowed in the main config file — included files cannot themselves include other files.

## Directory layout

```
/etc/limpid/
├── limpid.conf            # Main config: includes + global settings
├── inputs/
│   └── syslog.limpid      # Input definitions
├── outputs/
│   └── ama.limpid          # Output definitions
├── processes/
│   └── enrich.limpid       # Process definitions
└── pipelines/
    └── main.limpid         # Pipeline definitions
```

This structure is a convention, not a requirement. You can organize files however you like — limpid just loads whatever the `include` directives point to.

## Global blocks

### geoip

Enables the `geoip()` expression function for IP geolocation.

```
geoip {
    database "/usr/share/GeoIP/GeoLite2-City.mmdb"
}
```

Requires a [MaxMind GeoLite2 City database](https://dev.maxmind.com/geoip/geolite2-free-geolocation-data).

### control

Configures the Unix socket for `limpidctl` and metrics.

```
control {
    socket "/var/run/limpid/control.sock"
}
```

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

See [Table functions](./processing/functions.md#table-functions) for usage.

## DSL syntax

- `def` keyword defines inputs, outputs, processes, and pipelines
- `//` for line comments
- Semicolons are **optional** — use them to separate statements on one line
- Strings use double quotes: `"hello"`
- Integers: `42`, `-1`
- Booleans: `true`, `false`
- Null: `null`
- Nested blocks use `{ ... }`

```
// Multi-line — no semicolons needed
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

// One-liner — semicolons for readability
def output fw01 { type file; path "/var/log/fw/fw01.log" }
```

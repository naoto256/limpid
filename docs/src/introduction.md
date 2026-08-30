# limpid

**Log pipelines, limpid as intent.**

limpid is a log pipeline daemon that replaces rsyslogd, syslog-ng, and fluentd with a single, readable DSL. You define inputs, processes, outputs, and pipelines — and the config reads like what it does.

## Why limpid?

rsyslog configs are cryptic. syslog-ng is verbose. fluentd needs plugins for everything. limpid gives you:

- **One DSL for everything** — inputs, routing, transforms, outputs, all in the same language
- **Pipelines you can read** — no template strings, no regex escapes in config, no hidden behavior (see [Design Principles](./design-principles.md))
- **`--test-pipeline` mode** — validate your pipeline logic with sample data before deploying
- **Non-terminal outputs** — send to multiple destinations without copy-plugin hacks
- **Fan-out by design** — multiple pipelines can share the same input, each with independent processing
- **Hot reload** — `SIGHUP` reloads configuration with automatic rollback on failure
- **Instant shutdown** — graceful SIGTERM handling with a fixed 10s shutdown deadline

## Architecture

```
Input → Process → Process → ... → output(copy) → Process → output(copy) → finish
                                       ↓                        ↓
                                    [Queue]                  [Queue]
                                       ↓                        ↓
                                    Output                   Output
```

- **Input** modules receive log messages (syslog, file tailing, journal, unix socket)
- **Process** modules transform events (parse, filter, enrich, rewrite)
- **Output** modules write events to destinations (file, TCP, UDP, HTTP, unix socket)
- **Pipelines** wire them together with routing logic (if/switch/drop/finish)

Each output has an async queue. Pipelines run synchronously (one event at a time), but outputs are decoupled via queues so downstream bottlenecks don't block the pipeline.

When an output exhausts its retries or shutdown can't drain the queue in time, the payload is persisted as JSONL to the daemon-wide `control { error_log "..." }` sink rather than being silently dropped. The recovery story is part of the daemon, not a per-output afterthought — see [Error Log (DLQ)](./operations/error-log.md).

## At a glance

```limpid
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/${source.ip}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def output siem {
    type http
    peer { url "https://es:9200/_bulk" }
    batch_size 100
}

control {
    error_log "/var/log/limpid/errored.jsonl"
}

def pipeline security {
    input fw
    process { workspace.cef = cef.parse(ingress) }
    output archive
    // CEF severity is a numeric-or-named union — forward High/Very-High and numeric 7-10.
    if workspace.cef.severity == "High" or workspace.cef.severity == "Very-High" or workspace.cef.severity >= 7 {
        output siem
    }
}
```

For a larger walkthrough across two hops (edge hosts shipping to a central relay, then on to a SIEM), see [Multi-host Pipeline Example](./pipelines/multi-host.md).

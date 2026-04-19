# limpid

**Log pipelines, limpid as intent.**

limpid is a log pipeline daemon that replaces rsyslogd, syslog-ng, and fluentd with a single, readable DSL. You define inputs, processes, outputs, and pipelines — and the config reads like what it does.

## Why limpid?

rsyslog configs are cryptic. syslog-ng is verbose. fluentd needs plugins for everything. limpid gives you:

- **One DSL for everything** — inputs, routing, transforms, outputs, all in the same language
- **Pipelines you can read** — no template strings, no regex escapes in config, no hidden behavior
- **`--test-pipeline` mode** — validate your pipeline logic with sample data before deploying
- **Non-terminal outputs** — send to multiple destinations without copy-plugin hacks
- **Fan-out by design** — multiple pipelines can share the same input, each with independent processing
- **Hot reload** — `SIGHUP` reloads configuration with automatic rollback on failure
- **Instant shutdown** — graceful SIGTERM handling with configurable timeout

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

## At a glance

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
    process parse_cef
    output archive
    if severity <= 3 {
        output siem
    }
}
```

# Routing

Pipelines route events to outputs by sequencing `output`, `process`, `if`, `switch`, `drop`, and `finish` statements. The syntactic forms of `if` / `switch` / `drop` / `finish` are documented in [DSL Syntax Basics → Control flow](../dsl-syntax.md#control-flow); this page covers what they *do* inside a pipeline.

## What flows through a pipeline

A pipeline body executes top-to-bottom for each event arriving on `input`. Statements have these effects:

- **`process <name>`** (or inline `process { ... }`) — runs the named process body against the event, possibly mutating `workspace` / `egress`.
- **`output <name>`** — hands a deep-copy of the event to the named output's queue. **Non-terminal**: execution continues to the next statement, so the same event can flow into multiple outputs in order.
- **`if` / `switch`** — branches that conditionally execute the statements inside.
- **`drop`** — terminates routing for this event. Subsequent `output` / `process` statements do not run, and the event is counted as `events_dropped`.
- **`finish`** — terminates routing for this event without dropping. Subsequent statements do not run, but the event is counted as `events_finished` rather than dropped — use it when "we're done with this one, no error" rather than "we don't want this one".

## Examples

### Branch on workspace value

```
def pipeline main {
    input syslog

    process parse_severity   // sets workspace.severity
    if workspace.severity <= 3 {
        output alert
    }
    output siem
}
```

The `output alert` runs only when severity ≤ 3; `output siem` always runs (no `drop` / `finish` in the `if` body).

### Switch on source

```
def pipeline archive {
    input syslog_udp
    process { egress = syslog.strip_pri(egress) }

    switch source {
        "192.0.2.1" { output fw01 }
        "192.0.2.2" { output fw02 }
        "192.0.2.3" {
            if contains(ingress, "type=\"traffic\"") { drop }
            process { egress = source + " " + strftime(received_at, "%b %e %H:%M:%S") + " " + egress }
            output fw03
        }
        default { drop }
    }
}
```

### Multi-output (non-terminal `output`)

```
def pipeline main {
    input syslog

    // Archive raw bytes first
    output archive

    // Parse and rewrite egress
    process {
        workspace.cef = cef.parse(workspace.syslog.msg)
        workspace.geo = geoip(workspace.cef.src)
        egress = to_json(workspace)
    }

    // Send the enriched JSON downstream
    output siem
}
```

The `output archive` receives the original wire bytes; `output siem` receives the rewritten JSON. Each output sees the event at the moment its statement ran — modifications after a deep-copy boundary do not affect earlier branches.

For longer end-to-end recipes (firewall archival with source-based routing, AMA forwarding with disk queue, SIEM ingest with enrichment, FortiGate KV reformatting), see [Examples](./examples.md).

# Process Design Guide

This page is for people writing processes — your own `def process` blocks in a production config, or snippets intended for wider reuse (OCSF composers, SIEM-specific parsers, vendor normalizers shipping under `processes/*.limpid`).

It is a **style guide**, not a reference. The reference for what a process can express is [User-defined Processes](./user-defined.md); the reference for functions is [Expression Functions](./functions.md). The principles the guide rests on are in [Design Principles](../design-principles.md).

Everything here is about one thing: keeping processes small enough that a reader can hold one in their head, and composable enough that pipelines stay readable.

## The granularity rule

> One process does one thing. If you cannot name it in three or four words without `and`, it is doing too much.

Good process names describe a single abstraction:

- `strip_pri` — removes a syslog `<PRI>`.
- `parse_fortigate_kv` — parses FortiGate KV payloads.
- `enrich_with_geoip` — annotates `workspace.src` with GeoIP.
- `drop_healthchecks` — filters LB noise.
- `ama_rewrite` — rewrites the PRI byte for AMA facility routing.

Each of these is a verb phrase. Each has a single reason to exist. Each can be composed with `|` alongside others without its behavior depending on the neighbours.

Bad names give away the problem:

- `parse_and_enrich_and_filter` — three responsibilities.
- `process_event` — no abstraction, just "stuff happens here".
- `handle_fortigate` — vague enough that it will grow forever.

If you catch yourself writing `and` in a process name, split it:

```
// Don't: one process doing three things
def process parse_and_enrich_fortigate {
    syslog.parse(ingress)
    parse_kv(workspace.syslog_msg)
    workspace.geo = geoip(workspace.srcip)
    if contains(egress, "healthcheck") {
        drop
    }
}

// Do: three processes, each a single step, composed at the pipeline
def process parse_fortigate  { syslog.parse(ingress); parse_kv(workspace.syslog_msg) }
def process enrich_fortigate { workspace.geo = geoip(workspace.srcip) }
def process drop_healthchecks { if contains(egress, "healthcheck") { drop } }

def pipeline fw {
    input fw_syslog
    process parse_fortigate | drop_healthchecks | enrich_fortigate
    output siem
}
```

The split config is longer. It is also easier to test, easier to tap between steps (`limpidctl tap process parse_fortigate`), and easier to reuse when a second vendor needs the same GeoIP enrichment.

## Input and output contracts

Every process has an implicit contract with its neighbours in the pipeline: *what do I expect to be present when I run, and what do I leave behind for the next stage?*

Because the DSL does not (yet) check that contract statically, you document it in comments using a small, machine-parseable convention. limpid plans to grow a `limpid --check --strict` pass that reads these tags and warns when a pipeline links a composer to a parser that does not produce the required fields. Writing them now costs nothing; pretending the contracts do not exist costs the first person who has to modify the snippet a year later.

### The `@requires` / `@produces` tag convention

Put tags in the first block of comments inside the process body. One tag per line. Each tag names a field path in `workspace.*` (or, less commonly, `egress`).

```
def process ocsf_authentication_compose {
    // @requires: workspace.syslog_hostname (required)
    // @requires: workspace.cef_severity    (required)
    // @requires: workspace.src             (recommended)
    // @requires: workspace.suser           (recommended)
    // @produces: egress  (OCSF Authentication Activity, JSON, one event per line)
    //
    // Expects: the calling pipeline has already run a schema parser
    // (syslog.parse + cef.parse) and the canonical fields below are in workspace.

    workspace.ocsf = {
        "class_uid":    3002,
        "activity_id":  1,
        "severity_id":  workspace.cef_severity,
        "src_endpoint": { "ip": workspace.src },
        "actor":        { "user": { "name": workspace.suser } },
        ...
    }
    egress = to_json(workspace.ocsf)
}
```

Requirement levels follow the OCSF / ECS convention:

| Level | Meaning |
|-------|---------|
| `required` | The process will not produce a useful result without this field. If it is missing, the right call is usually to `drop` or `try { ... } catch { ... }` explicitly in the caller. |
| `recommended` | The process will run without it, but output quality degrades (lower fidelity, missing enrichment). |
| `optional` | Nice to have. Documented so a future reader knows the field exists and is consumed. |

Free-form prose comments explaining "what this process does" are fine *in addition to* the tags — but they are not a substitute. Prose drifts; structured tags survive review because tooling can check them.

### Why make contracts explicit

A process in isolation looks like code. A process in a pipeline is a node in a graph where the edges are workspace field names. When the graph is implicit, adding a new composer means reading every parser to see what fields happen to be populated; removing a parser means guessing whether anyone downstream depended on it. With explicit `@requires` / `@produces` you can answer both questions by grepping.

This is the same motivation as the *schema namespace* operating rule, applied one level down: *contracts that the config reader must know should be visible in the config, not inferred from runtime behaviour.*

## Anti-patterns

The following shapes compile, pass tests, and are wrong. They compile because the DSL is permissive; they are wrong because they make processes un-composable.

### Stateful processes

A `def process` that carries state across events — a counter, a cache, a "last seen" timestamp — cannot be reused across pipelines safely, cannot be replayed with `inject --json`, and cannot be reasoned about without knowing the history of traffic.

If you need dedup, rate-limiting, or aggregation, use a primitive that limpid ships as an explicit stateful construct (e.g. `table_lookup` + `table_upsert` backed by a declared `table`), not ad-hoc mutation inside a process body. The state is then named, observable, and owned by something other than the process.

### "God" processes with config-driven branches

```
// Don't — one body, many shapes, none of which is clearly the contract.
def process fw_dispatch {
    if workspace.vendor == "Fortinet" {
        parse_kv(egress)
        workspace.severity = workspace.level
    } else if workspace.vendor == "PaloAlto" {
        parse_csv(egress)
        workspace.severity = workspace.sev
    } else if workspace.vendor == "Cisco" {
        cef.parse(ingress)
        workspace.severity = workspace.cef_severity
    }
}
```

Split these into `parse_fortigate`, `parse_paloalto`, `parse_cisco` and dispatch at the pipeline level with `switch`. The switch is load-bearing routing information — it deserves to be in the pipeline where routing lives, not hidden inside a process that reads like a parser.

```
def pipeline fw {
    input fw_syslog
    switch workspace.vendor {
        "Fortinet"  { process parse_fortigate }
        "PaloAlto"  { process parse_paloalto }
        "Cisco"     { process parse_cisco }
        default     { drop }
    }
    process enrich_with_geoip
    output siem
}
```

### Processes that touch multiple schemas

A single process body that runs `syslog.parse`, `cef.parse`, and `ocsf.map` is conflating three different abstractions. Each schema deserves its own process; the pipeline composes them. This is the same rule as "one responsibility", stated in the schema axis.

### Silent recovery inside a process

Wrapping every call in `try { ... } catch { }` with an empty catch body swallows parse failures and makes `events_dropped` / `events_finished` metrics lie. If a process can fail, either:

1. Let it raise — the pipeline's `try` decides what to do with the error; or
2. Use an explicit catch that sets a `workspace.parse_error` field the next stage can branch on.

The rule of thumb: **a process should not make an event look successful when it was not.** The dropped/finished/discarded counts are the observability contract between limpid and the person running it.

## User-defined processes vs. built-in DSL functions

limpid ships two layers of reusable logic:

- **Functions** (`parse_json`, `regex_extract`, `syslog.parse`, `cef.parse`, …) are primitives. They are implemented in Rust, their call signature is fixed, and they have no pipeline context — no `ingress`, no `egress`, no `drop`. See [Expression Functions](./functions.md).
- **User-defined processes** (`def process`) are the DSL's compositional unit. They have the pipeline context: they can assign to `egress`, `drop`, `try`, branch, chain with `|`.

The question "should this be a function or a process?" has a clean answer:

| Situation | Write it as |
|-----------|-------------|
| Pure computation, no side effects, takes arguments → returns a value | A function (ideally, contribute it upstream) |
| Depends on a specific schema spec (RFC 5424, CEF, OCSF, …) | A namespaced function (`syslog.xxx`) if shipping with the daemon, otherwise a `def process` in a snippet |
| Reads or writes `egress`, `workspace`, or `ingress` directly | A `def process` |
| Can `drop`, or must run multiple statements in sequence | A `def process` |
| Operator-specific policy (facility rewrite, vendor filter, site-specific routing) | Always a `def process`, defined close to the pipeline that uses it |

A snippet library (for example, a future `processes/ocsf/*.limpid` collection) is entirely `def process` definitions. The functions they call — `syslog.parse`, `to_json`, `table_lookup` — are the primitives the daemon gives them to build on.

## Writing for a snippet library

If your process is intended to ship in a library (vendor parsers, OCSF composers, normalizers), a few additional conventions apply. They do not matter for a private site-specific config; they matter a great deal when hundreds of snippets coexist in a single directory.

### One concept per file

`processes/fortigate/traffic.limpid` holds everything needed to parse FortiGate traffic logs — the `def process parse_fortigate_traffic` itself plus any helpers it calls, if those helpers are not generic enough to promote. Do not pack multiple unrelated vendors or event classes into a single file.

### Stay close to the canonical schema

Composers (the snippets that produce OCSF JSON, CEF, ECS, …) should read field names that match the canonical schema they target. If you parse FortiGate KV into `workspace.srcip`, the OCSF composer reads `workspace.srcip`; it does not rename. Each rename layer is a drift risk. The canonical name wins.

### Keep composers pure

A composer has no branches, no conditional drops, no enrichment calls. It takes the fields the parser produced and assembles the output bytes. Enrichment (GeoIP, asset lookup, user-directory resolution) is a separate `enrich_*` process the pipeline runs between the parser and the composer.

The reason is not aesthetic: composers are the layer most likely to be mechanically generated from a schema definition file in the future (OCSF ships its spec as JSON). A composer that is field-pluck-plus-constants can be regenerated; a composer with conditional logic cannot.

### Test with `inject` + `tap`

Every snippet that ships in a library needs a fixture: a line of realistic input and the expected `egress` (or `workspace`) after the process runs. `limpidctl inject input <name>` + `limpidctl tap process <name> --json` is the testing primitive. See [Debug Tap](../operations/tap.md).

For examples of this in practice on a real multi-host deployment, see [Multi-host Pipeline Example](../pipelines/multi-host.md).

## Summary

- One process, one responsibility. If you need `and` in the name, split it.
- Document contracts as `@requires` / `@produces` comments.
- Do not put state, schema-dispatch, or silent error-swallowing in a process.
- Pick `def process` for anything with pipeline context; pick a function (or contribute one) for pure computation.
- Snippets destined for a library stay small, canonical, and testable with `inject` + `tap`.

These conventions exist to keep processes replaceable. A pipeline in limpid is valuable because every step is visible; that is only true while each step is small enough to see through.

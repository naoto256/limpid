# Process Design Guide

This page is for people writing processes — your own `def process` blocks in a production config, or snippets intended for wider reuse (OCSF composers, SIEM-specific parsers, vendor normalizers shipping under `processes/*.limpid`).

It is a **style guide**, not a reference. The reference for what a process can express is [User-defined Processes](./user-defined.md); the reference for functions is [Expression Functions](./functions.md). The principles the guide rests on are in [Design Principles](../design-principles.md).

Everything here is about one thing: keeping processes small enough that a reader can hold one in their head, and composable enough that pipelines stay readable.

## The granularity rule

> One process does one thing. If you cannot name it in three or four words without `and`, it is doing too much.

Good process names describe a single abstraction:

- `strip_pri` — removes a syslog `<PRI>`.
- `parse_fortigate_kv` — parses FortiGate KV payloads.
- `enrich_with_geoip` — annotates `workspace.geo` with a GeoIP lookup of an IP from earlier parsing.
- `drop_healthchecks` — filters LB noise.
- `ama_rewrite` — rewrites the PRI byte for AMA facility routing.

Each of these is a verb phrase. Each has a single reason to exist. Each can be composed with `|` alongside others without its behavior depending on the neighbours.

Bad names give away the problem:

- `parse_and_enrich_and_filter` — three responsibilities.
- `process_event` — no abstraction, just "stuff happens here".
- `handle_fortigate` — vague enough that it will grow forever.

If you catch yourself writing `and` in a process name, split it:

```
// Don't: one process doing three things — and parsing events you're
// about to drop is wasted work.
def process parse_and_enrich_fortigate {
    if contains(ingress, "healthcheck") {
        drop
    }
    workspace.syslog = syslog.parse(ingress)
    workspace.kv     = parse_kv(workspace.syslog.msg)
    workspace.geo    = geoip(workspace.kv.srcip)
}

// Do: three processes, each a single step, composed at the pipeline.
// drop_healthchecks runs first so noise events never hit the parser.
def process drop_healthchecks { if contains(ingress, "healthcheck") { drop } }
def process parse_fortigate {
    workspace.syslog = syslog.parse(ingress)
    workspace.kv     = parse_kv(workspace.syslog.msg)
}
def process enrich_fortigate { workspace.geo = geoip(workspace.kv.srcip) }

def pipeline fw {
    input fw_syslog
    process drop_healthchecks | parse_fortigate | enrich_fortigate
    output siem
}
```

The split config is longer. It is also easier to test, easier to tap between steps (`limpidctl tap process parse_fortigate`), and easier to reuse when a second vendor needs the same GeoIP enrichment.

## Input and output contracts

Every process has an implicit contract with its neighbours in the pipeline: *what do I expect to be present when I run, and what do I leave behind for the next stage?*

Because the DSL does not check that contract today, you document it in comments using a small, machine-parseable convention. The grammar is parseable enough that a future static-analysis pass could promote these tags to a real check (warning when a pipeline links a composer to a parser that does not produce the required fields), but limpid does not commit to that — write them because they help the next reader, not because the analyzer will catch you. Pretending the contracts do not exist costs the first person who has to modify the snippet a year later.

### The `@requires` / `@produces` tag convention

Put tags in the first block of comments inside the process body. One tag per line. Each tag names a field path in `workspace.*` (or, less commonly, `egress`).

```
def process compose_ocsf_authentication {
    // @requires: workspace.limpid.severity_id          (required)
    // @requires: workspace.limpid.src_endpoint.ip      (recommended)
    // @requires: workspace.limpid.actor.user.name      (recommended)
    // @produces: egress  (OCSF Authentication Activity, JSON, one event per line)
    //
    // Expects: the calling pipeline has run a vendor parser that
    // already mapped its raw fields into `workspace.limpid.*`
    // canonical form. This composer is vendor-unaware — it does not
    // read `workspace.cef.*` / `workspace.syslog.*` directly.

    workspace.limpid.class_uid   = 3002
    workspace.limpid.activity_id = 1
    egress = to_json(workspace.limpid)
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
        workspace.kv = parse_kv(egress)
        workspace.severity = workspace.kv.level
    } else if workspace.vendor == "PaloAlto" {
        workspace.csv = csv_parse(egress, ["receive_time", "serial", "type", "subtype", "sev"])
        workspace.severity = workspace.csv.sev
    } else if workspace.vendor == "Cisco" {
        workspace.cef = cef.parse(ingress)
        workspace.severity = workspace.cef.severity
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

### Silent recovery inside a process

Wrapping every call in `try { ... } catch { }` with an empty catch body swallows parse failures and makes `events_dropped` / `events_finished` metrics lie. If a process can fail, either:

1. Let it raise — the pipeline's `try` decides what to do with the error; or
2. Use an explicit catch that sets a `workspace.parse_error` field the next stage can branch on.

The rule of thumb: **a process should not make an event look successful when it was not.** The dropped/finished/discarded counts are the observability contract between limpid and the person running it.

## Functions vs. processes

limpid has three layers of reusable logic:

- **Built-in functions** (`parse_json`, `regex_extract`, `syslog.parse`, `cef.parse`, …) are primitives. Implemented in Rust, signature fixed, no pipeline context — no `ingress`, no `egress`, no `drop`. See [Expression Functions](./functions.md).
- **User-defined functions** (`def function`) are pure value-returning helpers in the DSL. Body is one expression. No Event reads, no side effects, no recursion. Composable in any expression context — HashLit values, function args, binary operands. See [User-defined Functions](./user-defined-functions.md).
- **User-defined processes** (`def process`) are the DSL's compositional unit *with* pipeline context: they can assign to `egress`, `drop`, `try`, branch, chain with `|`.

The question "function or process?" has a clean answer:

| Situation | Write it as |
|-----------|-------------|
| Pure computation, no side effects, takes arguments → returns a value, vendor-agnostic | **`def function`** in the DSL (or `_common/*.limpid`) |
| Pure computation but the daemon should ship it (built-in availability, performance) | A built-in function in Rust (contribute upstream) |
| Depends on a specific schema spec (RFC 5424, CEF, OCSF, …) | A namespaced built-in (`syslog.xxx`) if shipping with the daemon, otherwise a `def process` in a snippet |
| Reads or writes `egress`, `workspace`, or `ingress` directly | A `def process` |
| Can `drop`, or must run multiple statements in sequence | A `def process` |
| Recursive | A `def process` (`def function` rejects recursion at `--check` time) |
| Operator-specific policy (facility rewrite, vendor filter, site-specific routing) | Always a `def process`, defined close to the pipeline that uses it |

A snippet library (for example, the v0.6.0 `_common/*.limpid` + `parsers/*.limpid` + `composers/*.limpid` collection) mixes the three: `def function` for vendor-agnostic mappings (severity, proto, action), `def process` for the parser / composer bodies that consume Event state and write to `workspace.limpid`, and built-in primitives (`syslog.parse`, `cef.parse`, `to_json`, `regex_*`) as the building blocks underneath.

## Writing for a snippet library

If your process is intended to ship in a library (vendor parsers, OCSF composers, normalizers), a few additional conventions apply. They do not matter for a private site-specific config; they matter a great deal when hundreds of snippets coexist in a single directory.

### One schema per file

The library's organising axis is the **schema** a snippet implements. For vendor parsers, a schema is a *(vendor, format)* pair — `parsers/fortigate_cef.limpid` is one schema (FortiGate's CEF field model), `parsers/fortigate_syslog.limpid` is another (FortiGate's KV-over-syslog field model). The two share a vendor name but their field shapes, dispatchers, and subtype handling are different enough that the FortiGate documentation itself splits them into separate references; the snippet library follows.

For OCSF composers, the schema *is* the class — `composers/ocsf_network_activity.limpid`, `composers/ocsf_detection_finding.limpid`. OCSF is vendor- and format-independent on purpose, so per-class is the natural unit.

The contents of one file:

- The leaf parsers (or the per-class composer body).
- The dispatcher (subtype dispatcher for parsers, the schema-level `compose_ocsf` for composers).
- Helpers that are specific to this schema. Helpers shared across multiple schemas live under `_common/` and are included as needed.

A vendor's "any format" entry point (e.g. `parse_fortigate` that detects format and routes to the right `(vendor, format)` parser) is a thin shim that includes both per-schema files and dispatches between them — that shim is the only place the vendor-without-format abstraction lives.

Do not pack multiple unrelated schemas into a single file.

### Use `workspace.limpid` as the canonical intermediate

Pick one canonical intermediate shape and have every parser write into it; have every composer read from it. limpid's library uses the namespace `workspace.limpid` for this — OCSF-inspired in field shape, but explicitly *limpid's* canonical, not a strict OCSF spec binding. The chain has three responsibility layers:

```
ingress
   │
   ▼
┌──────────────────────┐    workspace.syslog.*
│  format primitives   │    workspace.cef.*       — raw, format-shaped
│  syslog.parse,       │ ─► workspace.kv.*
│  cef.parse, parse_kv,│    workspace.json.*
│  parse_json, …       │    …
└──────────────────────┘
   │
   ▼
┌──────────────────────┐
│  vendor parsers      │
│  parse_fortigate_cef,│ ─► workspace.limpid.*    — canonical intermediate
│  parse_paloalto_csv, │                            (OCSF-shaped, not strict)
│  parse_mde_alert, …  │
└──────────────────────┘
   │
   ▼
┌──────────────────────────────┐
│  composers                   │
│  compose_ocsf_network_activity, │ ─► egress (JSON in target wire schema)
│  compose_ecs_network, …      │
└──────────────────────────────┘
   │
   ▼
egress
```

- **Format primitives** (`syslog.parse`, `cef.parse`, `parse_kv`, `parse_json`, `csv_parse`) capture raw bytes into a format-specific namespace (`workspace.syslog`, `workspace.cef`, …). They know nothing about vendors or downstream schemas.
- **Vendor parsers** (`parse_fortigate_cef`, `parse_paloalto_csv`, `parse_mde_alert`) read the format namespace and write canonical fields under `workspace.limpid.*`. This is the only layer that knows both the vendor's quirks and the canonical shape.
- **Composers** (`compose_ocsf_network_activity`, `compose_ocsf_detection_finding`, `compose_ecs_network`, …) read `workspace.limpid.*` and serialise to `egress` in their target wire schema. They are vendor-unaware on purpose: they pluck `workspace.limpid.src_endpoint.ip` regardless of whether it came from a FortiGate or a Palo Alto event.

The payoffs:

- **Adding a new vendor** is a new parser; no composer change.
- **Bumping a target wire schema** (OCSF v3 → v4, ECS minor bump) is a composer change; no parser change.
- **Multiple vendors → one target** falls out for free — every parser drops its output into the same canonical workspace shape.
- **Multiple targets from the same canonical** (one OCSF composer + one ECS composer reading the same `workspace.limpid.*`) is what makes the matrix manageable. The N-vendor × M-target multiplication never happens at the parser level.

#### The parser / composer contract

The two-sided rule of thumb for `workspace.limpid.*`:

- **Parsers fill `workspace.limpid` as close to OCSF shape as possible — but they are not bound by OCSF.** Whenever a vendor field has a clean OCSF home, use the OCSF field name (`src_endpoint.ip`, `actor.user.name`). When it doesn't, carry it on `workspace.limpid` under a vendor-meaningful name; do not throw the data away just because OCSF has no slot.
- **Composers may assume `workspace.limpid` is OCSF-shaped — but they must not assume strict OCSF compliance.** A composer reads the fields it needs and tolerates extras / absences. An OCSF composer maps `workspace.limpid.*` directly into OCSF JSON; an ECS composer translates the same `workspace.limpid.*` into ECS JSON, taking advantage of the OCSF-likeness without depending on it.

A parser must not write vendor-specific format names that bleed into the composer (`workspace.cef.src`, `workspace.fgt_session_id` left at top level); a composer must not read vendor-specific format names directly (`workspace.cef.src`). The contract between them is `workspace.limpid.*`, full stop. Each rename or pass-through layer beyond that is a drift risk.

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

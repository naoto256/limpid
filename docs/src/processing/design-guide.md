# Process Design Guide

This page is for people writing processes — your own `def process` blocks in a production config, or snippets intended for wider reuse (OCSF composers, SIEM-specific parsers, vendor normalizers shipping under `processes/*.limpid`).

It is a **style guide**, not a reference. The reference for what a process can express is [User-defined Processes](./user-defined.md); the reference for functions is [Built-in Functions](../functions/expression-functions.md). The principles the guide rests on are in [Design Principles](../design-principles.md).

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

```limpid
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

The shipped snippet pack declares its public surface in the canonical header
schema: a file-level `Facade` plus adjacent per-member `Summary`, `Reads`,
`Writes`, or `Signature` contracts described
in the [pack README](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md#authoring-conventions).
Inside a large file, private leaf-local `@requires` / `@produces` comments may
add useful detail, but they do not replace a facade member block and the
analyzer does not consume them.

### The `@requires` / `@produces` tag convention

When a file contains several leaves, put supplemental tags in the first comment
block inside the leaf. One tag per line. Each tag names a field path in
`workspace.*` (or, less commonly, `egress`).

```limpid
def process compose_ocsf_authentication {
    // @requires: workspace.lsis.parsed.severity_number      (optional; normalized OTel SeverityNumber)
    // @requires: workspace.lsis.parsed.severity             (optional; exact source severity text)
    // @requires: workspace.lsis.parsed.src_endpoint.ip      (recommended)
    // @requires: workspace.lsis.parsed.user.name            (recommended)
    // @produces: workspace.lsis.composed.ocsf  (OCSF Authentication Activity, JSON string)
    //
    // Expects: the calling pipeline has run a vendor parser that
    // already mapped its raw fields into `workspace.lsis.parsed.*`
    // (the LSIS facts layer). This composer is vendor-unaware — it
    // does not read `workspace.cef.*` / `workspace.syslog.*` directly.
    //
    // Contract note: composers write their finished wire form to
    // `workspace.lsis.composed.<slot>` — never to `egress` directly.
    // A companion one-line process `ocsf_to_egress` (defined next to
    // the composer in the same file) moves the slot to `egress` when
    // the pipeline emits OCSF as its wire form. This is the egress
    // single-writer invariant — see the [pack
    // README](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md#slot-registry--composed-layer).

    process validate_ocsf_severity_number
    let activity = workspace.lsis.parsed.activity_id
    workspace.lsis.composed.ocsf = to_json(null_omit({
        class_uid: 3002,
        category_uid: 3,
        activity_id: activity,
        type_uid: 3002 * 100 + activity,
        time: timestamp_ns_to_ms(coalesce(workspace.lsis.parsed.time, received_at)),
        severity_id: compose_ocsf_severity_id(
            workspace.lsis.parsed.severity_number,
            workspace.lsis.parsed.severity_id,
            workspace.lsis.parsed.severity
        ),
        severity: workspace.lsis.parsed.severity,
        status_id: workspace.lsis.parsed.status_id,
        user: workspace.lsis.parsed.user,
        actor: workspace.lsis.parsed.actor,
        src_endpoint: workspace.lsis.parsed.src_endpoint,
        dst_endpoint: workspace.lsis.parsed.dst_endpoint,
        metadata: workspace.lsis.parsed.metadata
    }))
}
```

Requirement levels follow the OCSF / ECS convention:

| Level | Meaning |
|-------|---------|
| `required` | The process will not produce a useful result without this field. If it is missing, the right call is usually to `drop` or `try { ... } catch { ... }` explicitly in the caller. |
| `recommended` | The process will run without it, but output quality degrades (lower fidelity, missing enrichment). |
| `optional` | Nice to have. Documented so a future reader knows the field exists and is consumed. |

Free-form prose comments explaining "what this process does" are fine in
addition to the tags. For shipped snippets, neither form substitutes for the
canonical facade and member headers that header lint and inventory generation
validate.

### Why make contracts explicit

A process in isolation looks like code. A process in a pipeline is a node in a graph where the edges are workspace field names. When the graph is implicit, adding a new composer means reading every parser to see what fields happen to be populated; removing a parser means guessing whether anyone downstream depended on it. With explicit `@requires` / `@produces` you can answer both questions by grepping.

This is the same motivation as the *schema namespace* operating rule, applied one level down: *contracts that the config reader must know should be visible in the config, not inferred from runtime behaviour.*

## Anti-patterns

The following shapes compile, pass tests, and are wrong. They compile because the DSL is permissive; they are wrong because they make processes un-composable.

### Stateful processes

A `def process` that carries state across events — a counter, a cache, a "last seen" timestamp — cannot be reused across pipelines safely, cannot be replayed with `inject input <name> --json`, and cannot be reasoned about without knowing the history of traffic.

If you need dedup, rate-limiting, or aggregation, use a primitive that limpid ships as an explicit stateful construct (e.g. `table_lookup` + `table_upsert` backed by a declared `table`), not ad-hoc mutation inside a process body. The state is then named, observable, and owned by something other than the process.

### "God" processes with config-driven branches

```limpid
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

Split these into the shipped vendor parsers and dispatch at the pipeline level. The routing is load-bearing information — it deserves to be in the pipeline where routing lives, not hidden inside a process that reads like a parser.

FortiGate and Palo Alto CEF records share a syslog transport and CEF format, while Cisco ASA records use a different syslog body. No input creates a `workspace.vendor` field, so this deployment first unwraps syslog and then dispatches on its known, anchored body contracts. Each branch decodes CEF where applicable and runs the matching vendor parser. The parser may populate `workspace.lsis.parsed.src_endpoint.ip`; only when that fact is present does the shared enrichment process consume it. This is explicit routing for the wire formats accepted by this pipeline, not Limpid-wide vendor autodetection.

```limpid
def process enrich_with_geoip {
    if workspace.lsis.parsed.src_endpoint.ip != null {
        workspace.geo = geoip(workspace.lsis.parsed.src_endpoint.ip)
    }
}

def pipeline fw {
    input fw_syslog
    process parse_syslog
    if starts_with(workspace.syslog.msg, "CEF:0|Fortinet|Fortigate|") {
        process parse_cef | parse_fortigate_cef
    } else if starts_with(workspace.syslog.msg, "CEF:0|Palo Alto Networks|PAN-OS|") {
        process parse_cef | parse_paloalto_cef
    } else if regex_match(workspace.syslog.msg, "^(?:[^:]*: )?%ASA-\\d-\\d+: ") {
        process parse_asa
    } else {
        drop
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

- **Built-in functions** (`parse_json`, `regex_extract`, `syslog.parse`, `cef.parse`, …) are primitives. Implemented in Rust, signature fixed, no pipeline context — no `ingress`, no `egress`, no `drop`. See [Built-in Functions](../functions/expression-functions.md).
- **User-defined functions** (`def function`) are pure value-returning helpers in the DSL. Body is one expression. No Event reads, no side effects, no recursion. Composable in any expression context — HashLit values, function args, binary operands. See [User-defined Functions](../functions/user-defined.md).
- **User-defined processes** (`def process`) are the DSL's compositional unit *with* pipeline context: they can assign to `egress`, `drop`, `try`, branch, chain with `|`.

The question "function or process?" has a clean answer:

| Situation | Write it as |
|-----------|-------------|
| Pure computation, no side effects, takes arguments → returns a value, vendor-agnostic | **`def function`** in the DSL (or under the shipped `functions/*.limpid`) |
| Pure computation but the daemon should ship it (built-in availability, performance) | A built-in function in Rust (contribute upstream) |
| Depends on a specific schema spec (RFC 5424, CEF, OCSF, …) | A namespaced built-in (`syslog.xxx`) if shipping with the daemon, otherwise a `def process` in a snippet |
| Reads or writes `egress`, `workspace`, or `ingress` directly | A `def process` |
| Can `drop`, or must run multiple statements in sequence | A `def process` |
| Recursive | Not supported; both function and process call graphs must be acyclic |
| Operator-specific policy (facility rewrite, vendor filter, site-specific routing) | Always a `def process`, defined close to the pipeline that uses it |

A snippet library (the `functions/*.limpid` + `parsers/parse_*.limpid` + `composers/compose_*.limpid` collection that ships under `/usr/share/limpid/snippets/`) mixes the three: `def function` files under `functions/` for vendor-agnostic mappings (severity, proto, action), `def process` files under `parsers/parse_*` for vendor parsers and under `composers/compose_*` for the per-class composer bodies that consume Event state and write to `workspace.lsis.parsed` / `workspace.lsis.composed.<slot>`, and built-in primitives (`syslog.parse`, `cef.parse`, `to_json`, `regex_*`) as the building blocks underneath.

## Writing for a snippet library

If your process is intended to ship in a library (vendor parsers, OCSF composers, normalizers), a few additional conventions apply. They do not matter for a private site-specific config; they matter a great deal when hundreds of snippets coexist in a single directory.

### One schema per file

The library's organising axis is the **schema** a snippet implements. For vendor parsers, a schema is a *(vendor, format)* pair — `parsers/parse_fortigate_cef.limpid` is one schema (FortiGate's CEF field model), `parsers/parse_fortigate_syslog.limpid` is another (FortiGate's KV-over-syslog field model). The two share a vendor name but their field shapes, dispatchers, and subtype handling are different enough that the FortiGate documentation itself splits them into separate references; the snippet library follows.

For OCSF composers, the schema is the class. The library ships a single dispatcher per emit format (`composers/compose_ocsf.limpid`) that branches on the OCSF class id (`workspace.lsis.parsed.class_uid`) to assemble the right shape per event, so vendors that feed multiple OCSF classes can share one composer entry point.

The contents of one file:

- The leaf parsers (or the per-class composer body).
- The dispatcher (subtype dispatcher for parsers, the schema-level `compose_ocsf` for composers).
- Helpers that are specific to this schema. Helpers shared across multiple schemas live under `functions/` and are included as needed.

A vendor's "any format" entry point (e.g. `parse_fortigate` that detects format and routes to the right `(vendor, format)` parser) is a thin shim that includes both per-schema files and dispatches between them — that shim is the only place the vendor-without-format abstraction lives.

Do not pack multiple unrelated schemas into a single file.

### Use `workspace.lsis` as the canonical intermediate

Pick one canonical intermediate shape and have every parser write into it; have every composer read from it. limpid's library uses the namespace `workspace.lsis` for this — the Limpid Snippet Intermediate Schema (LSIS), stratified into `parsed` / `shed` / `composed` layers. See the [pack README](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md#lsis--the-limpid-snippet-intermediate-schema) for the layer contracts and slot registries; the summary that matters for this guide is the flow:

```text
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
│  parse_fortigate_cef,│ ─► workspace.lsis.parsed.*    — facts layer
│  parse_paloalto_cef, │                                  (OCSF-vocabulary,
│  parse_cloudtrail, … │                                   not OCSF-bound)
└──────────────────────┘
   │
   ▼
┌────────────────────────────────┐
│  per-source target adapters    │
│  fortigate_cef_to_otlp, ...    │ ─► workspace.lsis.shed.<target>.*
│  (OTLP path; OCSF bypasses)    │       (placement / Body construction)
└────────────────────────────────┘
   │
   ▼
┌────────────────────────────────┐
│  schema composers              │
│  compose_ocsf,                 │ ─► workspace.lsis.composed.<slot>
│  compose_rfc5424,              │       (target wire form: OCSF JSON,
│  compose_replayable, …         │        RFC 5424 record, JSONL, …)
└────────────────────────────────┘
   │
   ▼
┌────────────────────────────────┐
│  egress terminator             │
│  <slot>_to_egress companion,   │ ─► egress
│  or an envelope composer       │       (single-writer per pipeline)
│  (compose_otlp) downstream     │
└────────────────────────────────┘
```

- **Format primitives** (`syslog.parse`, `cef.parse`, `parse_kv`, `parse_json`, `csv_parse`) capture raw bytes into a format-specific namespace (`workspace.syslog`, `workspace.cef`, …). They know nothing about vendors or downstream schemas.
- **Vendor parsers** (`parse_fortigate_cef`, `parse_paloalto_cef`, `parse_cloudtrail`, `parse_ocsf`, …) read the format namespace and write facts under `workspace.lsis.parsed.*`. This is the only layer that knows both the vendor's quirks and the canonical shape. (The shipped set grows on the 0.7.x cadence — see [Snippet Library](../snippets/README.md) for the current inventory.)
- **Per-source target adapters** live in the same parser file. They own non-obvious target placement and construction, such as OTLP Resource/Scope/LogRecord attributes and the Body AnyValue variant. They write `workspace.lsis.shed.<target>.*`; deployment-specific adjustments may replace those slots only after the adapter.
- **Composers** read canonical facts for source-independent scalar mappings and target shed slots for adapter-owned structure. They serialise to `workspace.lsis.composed.<slot>` (OCSF JSON, RFC 5424 record, OTLP proto bytes, …). A companion one-line process, `<slot>_to_egress`, moves the slot to `egress` when the pipeline emits that shape as its wire form. Composers are vendor-unaware on purpose: they never decide whether a FortiGate or Palo Alto fact belongs in OTLP Resource, Scope, Body, or LogRecord attributes.

The payoffs:

- **Adding a new vendor** is a new parser plus its target adapters; no composer change.
- **Bumping a target wire schema** (OCSF v3 → v4, ECS minor bump) is a composer change; no parser change.
- **Multiple vendors → one target** falls out for free — every parser drops its output into the same facts layer.
- **Multiple targets from the same facts** (one OCSF composer + one ECS composer reading the same `workspace.lsis.parsed.*`) is what makes the matrix manageable. The N-vendor × M-target multiplication never happens at the parser level.

#### The parser / composer contract

The two-sided rule of thumb for `workspace.lsis.parsed.*`:

- **Parsers fill `workspace.lsis.parsed` in canonical LSIS shape — OCSF-vocabulary, not OCSF-bound.** Whenever a vendor field has a clean OCSF home, use the OCSF field name (`src_endpoint.ip`, `actor.user.name`). When it doesn't, carry it on `workspace.lsis.parsed` under a vendor-meaningful name; do not throw the data away just because OCSF has no slot.
- **Composers may assume `workspace.lsis.parsed` follows canonical LSIS shape — but they must not assume strict OCSF compliance.** A composer reads the fields it needs and tolerates extras / absences. An OCSF composer renders `workspace.lsis.parsed.*` to OCSF JSON in the `workspace.lsis.composed.ocsf` slot; an ECS composer would translate the same facts into an ECS JSON slot, taking advantage of the OCSF-vocabulary overlap without depending on it.

A parser must not write vendor-specific format names into the facts layer (`workspace.cef.extension.src`, `workspace.fgt_session_id` left at top level); a composer must not read vendor-specific format names directly (`workspace.cef.extension.src`). The contract between them is `workspace.lsis.parsed.*`, full stop. Each rename or pass-through layer beyond that is a drift risk.

### Keep composers pure

A composer has no branches, no conditional drops, no enrichment calls. It takes the fields the parser produced and assembles the output bytes. Enrichment (GeoIP, asset lookup, user-directory resolution) is a separate `enrich_*` process the pipeline runs between the parser and the composer.

The reason is not aesthetic: composers are the layer most likely to be mechanically generated from a schema definition file in the future (OCSF ships its spec as JSON). A composer that is field-pluck-plus-constants can be regenerated; a composer with conditional logic cannot.

### Document the upstream assumption in the file header

A vocabulary parser (or any snippet that consumes prior layer state in
`workspace.*`) is implicitly bound to a specific set of upstream stacks
— the transport layers it knows how to find its body / pid / hostname /
timestamp from. That binding is invisible from the dispatcher body, so
state it explicitly in a header block at the top of the file:

```text
// Vendor:   OpenSSH
// Wire:     sshd application body (Accepted publickey for ... / Failed
//           password for ... / Disconnected from ... etc.)
// Upstream: parse_syslog | parse_openssh
//             body  ← workspace.syslog.msg
//             pid   ← workspace.syslog.pid
//           parse_journald | parse_openssh
//             body  ← workspace.journald.MESSAGE
//             pid   ← coalesce(workspace.journald._PID,
//                              workspace.journald.SYSLOG_PID)
//           (no upstream — falls back to parse_syslog inline on `ingress`)
// Output:   workspace.lsis.parsed.* (OCSF Authentication, class_uid 3002)
```

Stacks not listed are out of scope. If your wire is `openssh` over
`CEF` over `syslog` over `JSON` over `OCSF` over `OTLP` (or any other
permutation a library author would not reasonably anticipate), the
correct response is to write your own vocabulary parser that reads
from whichever `workspace.<layer>` your particular pipeline populated.
The library covers the common cases and documents which they are; it
does not pretend to be a universal solver across arbitrary transport
stacks. See [Workspace is event-scoped, not message-passed](../design-principles.md#workspace-is-event-scoped-not-message-passed)
for the underlying design choice.

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

# Design Principles

These five principles describe the shape limpid is built around. Every other choice — which features ship, what the DSL admits, how the daemon behaves under load — follows from them. When a design question arises, the principles are where the answer is looked up first.

The first four describe the *pipeline*: what flows, what each layer is allowed to do, what crosses hop boundaries. The fifth describes the *software itself*: how it behaves toward the operator who runs it.

The principles were not arrived at by setting out to replace anything in particular. They came out of operating the established log forwarders day-to-day and noticing which kinds of friction kept recurring — and concluding that treating observability, type checking, and reversible operation as part of the foundation, rather than as features added on later, needed a different shape of daemon.

## Principle 1 — Zero hidden behavior

Every transform, mapping, default, and routing decision is expressed in the DSL and observable through the daemon's tap and inject capabilities.

- No implicit parsers that run "because the input looks like JSON"
- No magic templates, convenience defaults, or silent coercion
- No "this field gets filled in for you if you don't set it"
- Visible at every level — detail when you zoom in, structure when you zoom out

If a pipeline produces an event, you can point to the DSL line that produced it. If a field has a value, you can point to the DSL line that assigned it. There is no fourth layer where the daemon "helpfully" did something on your behalf.

**Why:** hidden behavior is invisible to reviewers, invisible to new team members, invisible to incident responders, and invisible to the person reading the config a year later. Every hidden transform becomes a landmine.

**Consequence:** every transform in the pipeline is open to inspection — you can read, audit, or change the exact DSL line that produced any event, any field, any routing decision. At the same time, the DSL's process composition (`process A | B | C`) keeps the bird's-eye view tractable — a pipeline is a chain of named pieces, not a wall of inline code. Full detail when you zoom in, clean overview when you zoom out.

## Principle 2 — I/O layer purity

Input and output modules do not touch the **semantic contents** of an event. Parsing, transformation, and normalization — anything that interprets what the bytes mean — happen exclusively in the `process` layer.

Mechanical structural work is allowed and necessary: input modules perform wire-level framing (boundary detection, octet-count, LF), split a wire-level batch into atomic Events per Principle 4, and attach `source` from the peer address. Output modules serialize the Event's `egress` bytes to the wire and may bundle multiple Events into one wire-level request (per Principle 4). None of this interprets the record's contents.

- **Input** modules receive bytes, perform wire-level framing (boundary detection, LF / octet-counting), optionally sanity-check the wire format for shape only, and emit one Event per logical record — one syslog line, one OTLP LogRecord, one Kafka message, one NDJSON line — into the pipeline (per Principle 4). They do not extract PRI, do not parse timestamps, do not structure the message. The event enters the pipeline with `ingress` set to the received bytes, `egress` initialized to the same bytes, an empty `workspace`, and the source / receive-time metadata.
- **Output** modules serialize the event's `egress` bytes to the wire, applying any wire-level framing the destination requires (RFC 5424 octet-counting, gRPC HTTP/2, OTLP envelope construction, etc.) and optionally bundling multiple events into one wire-level request. They do not interpret, rewrite, lookup, or enrich the bytes. An output is a dumb transport.
- **Process** modules do everything else. Parsing, PRI extraction, timestamp derivation, field normalization, OTLP/OCSF mapping, enrichment — all in `process`.

**Why:** when parsing lives in the input, the config reader has no idea which fields exist and which don't. When transformation lives in the output, the same field can mean different things depending on where it is sent. Centralizing all event mutation in `process` makes the pipeline a single, readable transformation — observable at any point with `tap`.

**Consequence:** limpid ships domain-specific parsers as DSL snippets invoked from a `process`, not as behavior baked into an input module (see *Operating rules → Domain knowledge in DSL* below).

## Principle 3 — Only `egress` crosses hop boundaries

This follows naturally from Principle 2. With input as pure transport and output as pure transport, the entire pipeline reduces to a journey from `ingress` to `egress`:

> `ingress` (immutable, from the wire) → process · process · process → `egress` (to the wire)

Each `process` block is a step in **constructing** `egress` out of what arrived as `ingress`. Scratch space (`workspace`, `let`, `source`, `received_at`) is available for the construction work, but it lives and dies inside the pipeline; none of it is written to the wire. Only `egress` is.

**At the semantic level, that is the entire wire contract between hops** — the next hop receives whatever was put into `egress` and nothing else. If it needs the source-claimed event timestamp, a syslog header value, the original PRI, the device name — anything — that information has to be inside `egress`. Parser snippets at the next hop re-extract whatever's needed from the bytes received.

**Why:** when the wire contract is just `egress` bytes, no metadata can silently drift between daemons, no schema can grow incompatible across versions, and no "attribute" can mean one thing on one hop and another thing on the next. Each hop re-derives what it needs from the bytes it received.

**Consequence:** multi-hop pipelines are simple to reason about. The contract between hops is exactly the bytes in `egress` — there is no hidden sidecar of "things that also travel".

## Principle 4 — Atomic events through the pipeline

The unit flowing through a limpid pipeline is one event = one logical record. The pipeline never operates on bundles, never fans out, never aggregates.

- **Inputs** split: where the wire format bundles records (a TCP syslog stream framed with octet-counting or LF, an OTLP `ExportLogsServiceRequest` carrying ten `LogRecord`s, a Kafka batch, an HTTP request body of NDJSON), the input decomposes the bundle at the transport boundary into one Event per record. Resource and Scope context, when present, is preserved per Event by carrying it inside `ingress`.
- **Process** snippets pass through: a `process` body either mutates the in-flight event, drops it (`drop`), or terminates the pipeline for that event (`finish`). It never emits a second event from a single input. Cardinality only ever decreases inside the pipeline.
- **Outputs** bundle: where the destination benefits from batching (collector → SaaS, NDJSON to Elasticsearch, Kafka producer batches), the output rebuilds the wire-level bundle at the emit boundary, with bundling shape and timing set by the output module's configuration.

**Why:** an event is the natural unit of meaning in log data — one record, one thing that happened. Wire-level bundling is an optimization on top of the data, not part of the data itself. Aligning the pipeline with the data's natural unit means every operation — replay, observation, queueing, redelivery — addresses what operators actually care about.

**Consequence:** when you write a `process`, you only have to think about one event at a time. The same single-event interface runs through the rest of limpid — tap streams one line per event, inject replays one event at a time, the queue redelivers one event at a time. A captured event is a complete unit of work that can be reasoned about, replayed, or relocated on its own — and a `process` written for one pipeline drops into another without modification, because none carry batch context.

## Principle 5 — Safety and operational transparency

A log forwarder runs unattended on production paths, and any mistake compounds quickly — lost events, broken downstream parsers, blind monitoring, slow root-cause analysis. limpid is built so that every operationally-risky step has a *verify-before*, *observe-during*, *undo-after*, or *replay-later* path attached. The operator is never blind to what the daemon is doing, and never out of options when something goes wrong.

- **Verify before deployment.** Static analysis runs over the full configuration before the daemon binds a socket. Type errors in DSL, unknown identifiers, unreachable pipelines — all surfaced at parse time with line, column, and "did you mean" suggestions.

- **Observe during runtime.** Live event streaming at any named hop in any pipeline — input, output, or named process — without pausing the pipeline or duplicating traffic. Per-module counters (events received, invalid, written, failed) are exposed as metrics, so dashboards reflect the daemon's actual state. Together with Principle 1's zero hidden behavior in *config*, Principle 5 enforces zero hidden behavior in the *running system*.

- **Replay later for investigation.** Captured events can be re-run through any configuration — yesterday's production traffic against today's pipeline change — without involving the production source. CI exercises the same path with a single sample event.

- **Undo after deployment.** Configuration reload is atomic: a new configuration that fails to parse, type-check, or start is rolled back; the previous configuration keeps running while the operator sees a diagnostic. The daemon never enters a half-loaded state.

- **Fail soft, surface clearly.** Output queues retry with backoff, fall back to a secondary destination for dead-letter routing, and optionally persist to disk so a crash does not lose events in flight. Shutdown reports the count of events still buffered rather than letting them disappear silently.

**Why:** a daemon that processes production traffic 24/7 cannot afford a *"well, redeploy and hope"* recovery story, and an opaque daemon cannot be operated safely even when it is technically working. The cost of these affordances — more code paths, more CLI surface, more documentation, more API — is paid up front. The benefit is paid every time something goes wrong, and every time an operator wonders *"is the pipeline doing what I think it is?"* without that question being a costly investigation.

**Consequence:** changes to a limpid pipeline are inexpensive to make. The operator verifies before deploying, observes the result live, replays captured traffic to confirm a fix, and rolls back instantly if anything looks wrong — all without involving the production source. limpid is something you operate, not just something that runs.

---

## Operating rules

These are not principles — they are concrete consequences of the five principles above, written down so they are easy to cite. They might be revised over time, but since they are derived from the principles above, significant change is not expected; the principles themselves do not move.

### Domain knowledge ships as DSL snippets, not Rust

Typical parsing patterns — OTLP normalization, OCSF mapping, Windows Snare parsing, Apache / nginx / Cisco ASA / FortiGate log formats — are not built-in Rust modules. They are DSL snippets that users `include` from their configuration. Snippets shipped with limpid live read-only under `/usr/share/limpid/snippets/` (organized as `parsers/`, `composers/`, `_common/`); user-authored snippets live wherever the operator keeps them.

- Users can include them as-is, modify them, ignore them, or replace them
- A specification change (e.g. OTLP version update) is a DSL edit, not a Rust release
- Adding support for a new vendor format is a pull request to a snippet, not a feature in the daemon
- The daemon itself ships only **primitives**: the wire-speaking inputs / outputs, the generic functions (`parse_kv`, `regex_extract`, `table_lookup`), and the runtime

This is Principle 1 (zero hidden behavior) applied to the vendor axis: a format change should not surprise anyone with a hidden Rust release-cycle dependency. It is also Principle 2 (I/O is dumb transport) extending into "domain shape is dumb transport": the daemon does not pretend to know what your logs mean.

### Schema-specific functions live under a schema namespace

Functions that depend on a specific schema specification (syslog, CEF, OTLP, …) live under a dot namespace that names the schema: `syslog.*`, `cef.*`, `otlp.*`. Schema-agnostic primitives — the ones that don't know or care what format the input is — stay flat: `parse_json`, `regex_extract`, `strftime`, `table_lookup`, `csv_parse`, `to_int`, `to_bytes`, `find_by`, …

The judgement rule is a single question: **does the function's behavior follow a specific schema specification?** If yes, the schema's name is part of the function's name. If no, it's a flat primitive.

- `syslog.parse(x)` — behaves according to RFC 3164 / RFC 5424. Schema-specific.
- `cef.parse(x)` — behaves according to the ArcSight CEF specification. Schema-specific.
- `otlp.encode_resourcelog_protobuf(x)` — behaves according to the OTLP/protobuf wire schema. Schema-specific.
- `parse_json(x)` — extracts JSON structure. Doesn't care whether the JSON encodes an event, a metric, or a config blob. Primitive.
- `regex_extract(x, pat)` — runs a regex. No schema awareness at all. Primitive.

This is Principle 1 applied to naming: putting the schema in the name makes the binding explicit and greppable — *"what talks syslog?"* becomes `rg 'syslog\.'`. It also draws a clean line that matches the previous rule: schema-specific helpers go in the snippet library under the namespace they describe; schema-agnostic primitives are part of the daemon's small, stable surface.

---

## How these principles are used

When a feature request, bug report, or design proposal arrives, it is evaluated against these principles first. Proposals that require violating a principle are rejected — not because the proposal is bad, but because limpid is *defined* by the principles. A tool that has these principles plus convenient shortcuts is a different tool.

If you find a principle getting in the way of something you need, the honest path is:

1. Express the need in terms of a primitive the daemon could ship (and that you could then build on top of, in DSL)
2. Propose a new principle-compatible way to achieve the goal
3. Accept that the goal may not fit limpid's scope, and reach for a different tool

Adding a sixth principle is on the table, if it genuinely earns its place. Pre-1.0, all of this is still under negotiation. Post-1.0, these are load-bearing.

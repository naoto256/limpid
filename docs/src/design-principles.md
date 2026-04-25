# Design Principles

limpid is built on five principles. They are non-negotiable and take precedence over feature requests. When a proposal conflicts with a principle, limpid does not take the proposal — even if it is convenient, common in other tools, or widely expected.

These principles exist because the tools limpid replaces (rsyslog, syslog-ng, fluentd) did not fail from missing features — they collapsed under the accumulated weight of hidden behavior, convenience defaults, and special cases. limpid does not take that path.

The first four principles describe the *pipeline* — how events flow, what each layer is allowed to do, what crosses hop boundaries. The fifth describes the *software itself* — the operational stance limpid takes toward running unattended on production paths.

## Principle 1 — Zero hidden behavior

Every transform, mapping, default, and routing decision is expressed in the DSL and observable via `tap`, `inject`, and `--test-pipeline`.

- No implicit parsers that run "because the input looks like JSON"
- No magic templates, convenience defaults, or silent coercion
- No "this field gets filled in for you if you don't set it"
- Verbose but visible is always preferred over terse but hidden

If a pipeline produces an event, you can point to the DSL line that produced it. If a field has a value, you can point to the DSL line that assigned it. There is no fourth layer where the daemon "helpfully" did something on your behalf.

**Why:** hidden behavior is invisible to reviewers, invisible to new team members, invisible to incident responders, and invisible to the person reading the config a year later. Every hidden transform becomes a landmine.

**Consequence:** limpid configs are longer than rsyslog equivalents for the same behavior. That is the intended trade.

## Principle 2 — I/O layer purity

Input and output modules do not touch the contents of an event. Parsing, transformation, and normalization happen exclusively in the `process` layer.

- **Input** modules receive bytes, perform wire-level message framing (boundary detection, LF / octet-count), optionally sanity-check the wire format (e.g. `validate_pri` — shape only), and emit `Event::new(ingress, source)`. They do not extract PRI, do not parse timestamps, do not structure the message. The event enters the pipeline with `ingress` set, `egress = ingress.clone()`, an empty workspace, and the source / receive-time metadata.
- **Output** modules serialize the event's `egress` bytes to the wire. They do not rewrite, transform, lookup, or enrich. An output is a dumb transport.
- **Process** modules do everything else. Parsing, PRI extraction, timestamp derivation, field normalization, OTLP/OCSF mapping, enrichment — all in `process`.

**Why:** when parsing lives in the input, the config reader has no idea which fields exist and which don't. When transformation lives in the output, the same field can mean different things depending on where it is sent. Centralizing all event mutation in `process` makes the pipeline a single, readable transformation — observable at any point with `tap`.

**Consequence:** limpid ships domain-specific parsers as DSL snippets invoked from a `process`, not as behavior baked into an input module (see *Operating rules → Domain knowledge in DSL* below).

## Principle 3 — Only `egress` crosses hop boundaries

In a multi-hop pipeline, the only thing that travels from one limpid daemon to the next is `egress` (the bytes written to the wire). Everything else — `source`, `received_at`, `workspace`, `let` — is derived pipeline-local state that dies at the output.

- `ingress`: the current hop's immutable input. The next hop receives its own `ingress` (whatever it got on the wire, which was the previous hop's `egress`).
- `egress`: the sole hop contract. Assembled in the process layer; serialized by the output.
- `source`: set locally by the receiving input from the remote peer address. Not on the wire.
- syslog facility / severity: not Event fields at all in 0.3 — they are bytes inside the `<PRI>` header at the start of a syslog-framed `egress`. The next hop re-extracts them with `syslog.extract_pri(...)` if it cares; the previous hop set them with `syslog.set_pri(...)` if it needed to.
- `received_at`: pipeline-local — the wall-clock time at which this hop received the event. The wire-side event timestamp, when present (e.g. RFC 5424's TIMESTAMP field), lives inside `egress` and is parsed into workspace by `process` snippets — never overwritten onto `Event.received_at` by inputs (Principle 2: input is dumb transport).
- `workspace` / `let`: pipeline-local scratch. See [Event Model](./processing/README.md).

**Why:** this is the natural consequence of Principle 2. When the wire contract is just `egress` bytes, no metadata can silently drift between daemons, no schema can grow incompatible across versions, and no "attribute" can mean one thing on one hop and another thing on the next. Each hop re-derives what it needs from the bytes it received.

**Consequence:** limpid multi-hop pipelines are simple to reason about. The contract is auditable — it's whatever you construct in `egress`. There is no hidden sidecar of "things that also travel".

## Principle 4 — Atomic events through the pipeline

The unit flowing through a limpid pipeline is one event = one logical record. The pipeline never operates on bundles, never fans out, never aggregates.

- **Inputs** split: where the wire format bundles records (an OTLP `ExportLogsServiceRequest` carrying ten `LogRecord`s, a Kafka batch, an HTTP request body of NDJSON), the input decomposes the bundle at the transport boundary into one Event per record. Resource and Scope context, when present, is preserved per Event by carrying it inside `ingress`.
- **Process** snippets pass through: a `process` body either mutates the in-flight event, drops it (`drop`), or terminates the pipeline for that event (`finish`). It never emits a second event from a single input. Cardinality only ever decreases inside the pipeline.
- **Outputs** bundle: where the destination benefits from batching (collector → SaaS, NDJSON to Elasticsearch, Kafka producer batches), the output rebuilds the wire-level bundle at the emit boundary, controlled by `batch_size` / `batch_timeout` and (for OTLP) `batch_level`.

**Why:** keeping events atomic through the pipeline is the foundation that the rest of the daemon assumes. Replay through `inject --json` works because every captured event is independent. `tap --json` is line-per-event because the pipeline is event-per-step. The queue's redelivery and disk-WAL semantics are tractable because each unit of work is one record. Snippet authors don't have to reason about state across events because there is no across-events state inside `process`. Stateful aggregation (group-by, summarize-over-window) is unnecessary at the process layer because output's `batch_size` / `batch_timeout` already do bundling at the only place where bundling has meaning — the wire boundary.

The economic shape of the trade — input pays CPU to undo upstream's wire-bundling, output pays CPU to rebundle on emit — is discussed at length in [OTLP — design rationale](./otlp.md), where it shows up most visibly.

**Consequence:** a snippet that wants to "split one log line into multiple sub-events" cannot do so in `process`. The correct shape is to write a custom input that performs the split at the wire boundary. A snippet that wants to "wait for N events and emit a summary" cannot do so in `process` either; that is what the output's batching layer is for.

## Principle 5 — Safety and operational transparency

A log forwarder runs unattended on production paths, and any mistake compounds quickly — lost events, broken downstream parsers, blind monitoring, slow root-cause analysis. limpid is built so that every operationally-risky step has a *verify-before*, *observe-during*, *undo-after*, or *replay-later* path attached. The operator is never blind to what the daemon is doing, and never out of options when something goes wrong.

- **Verify before deployment.** `limpid --check` runs rustc-style static analysis with type checking, "did you mean" suggestions, and a flow graph (Mermaid / DOT / ASCII). Type errors in DSL snippets, unknown identifiers, unreachable pipelines — all rejected at parse time, before the daemon binds a single socket.

- **Observe during runtime.** `limpidctl tap` streams events at any named hop in any pipeline — inputs, outputs, named processes — in raw bytes mode or full Event JSON mode. The pipeline does not pause and traffic is not duplicated; the tap is a live cut into a running system. The Prometheus exporter publishes per-module counters (`events_received`, `events_invalid`, `events_written`, `events_failed`) so dashboards reflect what is actually happening, not what the operator thinks should be. Together with Principle 1's zero hidden behavior in *config*, Principle 5 enforces zero hidden behavior in the *running system*.

- **Replay later for investigation.** `tap --json` emits the same shape `inject --json` consumes, so yesterday's traffic can be re-run through today's configuration to confirm a bug fix or validate a refactor. `--test-pipeline` does the same with a single sample event, fast enough for CI.

- **Undo after deployment.** `SIGHUP` swaps configuration atomically. A reload whose new config fails to parse, fails type checking, or fails to start is rolled back transparently; the operator sees a diagnostic and the previous configuration keeps running. The daemon never enters a half-loaded state.

- **Fail soft, surface clearly.** Output queues have retry with exponential backoff, secondary fallback for dead-letter routing, and optional disk-WAL persistence so a crash does not lose events in flight. `Drop` hooks emit a count of events still buffered at shutdown rather than letting them disappear silently.

**Why:** a daemon that processes production traffic 24/7 cannot afford a *"well, redeploy and hope"* recovery story, and an opaque daemon cannot be operated safely even when it is technically working. The cost of these affordances — more code paths, more CLI surface, more documentation, more API — is paid up front. The benefit is paid every time something goes wrong, and every time an operator wonders *"is the pipeline doing what I think it is?"* without that question being a costly investigation.

**Consequence:** features that cannot be verified, observed, undone, or replayed do not ship. A "fast path" that skips static analysis, an inline transformation that cannot be captured by `tap`, a config option that takes effect without a rollback handshake — all rejected at design review, before any code is written. The pre-1.0 breaking changes that limpid has accepted (the v0.3 Event-model rename, the v0.5 `Event.timestamp` → `received_at` rename) are themselves expressions of this principle: breaking now to converge on names and shapes that will not need to break at 1.0, where breaking is expensive forever.

---

## Operating rules

These are not principles — they are concrete consequences of the five principles above, written down so they are easy to cite. Unlike principles, rules can shift as the project learns; the principles cannot.

### Domain knowledge ships as DSL snippets, not Rust

Typical parsing patterns — OTLP normalization, OCSF mapping, Windows Snare parsing, Apache / nginx / Cisco ASA / FortiGate log formats — are not built-in Rust modules. They are DSL snippets shipped under `processes/*.limpid` and `inputs/*.limpid` that users `include` into their configuration.

- Users can include them as-is, modify them, ignore them, or replace them
- A specification change (e.g. OTLP version update) is a DSL edit, not a Rust release
- Adding support for a new vendor format is a pull request to a snippet, not a feature in the daemon
- The daemon itself ships only **primitives**: the wire-speaking inputs / outputs, the generic functions (`parse_kv`, `regex_extract`, `table_lookup`), and the runtime

This is Principle 1 (zero hidden behavior) applied to the vendor axis: a format change should not surprise anyone with a hidden Rust release-cycle dependency. It is also Principle 2 (I/O is dumb transport) extending into "domain shape is dumb transport": the daemon does not pretend to know what your logs mean.

### Schema-specific functions live under a schema namespace

Functions that depend on a specific schema specification (syslog, CEF, OCSF, …) live under a dot namespace that names the schema: `syslog.parse`, `cef.parse`, `otlp.encode_resourcelog_protobuf`, `ocsf.*`. Schema-agnostic primitives — the ones that don't know or care what format the input is — stay flat: `parse_json`, `regex_extract`, `strftime`, `table_lookup`, `csv_parse`, `to_int`, `to_bytes`, `find_by`, …

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

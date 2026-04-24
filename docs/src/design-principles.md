# Design Principles

limpid is built on five principles. They are non-negotiable and take precedence over feature requests. When a proposal conflicts with a principle, limpid does not take the proposal — even if it is convenient, common in other tools, or widely expected.

These principles exist because the tools limpid replaces (rsyslog, syslog-ng, fluentd) did not fail from missing features — they collapsed under the accumulated weight of hidden behavior, convenience defaults, and special cases. limpid does not take that path.

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

**Consequence:** limpid ships domain-specific parsers (see Principle 3), but they are DSL snippets invoked from a `process`, not behavior baked into an input module.

## Principle 3 — Domain knowledge ships as DSL snippets, not Rust

Typical parsing patterns — OTLP normalization, OCSF mapping, Windows Snare parsing, Apache / nginx / Cisco ASA / FortiGate log formats — are not built-in Rust modules. They are DSL snippets shipped under `processes/*.limpid` and `inputs/*.limpid` that users `include` into their configuration.

- Users can include them as-is, modify them, ignore them, or replace them
- A specification change (e.g. OTLP version update) is a DSL edit, not a Rust release
- Adding support for a new vendor format is a pull request to a snippet, not a feature in the daemon
- The daemon itself ships only **primitives**: the wire-speaking outputs, the generic functions (`parse_kv`, `regex_extract`, `table_lookup`), and the runtime

**Why:** domain knowledge has a half-life. Log formats change. Standards evolve. Vendor fields rename. When domain knowledge is baked into Rust, every format change requires a daemon release. When it is a DSL snippet, the user edits it on their own schedule.

**Consequence:** the daemon stays small. The DSL does more work. New formats are additions to a snippet library, not changes to the core.

## Principle 4 — Only `egress` crosses hop boundaries

In a multi-hop pipeline, the only thing that travels from one limpid daemon to the next is `egress` (the bytes written to the wire). Everything else — `source`, `timestamp`, `workspace`, `let` — is derived pipeline-local state that dies at the output.

- `ingress`: the current hop's immutable input. The next hop receives its own `ingress` (whatever it got on the wire, which was the previous hop's `egress`).
- `egress`: the sole hop contract. Assembled in the process layer; serialized by the output.
- `source`: set locally by the receiving input from the remote peer address. Not on the wire.
- syslog facility / severity: not Event fields at all in 0.3 — they are bytes inside the `<PRI>` header at the start of a syslog-framed `egress`. The next hop re-extracts them with `syslog.extract_pri(...)` if it cares; the previous hop set them with `syslog.set_pri(...)` if it needed to.
- `timestamp`: the wire-side timestamp, when present, lives inside `egress` (e.g. RFC 5424's TIMESTAMP field). `Event.timestamp` is pipeline-local — the time at which this hop received the event.
- `workspace` / `let`: pipeline-local scratch. See [Event Model](./processing/README.md).

**Why:** this is the natural consequence of Principle 2. When the wire contract is just `egress` bytes, no metadata can silently drift between daemons, no schema can grow incompatible across versions, and no "attribute" can mean one thing on one hop and another thing on the next. Each hop re-derives what it needs from the bytes it received.

**Consequence:** limpid multi-hop pipelines are simple to reason about. The contract is auditable — it's whatever you construct in `egress`. There is no hidden sidecar of "things that also travel".

## Principle 5 — Schema identity is declared by namespace

Functions that depend on a specific schema specification (syslog, CEF, OCSF, …) live under a dot namespace that names the schema: `syslog.parse`, `cef.parse`, `ocsf.map`. Schema-agnostic primitives — the ones that don't know or care what format the input is — stay flat: `parse_json`, `regex_extract`, `strftime`, `table_lookup`.

The judgement rule is a single question: **does the function's behavior follow a specific schema specification?** If yes, the schema's name is part of the function's name. If no, it's a flat primitive.

- `syslog.parse(x)` — behaves according to RFC 3164 / RFC 5424. Schema-specific.
- `cef.parse(x)` — behaves according to the ArcSight CEF specification. Schema-specific.
- `parse_json(x)` — extracts JSON structure. Doesn't care whether the JSON encodes an event, a metric, or a config blob. Primitive.
- `regex_extract(x, pat)` — runs a regex. No schema awareness at all. Primitive.

**Why:** reading a config should tell you exactly which specifications the pipeline is bound to. When a syslog parser hides behind a generic name like `parse()`, the reader has to know out-of-band which RFC is being followed; an upgrade that switches parsers is invisible at the call site. Putting the schema in the name makes the binding explicit and greppable — "what talks syslog?" becomes `rg 'syslog\\.'`.

It also draws a clean line for Principle 3 (domain knowledge as DSL snippets). Schema-specific DSL snippets can be grouped under the namespace they describe; schema-agnostic primitives belong to the daemon. The namespace boundary is the same boundary as the "ships in Rust vs. ships in DSL" line, just made visible in the grammar.

**Consequence:** v0.3.0 completes the migration of the schema-specific helpers shipped with the daemon — `parse_syslog` / `parse_cef` / `strip_pri` are gone, replaced by `syslog.parse` / `cef.parse` / `syslog.strip_pri` / `syslog.set_pri` / `syslog.extract_pri`. The native process layer is removed at the same time, so a `process` body in 0.3 is purely DSL: function calls, assignments, control flow.

---

## How these principles are used

When a feature request, bug report, or design proposal arrives, it is evaluated against these principles first. Proposals that require violating a principle are rejected — not because the proposal is bad, but because limpid is defined by the principles. A tool that has these principles plus convenient shortcuts is a different tool.

If you find a principle getting in the way of something you need, the honest path is:

1. Express the need in terms of a primitive the daemon could ship (and that you could then build on top of, in DSL)
2. Propose a new principle-compatible way to achieve the goal
3. Accept that the goal may not fit limpid's scope, and reach for a different tool

Adding a fifth principle is also on the table, if it genuinely earns its place. Pre-1.0, all of this is still under negotiation. Post-1.0, these are load-bearing.

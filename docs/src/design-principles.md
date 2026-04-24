# Design Principles

limpid is built on four principles. They are non-negotiable and take precedence over feature requests. When a proposal conflicts with a principle, limpid does not take the proposal — even if it is convenient, common in other tools, or widely expected.

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

- **Input** modules receive bytes, perform wire-level message framing (boundary detection, LF / octet-count), optionally sanity-check the wire format (e.g. `validate_pri` — shape only), and emit `Event::new(ingress, source)`. They do not extract PRI, do not parse timestamps, do not structure the message, do not populate `facility` / `severity`. The event enters the pipeline with `ingress` set, `egress = ingress.clone()`, and everything else empty or default.
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

In a multi-hop pipeline, the only thing that travels from one limpid daemon to the next is `egress` (the bytes written to the wire). Everything else — `source`, `facility`, `severity`, `timestamp`, `workspace`, `let` — is derived pipeline-local state that dies at the output.

- `ingress`: the current hop's immutable input. The next hop receives its own `ingress` (whatever it got on the wire, which was the previous hop's `egress`).
- `egress`: the sole hop contract. Assembled in the process layer; serialized by the output.
- `source`: set locally by the receiving input from the remote peer address. Not on the wire.
- `facility` / `severity`: encoded in the `<PRI>` byte at the start of a syslog-framed `egress`. The next hop re-extracts them from the incoming bytes. They are not independent fields that travel.
- `timestamp`: the wire-side timestamp, when present, lives inside `egress` (e.g. RFC 5424's TIMESTAMP field). `Event.timestamp` is pipeline-local.
- `workspace` / `let`: pipeline-local scratch. See [Event Model](./processing/README.md).

**Why:** this is the natural consequence of Principle 2. When the wire contract is just `egress` bytes, no metadata can silently drift between daemons, no schema can grow incompatible across versions, and no "attribute" can mean one thing on one hop and another thing on the next. Each hop re-derives what it needs from the bytes it received.

**Consequence:** limpid multi-hop pipelines are simple to reason about. The contract is auditable — it's whatever you construct in `egress`. There is no hidden sidecar of "things that also travel".

---

## How these principles are used

When a feature request, bug report, or design proposal arrives, it is evaluated against these principles first. Proposals that require violating a principle are rejected — not because the proposal is bad, but because limpid is defined by the principles. A tool that has these principles plus convenient shortcuts is a different tool.

If you find a principle getting in the way of something you need, the honest path is:

1. Express the need in terms of a primitive the daemon could ship (and that you could then build on top of, in DSL)
2. Propose a new principle-compatible way to achieve the goal
3. Accept that the goal may not fit limpid's scope, and reach for a different tool

Adding a fifth principle is also on the table, if it genuinely earns its place. Pre-1.0, all of this is still under negotiation. Post-1.0, these are load-bearing.

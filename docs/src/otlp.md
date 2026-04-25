# OTLP in limpid — design rationale

This page exists so that an issue starting with *"why doesn't limpid set
`service.name` automatically?"* or *"OTLP says X, you should do X,"* can
be answered with a link instead of a thread. The OTLP specification is
public, several major implementations interpret it differently, and the
community has well-formed factions on a handful of points. limpid took
positions on each of those points; this page is where those positions
are written down with the reasoning attached.

The reference docs for *how to use* OTLP in limpid are elsewhere:

- [`otlp_http`](./inputs/otlp-http.md) and
  [`otlp_grpc`](./inputs/otlp-grpc.md) — input transports
- [`otlp` output](./outputs/otlp.md) — three-protocol sender
- [`otlp.*` functions](./processing/functions.md#otlp---opentelemetry-protocol-logs-signal) —
  encode / decode primitives

The general design philosophy of limpid (the five principles) lives in
[Design Principles](./design-principles.md). This page assumes you've
read that and explains the OTLP-specific reading on top.

---

## 1. Scope of v0.5.0

| Aspect | v0.5.0 |
|---|---|
| Signal | **logs** only — no traces, no metrics, no profiles |
| Transports | HTTP/JSON, HTTP/protobuf, gRPC (all three) |
| Direction | input *and* output (so collector-to-collector relay works) |
| TLS | server-side TLS / mTLS on `otlp_grpc`; HTTP server TLS queued for v0.5.x |
| Versioning | OTLP 1.4 wire (the proto3 schema as of opentelemetry-proto 0.27) |

Traces and metrics share the same wire envelope shape but use different
proto messages, so the input / output skeleton from logs is reusable.
v0.5.0 ships logs first because that is where limpid's existing pipeline
identity lives — every other limpid module produces or consumes log
records.

---

## 2. The OTLP wire, briefly

OTLP carries logs in a three-tier hierarchy:

```
ExportLogsServiceRequest
  └─ resource_logs[]: ResourceLogs        (who emitted: identity / entity)
       ├─ resource: Resource              (attributes describing the source)
       └─ scope_logs[]: ScopeLogs         (which library / module)
            ├─ scope: InstrumentationScope
            └─ log_records[]: LogRecord   (the events themselves)
```

The intent is that one batch can carry:

- *N* Resources (different services / hosts) under one envelope
- under each Resource, *M* Scopes (different libraries inside that
  service)
- under each Scope, the actual log records

This lets a sender deduplicate Resource and Scope metadata across many
records — useful when a single library inside a single service is
emitting a burst.

Two structural facts of proto3 matter for the rest of this page:

1. **`repeated` fields concat = merge.** Sending one `ResourceLogs` with
   ten records and sending ten singleton `ResourceLogs` produce the
   same set of records at the receiver; only the framing differs. The
   spec calls this out explicitly.
2. **The schema is immutable on the wire.** Fields are tagged; adding
   new ones is backward-compatible by design.

The `Export` RPC returns an `ExportLogsServiceResponse` whose only
field is an optional `partial_success` carrying:

```protobuf
ExportLogsPartialSuccess {
    int64  rejected_log_records = 1;
    string error_message        = 2;
}
```

This is the protocol's mechanism for the receiver to say *"I accepted N
of M records; here is the count of the ones I refused, and a message."*
The intent and the actual usage diverge, which is the topic of §4.

---

## 3. Where the spec is clear

A few points the spec *is* explicit about, where casual readings get
them wrong. limpid's behaviour follows the spec on these.

### 3.1 External Logs: Resource describes the source, not the agent

The OTLP Logs Data Model has a section titled
[*"External Logs"*](https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/logs/data-model.md#how-opentelemetry-handles-external-logs)
that addresses the case limpid is in:

> When receiving logs from sources outside the SDK, the Resource should
> describe **the source of the logs, not the agent that collected
> them.**

`service.name` in this case is the application / device that produced
the log line, not the limpid daemon that received it on UDP/514. The
reading is unambiguous. limpid follows it (see §5.4 on why this means
limpid does not auto-set the value).

### 3.2 SeverityNumber is a number, not text

`severity_number` is a 1..24 enum with the canonical level values at
1, 5, 9, 13, 17, 21 (TRACE, DEBUG, INFO, WARN, ERROR, FATAL) and three
intermediate slots in each level (`*2/*3/*4`) for systems with finer
granularity. `severity_text` is a separate string field for the
original, human-readable level token from the source.

Both are independent. A receiver that wants to filter by severity
should look at `severity_number`; one that wants to display the
original token uses `severity_text`. limpid emits both when both are
known and otherwise leaves the unknown one empty (see §5.5).

### 3.3 partial_success is *not* a retry primitive

The spec is quiet on what a sender should do with `rejected_log_records`,
but it gives the field a clear definition:

> The number of rejected log records. A `rejected_log_records` of `0`
> indicates full success.

Being *rejected* is a terminal state for those records as far as the
receiver is concerned: they were validated, found unacceptable, and
dropped. This is distinct from a transport failure (5xx, connection
refused, timeout) where the receiver never had a chance to validate.

The OpenTelemetry Collector's `otlp` exporter explicitly does not retry
the rejected subset on its own; it surfaces the partial-success in
metrics and (depending on configuration) propagates the failure to its
own sender. limpid does the same — see §5.6.

---

## 4. Where the spec is ambiguous, and the ecosystem disagrees

These are the points where a community member writing an issue is
most likely to assume a different interpretation than limpid's. Each
gets its own subsection.

### 4.1 Whose attributes go on the Resource

The "Resource describes the source" rule above is unambiguous *if* you
know the source's identity. In a real forwarder the identity has to
come from somewhere. There are three popular sources:

| Source | What it produces | Used by |
|---|---|---|
| Auto-detection | `host.name = $(hostname)`, `service.name` from a config or env | OTel Collector receivers, most SDKs |
| Per-source mapping | `service.name = workspace.cef_device_vendor` (computed per Event) | bespoke pipelines, CEF/syslog forwarders |
| Hand-authored | `resource { attributes [...] }` block in the config | this is uncommon, but it's what limpid expects |

The OTel Collector's `host` and `resource` processors lean heavily on
auto-detection — it Just Works for the common case where one collector
runs on one host serving one service. It does the wrong thing for a
forwarder that aggregates dozens of source devices: every record
inherits the *forwarder's* `host.name`, contradicting External Logs
guidance.

The community has not converged. limpid takes the position that the
forwarder doesn't know enough at the input layer to make this call
correctly, and pushes it into the snippet (§5.4).

### 4.2 What goes in `body`

`LogRecord.body` is an `AnyValue` — it can be `string`, `bool`, `int`,
`double`, `bytes`, an array, or a kvlist (nested map). The spec
permits all of these, and different ecosystems use different shapes:

- **The fluent ecosystem** (fluentd, fluent-bit) tends to put a
  flat-or-nested key/value structure as `kvlist_value`, treating
  `body` as the primary parsed payload.
- **The OTel SDK ecosystem** tends to put a human-readable line as
  `string_value` and reserve attributes for structured metadata.
- **The "log-as-JSON" ecosystem** (most cloud platforms) puts a JSON
  string in `string_value` because their backends parse it
  downstream regardless.

limpid does not pick one. The DSL snippet builds whatever AnyValue
shape the destination expects. See §5.7 for the bridging convention.

### 4.3 Whether the originating timestamp is in `time_unix_nano` or `observed_time_unix_nano`

`LogRecord` has two timestamp fields:

| Field | Defined as |
|---|---|
| `time_unix_nano` | When the event occurred, as claimed by the source |
| `observed_time_unix_nano` | When the receiver observed the event |

For a forwarder, "the source" is the upstream device that produced
the syslog / cef / kafka message; "the receiver" is whichever
component is currently holding the Event. Many implementations
collapse them — the OTel Collector's `journald` receiver, for
example, sets only `time_unix_nano` from the journal entry's
`__REALTIME_TIMESTAMP` and leaves `observed_time_unix_nano` empty.

limpid's snippet convention is:

- `time_unix_nano = workspace.event_time` (the source-claimed time
  the parser extracted from the wire — `syslog_timestamp`, `cef_rt`,
  etc.)
- `observed_time_unix_nano = received_at` only when the snippet
  explicitly chooses to populate it; not auto-set

The spec is comfortable with this split; the practice in the wild is
not consistent.

### 4.4 What `Scope` means for forwarded logs

`InstrumentationScope` is "the *library* that emitted the log." For an
SDK this is meaningful — `io.opentelemetry.slf4j` vs `okhttp3`, etc.
For a forwarder receiving syslog from a network device, there is no
library. Implementations handle this differently:

- **Skip it.** Some receivers leave `scope` unset, producing a
  `ScopeLogs` with `scope: null`. This is technically valid but most
  backends flag it.
- **Synthesise from the receiver name.** OTel Collector's
  `filelog` receiver sets `scope.name = "filelog"`,
  `scope.version = collector version`. Functional but conveys nothing
  about the actual log.
- **Synthesise from the parser.** Some pipelines set
  `scope.name = "syslog"` or the vendor (`"fortinet.fortigate"`).
  This is more useful for filtering downstream.

limpid's input modules synthesise minimally: they emit a singleton
ScopeLogs with no scope data on the wire (`scope: None` in the proto)
and let the snippet author decide whether to populate it during
composition. The output simply forwards what the snippet built.

### 4.5 Concat vs merge in batches (`batch_level`)

Because `repeated ResourceLogs` is concat-equals-merge on the wire,
a sender has freedom: send one `ResourceLogs` per Event, or merge
same-Resource Events into one entry, or merge same-(Resource, Scope)
into one ScopeLogs. All three produce the same set of records at the
receiver.

The OTel Collector's exporter merges aggressively (smallest wire form).
Some bespoke senders do pure concat (smallest CPU). limpid offers
all three as `batch_level = none | resource | scope` and documents
that they are semantically identical at the wire. See [§ batch_level
on the output reference page](./outputs/otlp.md#batch_level) for the
operational tradeoff.

The reason this is in §4 (ambiguous) rather than §3 (clear) is that
the spec does not *require* either form, but a strict reading of "a
batch is a set of records grouped by Resource and Scope" makes the
merged form feel more natural, and some receivers' debug logs assume
it.

---

## 5. limpid's positions

Each position is named, defended, and cross-linked to where it shows
up in the code or config. Readers who want to argue for a different
position know exactly where to look.

### 5.1 One LogRecord = one Event

A wire request carrying *N* LogRecords becomes *N* Events on the
limpid pipeline. The input splits along the LogRecord axis at the
moment of receive; Resource and Scope context is preserved by
constructing a *singleton* ResourceLogs (1 Resource + 1 Scope + 1
LogRecord) per Event and writing it to `ingress`.

**Why one-record granularity.** Every other limpid input does the
same: a syslog UDP packet is one Event, a CEF line is one Event, a
journal entry is one Event. Pipelines, snippets, the queue, the
WAL, `tap --json` — all assume one Event = one record. Treating an
OTLP envelope as a single Event would create a second mode of
operation that none of the rest of limpid speaks. The DSL would
need envelope-aware semantics; the queue would need envelope-or-record
batching; `tap` would need to display ten records as one line. None
of that is justified for a logs use case where per-record routing /
filtering / enrichment is the common operation.

**Why not "envelope mode" as an option.** A relay use case ("forward
unchanged") could in principle bypass the per-record split and pass
the envelope through. The argument against: it doesn't pay off.
With per-record split *plus* a no-op pipeline, the envelope is
reconstructed by the output's batch + `batch_level=scope` path with
the same wire result. The cost is the per-record cycle through the
queue, which for OTLP-relay traffic is a few microseconds per
record. Not worth a second mode.

### 5.2 `egress` is the singleton ResourceLogs proto bytes

When an Event leaves a hop, it carries `egress` — the bytes the
output writes to the wire. For OTLP pipelines this is the
proto3-encoded singleton ResourceLogs. The next hop receives those
bytes as `ingress` and can either pass them through unchanged
(pure relay) or decode them with `otlp.decode_resourcelog_protobuf`
to inspect / mutate the contents.

**Why bytes, not a structured value.** Principle 4 of limpid's
design says "only `egress` crosses hop boundaries." The bytes are
already the canonical wire form; storing the decoded struct *and*
re-encoding at every hop wastes work and creates a chance of
drift between the in-memory and on-the-wire representations.
Bytes on the hop, decode-on-demand in the snippet.

**Why protobuf, not JSON.** OTLP/JSON is a real wire format (the
`http_json` protocol) but the protobuf form is more compact, decodes
faster, and is the canonical form in the spec. The output transport
re-encodes on the way out if the configured protocol is `http_json`.

### 5.3 `Event.received_at` is wall-clock, not source time

`Event.received_at` is set by the input to `Utc::now()` at the moment
the wire bytes arrived. The input does not parse the body to extract
a source-claimed time and overwrite it. This is Principle 2 of
limpid's design: input is dumb transport; payload semantics belong
to the process layer.

Source-claimed times surface in the workspace through parser snippets:

- `syslog.parse` writes `workspace.syslog_timestamp`
- `cef.parse` writes `workspace.cef_rt`
- vendor-specific parsers (Palo Alto's CSV format etc.) write to
  whatever workspace field the snippet author chooses

A composer snippet then chooses what `LogRecord.time_unix_nano` should
be — typically the source-claimed time, falling back to
`received_at` when the source did not provide one.

The semantic separation is the point: a forwarder does not silently
rewrite `received_at` as if the wall clock and the source clock were
interchangeable. This is the rename that v0.5.0 committed to (see
the breaking change in [CHANGELOG.md](../../CHANGELOG.md) and
the [v0.5 upgrade notes](./operations/upgrade-0.5.md)).

### 5.4 Resource attributes are user-authored

limpid does **not** auto-detect `host.name`, `service.name`,
`os.type`, or any other Resource attribute. The `resource { ... }`
block in the OTLP output (and the per-Event `resource` field in
the snippet's HashLit) is the only source of those values.

**Why no auto-detect.** As §4.1 notes, the OTel Collector's
auto-detect is correct for one common case (one collector =
one host = one service) and silently wrong for the case limpid
exists for (one forwarder = many sources). A device-aggregating
forwarder that auto-set `host.name` to its own hostname would
violate External Logs guidance for every record it emits.

The right value comes from the parser: a CEF line carries the
device hostname in `dvchost`; a syslog line carries the source
in the HOSTNAME field; a Kafka record carries it as a key.
A snippet extracts that and writes it into the Resource attributes
the snippet builds, per record. The forwarder's identity (who
relayed) is unrelated and uninteresting; the source's identity (who
emitted) is what `host.name` and `service.name` should describe.

**The config-block fallback.** Configs that only ever forward one
service's logs can hand-author the Resource block; the snippet then
produces records that all share the same Resource and `batch_level=resource`
collapses them efficiently on the wire. Both modes are first-class.

### 5.5 SeverityNumber mapping is in snippets, not Rust

Mapping syslog priorities → OTLP `severity_number`, or CEF severity
strings → OTLP `severity_number`, is a per-source decision: a CEF
producer's "High" means OTLP 13 (WARN) for some vendors and OTLP 17
(ERROR) for others. limpid does not bake any mapping into Rust;
snippets carry the table.

The reference snippet library (queued for v0.6.0) will ship
opinionated mappings for common vendors. Until then, snippet authors
write the table inline.

### 5.6 Retry: transport-level only

The output module retries the *whole* `ExportLogsServiceRequest` on
hard transport failures (connection refused, 5xx, gRPC `UNAVAILABLE`,
timeout). It does **not** retry just the rejected subset surfaced
through `partial_success`.

**Why.** As §3.3 notes, `rejected_log_records` is a terminal state
for those records — the receiver validated and refused them. Retrying
doesn't change validity; it would just re-deliver the same records
to the same receiver and get the same rejection. A "selective
retry" mechanism would imply the rejection was transient, which the
spec does not say.

**What's queued.** The OTel Collector has an `otlp` exporter mode
where rejected records get logged-and-dropped, and another where they
trigger the whole batch's failure path. limpid currently does the
former. A configurable selective-drop / dead-letter mode is queued
for v0.5.x; the transport retry that landed in v0.5.0 is the
"hard failure" half.

### 5.7 Body format is the snippet's call

OTLP `body` accepts string / int / bool / double / bytes / array /
kvlist. limpid's snippet author chooses, per pipeline:

- **string** for a JSON-encoded payload (typical when downstream
  parses on its end — most cloud SaaS backends)
- **string** for a human-readable line (typical for archival / search)
- **kvlist** for structured composition where the receiver natively
  understands the OTLP attribute model (the OTel-native path)

A common pattern for cloud-bound pipelines is to compose an OCSF
record in `workspace.ocsf` and ship it as
`body: { string_value: to_json(workspace.ocsf) }`. This matches what
most cloud backends expect, lets the OCSF schema do the structuring
work, and avoids fighting the OTLP attribute namespace.

---

## 6. What limpid intentionally does *not* do

These are not "queued for v0.5.x" — they are out of scope for
limpid's identity as a forwarder. Issues asking for them will be
closed with a link here.

### 6.1 SDK semantics

limpid is not an OpenTelemetry SDK. It does not instrument code,
does not attach to a process to collect telemetry, does not provide
a logging facade. It receives bytes on a socket and forwards bytes
out a socket. The OTel SDKs do the SDK work; limpid does the
forwarder work.

### 6.2 Trace context auto-injection

`LogRecord.trace_id` and `span_id` connect a log record to an active
trace. If the source provides them (in the syslog payload, in a
header, in a Kafka key), a snippet can write them through. limpid
does not synthesise them and does not maintain a trace context.

### 6.3 Service identity auto-detection

Restated for clarity: see §5.4. No `hostname()`-as-`host.name`,
no `cargo_pkg_version()`-as-`service.version`, no `$HOSTNAME` env
fallback. The snippet decides.

### 6.4 Schema URL inference

`ResourceLogs.schema_url` and `ScopeLogs.schema_url` are optional
fields pointing at an OpenTelemetry Schema URL (e.g.,
`https://opentelemetry.io/schemas/1.27.0`). Most backends ignore
them. limpid leaves them empty. A future config-level
`schema_url "..."` directive is plausible if it becomes a real
ask; it is not v0.5.0.

---

## 7. Pre-empted FAQs

### *"Why doesn't `service.name` show up in my OTLP output?"*

Because no snippet wrote it. limpid will not auto-detect or default
the field. The composer snippet must include it in the Resource
attributes, typically extracted from a parser field like
`workspace.cef_device_vendor` or hand-coded in the config's
`resource { ... }` block. See §5.4.

### *"Why is my body a JSON string instead of structured attributes?"*

Because the snippet built it that way. Either change the snippet to
emit `body: { kvlist_value: { ... } }`, or recognise that the
JSON-string path is intentional for cloud-SaaS-bound pipelines (§5.7).

### *"OTLP says I should retry rejected records — why doesn't limpid?"*

OTLP doesn't say that. It says receivers can report
`rejected_log_records` to indicate they refused some subset. The
sender's behaviour on receiving that report is unspecified, and the
"retry just the rejects" interpretation contradicts the
field's terminal-state semantics. See §3.3 and §5.6.

### *"Why do I have to author Resource attributes? OTel Collector handles this for me."*

Because OTel Collector's auto-detection is correct for one common
deployment shape and wrong for limpid's primary one (multi-source
forwarder). See §4.1 and §5.4. If you only forward one service's
logs, hand-author the `resource { ... }` block once and it stops
being a per-record concern.

### *"Why is `received_at` not the event time?"*

Because the event time is what the source claimed, not what the
forwarder observed. The two are not the same thing, and conflating
them was the v0.5.0 breaking change that motivated the rename. See
§5.3 and the [v0.5 upgrade notes](./operations/upgrade-0.5.md).

### *"Can I send Resource attributes from the input layer?"*

Not in v0.5.0. Inputs do not interpret payloads (Principle 2). The
parser snippet that runs in the process layer extracts the source's
identity into workspace fields; the composer snippet then writes
them into the Resource. If the same one-Resource pipeline is
common, hand-author the config's `resource { ... }` block once and
the snippet just references it.

### *"limpid is not OTel-conformant because it does X / does not do Y."*

There is no single "OTel-conformant" definition for a forwarder
component. The OpenTelemetry project ships a SDK conformance suite
(for instrumentation libraries) and a Collector receiver test suite
(for receivers); neither targets a forwarder/relay use case. limpid
implements the OTLP wire protocol fully and follows the spec where
it is unambiguous (§3) and reasoned positions where it is
ambiguous (§4 → §5). If you have a *specific* spec citation that
limpid contradicts, that is a real bug — open an issue with the
section reference and the wire trace.

---

## 8. Where to look for what

| Question | Source |
|---|---|
| How do I configure the input / output? | [otlp_http](./inputs/otlp-http.md), [otlp_grpc](./inputs/otlp-grpc.md), [otlp output](./outputs/otlp.md) |
| What primitives are in the `otlp.*` namespace? | [Expression Functions](./processing/functions.md#otlp---opentelemetry-protocol-logs-signal) |
| How do I migrate from `Event.timestamp` to `Event.received_at`? | [v0.5 upgrade notes](./operations/upgrade-0.5.md) |
| What are the design principles this builds on? | [Design Principles](./design-principles.md) |
| What changed in v0.5.0 specifically? | [CHANGELOG](../../CHANGELOG.md) |

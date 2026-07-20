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
- [`otlp_http` output](./outputs/otlp_http.md) / [`otlp_grpc` output](./outputs/otlp_grpc.md) — HTTP and gRPC senders
- [`otlp.*` functions](./functions/expression-functions.md#otlp---opentelemetry-protocol-logs-signal) —
  encode / decode primitives

The general design philosophy of limpid (the five principles) lives in
[Design Principles](./design-principles.md). This page assumes you've
read that and explains the OTLP-specific reading on top.

---

## 1. Scope

| Aspect | Current (0.7.15) |
|---|---|
| Signal | **logs** only — no traces, no metrics, no profiles |
| Transports | HTTP/JSON, HTTP/protobuf, gRPC (all three; output side split into `output otlp_http` / `output otlp_grpc` introduced in 0.7.6; in 0.7.8 plaintext `http://` with `tls { ... }` is now rejected at parse time — see CHANGELOG 0.7.8) |
| Direction | input *and* output (so collector-to-collector relay works) |
| TLS | server-side TLS / mTLS on every input (`otlp_http`, `otlp_grpc`); client-side TLS / mTLS on every output (per-peer `tls { ca cert key }` on both `output otlp_http` and `output otlp_grpc`; introduced in 0.7.6, with 0.7.8 adding fail-fast rejection of plaintext `http://` URLs that pair a `tls { ... }` block) |
| Versioning | OTLP 1.10 wire (the proto3 schema as of opentelemetry-proto 0.32) |

Traces and metrics share the same wire envelope shape but use different
proto messages, so the input / output skeleton from logs is reusable.
The 0.5.x line shipped logs first because that is where limpid's existing
pipeline identity lives — every other limpid module produces or consumes
log records.

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

| Source | What it produces | limpid policy |
|---|---|---|
| Auto-detection | `host.name = $(hostname)`, `service.name` from a config or env | Rejected for aggregated external logs |
| Per-source adapter | Source-backed identity placed per Event | Packaged canonical path |
| Post-adapter adjustment | Deployment-known target attributes | Explicit deployment seam |

The OTel Collector's `host` and `resource` processors lean heavily on
auto-detection — it Just Works for the common case where one collector
runs on one host serving one service. It does the wrong thing for a
forwarder that aggregates dozens of source devices: every record
inherits the *forwarder's* `host.name`, contradicting External Logs
guidance.

The community has not converged. limpid takes the position that the
generic input and composer do not know enough to make this call
correctly. Each OTLP-capable raw-source parser has a sibling
`<source>_to_otlp` adapter that places source-backed identity per Event
(§5.4). A
deployment may adjust that adapter output afterward when it owns
additional facts.

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

limpid does not pick one globally. The parser-owned source adapter
builds the AnyValue shape the destination expects. See §5.7 for the
post-adapter adjustment convention.

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

- A parser normalizes source-claimed event time into
  `workspace.lsis.parsed.time` as epoch nanoseconds. `compose_otlp`
  selects `coalesce(shed.log_record.time_unix_nano, parsed.time)`.
  When both are absent, `time_unix_nano` is omitted; observation time
  is never substituted for event time.
- `observed_time_unix_nano` selects
  `coalesce(shed.log_record.observed_time_unix_nano, received_at)`.
  The Event receive timestamp is therefore the default observation
  time, while an explicit earlier observation point can override it.

The spec is comfortable with this split; the practice in the wild is
not consistent.

### 4.4 What `Scope` means for forwarded logs

`InstrumentationScope` is "the *library* that emitted the log." For an
SDK this is meaningful — `io.opentelemetry.slf4j` vs `okhttp3`, etc.
For a forwarder receiving syslog from a network device, there may be no
source-backed library identity. Implementations handle this differently:

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

limpid's per-source adapters set Scope only when the source exposes a
logger or instrumentation identity. Otherwise `compose_otlp` emits a
singleton ScopeLogs with no scope data on the wire (`scope: None` in the
proto). A deployment-owned identity may be supplied through an explicit
post-adapter adjustment. The composer does not synthesize one.

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
on the output reference page](./outputs/otlp_http.md#batch_level) for the
operational tradeoff and §5.1 above for the underlying wire ↔ CPU
trade that motivates the choice.

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

### 5.1 One LogRecord = one Event (Principle 4 in action)

A wire request carrying *N* LogRecords becomes *N* Events on the
limpid pipeline. The input splits along the LogRecord axis at the
moment of receive; Resource and Scope context is preserved by
constructing a *singleton* ResourceLogs (1 Resource + 1 Scope + 1
LogRecord) per Event and writing it to `ingress`. This is
[Principle 4 — atomic events through the pipeline](./design-principles.md#principle-4--atomic-events-through-the-pipeline)
applied to OTLP.

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

**Why bundling exists at all (the wire ↔ CPU trade).** OTLP envelopes
are not a fundamental property of the data — they are an
optimization. Sending 10 records as one `ExportLogsServiceRequest`
saves per-message header bytes, TLS handshake amortization, gRPC
stream setup, and TCP roundtrips compared to 10 separate RPCs. That
saving is real when wire is expensive (cross-WAN, metered links,
high-fanout collector → SaaS hops). When wire is cheap (loopback,
trusted LAN), bundling buys very little and you'd just send raw
atomic units.

This means **input bundling is a CPU-for-wire trade made by the
upstream**. The right way for limpid to handle it is to undo the
trade — pay CPU at receive to split back to atomic events — because
the rest of the pipeline (process snippets, queue, replay, tap)
operates on atomic events by design. Then, on emit, limpid pays CPU
again to rebundle if and only if the *downstream* benefits from the
wire saving. The split-then-rebundle round trip on a relay path is
not waste; it is the cost of treating events as the pipeline's unit
of meaning rather than treating wire envelopes as one.

**Why not "envelope mode" as an option.** A relay use case ("forward
unchanged") could in principle bypass the per-record split and pass
the envelope through. The argument against: it doesn't pay off.
With per-record split *plus* a no-op pipeline, the envelope is
reconstructed by the output's batch + `batch_level=scope` path with
the same wire result. The cost is the per-record cycle through the
queue, which for OTLP-relay traffic is a few microseconds per
record. The benefit of preserving Principle 4 across every input is
keeping limpid one tool with one mental model, not two.

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

### 5.3 Source time vs `received_at`

The answer to 4.3 follows the parser/adapter/composer split. The input
does not parse a payload, but the semantic parser does:

- **Input** sets `Event.received_at = Utc::now()` (wall-clock,
  always present).
- **Parser snippet** extracts source-claimed time and normalizes it to
  `workspace.lsis.parsed.time` as epoch nanoseconds.
- **Source adapter** may set an explicit target override only when the
  canonical scalar is not the intended OTLP value.
- **`compose_otlp`** applies the fixed chains:

  ```
  time_unix_nano          <- coalesce(shed override, parsed.time), else omit
  observed_time_unix_nano <- coalesce(shed override, received_at)
  ```

The composer never substitutes observation time for an absent event
time. A deployment that intentionally overrides event time does so in
the adapter output before composition.

Timezone interpretation follows the source contract. A vendor-defined
zone wins. Device-local timestamps and timestamps whose specification
leaves the zone undocumented both default to the limpid host's system
timezone (the device most likely shares the host's zone). Parsers
that accept local timestamps expose a source-specific timezone override;
their headers document the exact default and accepted values.

This is also why the `Event.timestamp` → `Event.received_at` rename
that landed in v0.5.0 was made: a forwarder must not silently conflate
wall-clock and source-clock semantics. See the breaking change entry in
[CHANGELOG.md](../../CHANGELOG.md).

### 5.4 Resource attributes are source-adapter-owned

limpid does **not** auto-detect `host.name`, `service.name`,
`os.type`, or any other Resource attribute. The parser-owned
`<source>_to_otlp` adapter places source-backed identity into
`workspace.lsis.shed.otlp.resource.attributes` per Event.

**Why no auto-detect.** As §4.1 notes, the OTel Collector's
auto-detect is correct for one common case (one collector =
one host = one service) and silently wrong for the case limpid
exists for (one forwarder = many sources). A device-aggregating
forwarder that auto-set `host.name` to its own hostname would
violate External Logs guidance for every record it emits.

The right value comes from the parser: a CEF line carries the
device hostname in `dvchost`; a syslog line carries the source
in the HOSTNAME field; a Kafka record carries it as a key.
A source adapter extracts that and writes it into Resource attributes
per record. The forwarder's identity (who
relayed) is unrelated and uninteresting; the source's identity (who
emitted) is what `host.name` and `service.name` should describe.

Deployment-owned facts may replace or extend an adapter slot in an
explicit block immediately after the adapter. That is a target-specific
adjustment seam, not a substitute for source-aware placement.

### 5.5 SeverityNumber mapping is in snippets, not Rust

Mapping a severity field in the log payload to OTLP
`severity_number` is a per-source decision: a producer's "High" may
mean OTLP 13 (WARN) for one vendor and OTLP 19 (ERROR3) for another.
Syslog PRI is transport metadata and is never promoted or used as a
fallback for canonical severity. limpid does not bake source mappings
into Rust; parser snippets carry exact tables and reject unknown
non-null source values.

The reference snippet library that landed in v0.7.0 (and was expanded
across the 0.7.x line) ships opinionated mappings for common vendors — see
[Snippet Library](./snippets/README.md). Authors of new snippets
follow the table conventions documented there.

`compose_otlp` projects the parser's canonical
`workspace.lsis.parsed.severity_number` into `SeverityNumber`, after
an explicit `shed.otlp.log_record.severity_number` override. When the
parser also preserved exact source text in
`workspace.lsis.parsed.severity`, the composer uses it as
`SeverityText`, after the corresponding shed override. Missing values
are omitted from the encoded LogRecord.

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

**What limpid does today.** In 0.7.8 the rejected subset is split out
into `events_failed` (separate from `events_written`) so the loss is
visible on dashboards, **and the trailing N events (N =
`rejected_log_records`) are routed to `control { error_log }` as
Output-flavor DLQ records** (`reason = "collector reported
partial_success rejection"`) for inspection and operator-driven
replay — sharing the same dead-letter sink as retry-exhausted /
shutdown-flush payloads. Attribution is approximate (the OTLP
response gives a count, not identities — limpid splits along the
trailing N entries; see the per-output notes in
[`otlp_grpc`](./outputs/otlp_grpc.md) /
[`otlp_http`](./outputs/otlp_http.md) for the caveat). Selective
*automatic* re-send of only the rejected records on a
`partial_success` reply remains future work (tracked in `send_once`
doc comments) — today the rejected count drives metrics and DLQ
routing, not in-pipeline retry shape.

### 5.7 Body format is the snippet's call

OTLP `body` accepts string / int / bool / double / bytes / array /
kvlist. limpid's snippet author chooses, per pipeline:

- **string** for a JSON-encoded payload (typical when downstream
  parses on its end — most cloud SaaS backends)
- **string** for a human-readable line (typical for archival / search)
- **kvlist** for structured composition where the receiver natively
  understands the OTLP attribute model (the OTel-native path)

A source adapter chooses the normal Body variant. A cloud-bound
deployment that intentionally wraps composed OCSF JSON changes that
choice in a target-specific adjustment after the adapter:

```
process parse_x
      | x_to_otlp
      | compose_ocsf
      | {
          workspace.lsis.shed.otlp.log_record.body =
              { string_value: workspace.lsis.composed.ocsf }
        }
      | compose_otlp
      | otlp_to_egress
```

The composed slot holds the already-serialised string, so no
per-event `to_json` is needed. The explicit `string_value` tag selects
the OTLP AnyValue variant without forcing every source Body to a
string.

---

## 6. What limpid intentionally does *not* do

These are not "queued for a later 0.x release" — they are out of scope for
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

Restated for clarity: see §5.4. No composer-side
`hostname()`-as-`host.name`, `cargo_pkg_version()`-as-`service.version`,
or `$HOSTNAME` fallback. The per-source adapter places source-backed
identity; otherwise the attribute is absent.

### 6.4 Schema URL inference

`ResourceLogs.schema_url` and `ScopeLogs.schema_url` are optional
fields pointing at an OpenTelemetry Schema URL (e.g.,
`https://opentelemetry.io/schemas/1.27.0`). Most backends ignore
them. limpid leaves them empty. A future config-level
`schema_url "..."` directive is plausible if it becomes a real
ask; it is not part of the 0.5.0 → 0.7.x cycle.

---

## 7. Pre-empted FAQs

### *"Why doesn't `service.name` show up in my OTLP output?"*

Because the source adapter did not have a source-backed value for it.
limpid will not auto-detect or default the field. Add a deployment-owned
value only in an explicit post-adapter adjustment. See §5.4.

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

### *"Why does the source adapter author Resource attributes? OTel Collector handles this for me."*

Because OTel Collector's auto-detection is correct for one common
deployment shape and wrong for limpid's primary one (multi-source
forwarder). See §4.1 and §5.4. If a deployment owns additional stable
identity, add it through the post-adapter adjustment seam.

### *"Why is `received_at` not the event time?"*

Because the event time is what the source claimed, not what the
forwarder observed. The two are not the same thing, and conflating
them motivated the `Event.timestamp` → `Event.received_at` rename
that landed in v0.5.0. See §5.3 and the
[CHANGELOG entry](../../CHANGELOG.md) for v0.5.0.

### *"Can I send Resource attributes from the input layer?"*

Inputs do not interpret payloads (Principle 2). The semantic parser
extracts source identity, and its sibling OTLP adapter places that
identity into Resource attributes. The generic composer does not make
that source-specific placement decision.

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
| How do I configure the input / output? | [otlp_http](./inputs/otlp-http.md), [otlp_grpc](./inputs/otlp-grpc.md), [otlp_http output](./outputs/otlp_http.md), [otlp_grpc output](./outputs/otlp_grpc.md) |
| What primitives are in the `otlp.*` namespace? | [Built-in Functions](./functions/expression-functions.md#otlp---opentelemetry-protocol-logs-signal) |
| What are the design principles this builds on? | [Design Principles](./design-principles.md) |
| What changed in v0.5.0 specifically? | [CHANGELOG](../../CHANGELOG.md) (covers the `Event.timestamp` → `Event.received_at` rename that landed in v0.5.0) |

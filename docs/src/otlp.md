# OTLP in limpid — design rationale

This page exists so that an issue starting with *"why doesn't limpid set
`service.name` automatically?"* or *"OTLP says X, you should do X,"* can
be answered with a link instead of a thread. The OTLP specification is
public. This page distinguishes its wire requirements from limpid's
choices for forwarding logs, with the reasoning for those choices.

The reference docs for *how to use* OTLP in limpid are elsewhere:

- [`otlp_http`](./inputs/otlp-http.md) and
  [`otlp_grpc`](./inputs/otlp-grpc.md) — input transports
- [`otlp_http` output](./outputs/otlp_http.md) / [`otlp_grpc` output](./outputs/otlp_grpc.md) — HTTP and gRPC senders
- [`otlp.*` functions](./functions/expression-functions.md#otlp--opentelemetry-protocol-logs-signal) —
  encode / decode primitives

The general design philosophy of limpid (the five principles) lives in
[Design Principles](./design-principles.md). This page assumes you've
read that and explains the OTLP-specific reading on top.

---

## 1. Scope

| Aspect | Support |
|---|---|
| Signal | **logs** only — no traces, no metrics, no profiles |
| Transports | HTTP/JSON, HTTP/protobuf, gRPC; HTTP output uses `otlp_http`, gRPC output uses `otlp_grpc`. A plaintext `http://` URL with a `tls` block is rejected during configuration checks. |
| Direction | input *and* output (so collector-to-collector relay works) |
| TLS | Server TLS / mTLS on both inputs; per-peer `tls { ca cert key }` on both outputs. Plaintext URLs cannot be combined with a `tls` block. |
| Versioning | OTLP 1.10 wire (the proto3 schema as of opentelemetry-proto 0.32) |

Traces and metrics share the same wire envelope shape but use different
proto messages, so the input / output skeleton from logs is reusable.
limpid supports logs because that is where its
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

1. **Repeated fields concatenate when protobuf messages are merged.**
   The [Protobuf encoding guide](https://github.com/protocolbuffers/protocolbuffers.github.io/blob/7a68ef28113f81f7ce7eabf72df89bc105e5cf13/content/programming-guides/encoding.md)
   describes this parsing rule. limpid can keep records in separate entries
   or shared groups, subject to its schema-URL grouping rules (§4.5).
2. **Field numbers support compatible schema evolution.** Adding fields
   is a binary wire-safe change under the rules in the
   [Proto3 guide](https://github.com/protocolbuffers/protocolbuffers.github.io/blob/7a68ef28113f81f7ce7eabf72df89bc105e5cf13/content/programming-guides/proto3.md).
   This does not make an entire schema immutable or every change safe.

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
A populated partial-success response can also carry a warning with zero
rejected records; see §3.3.

---

## 3. Where the spec is clear

A few points the spec *is* explicit about, where casual readings get
them wrong. limpid's behaviour follows the spec on these.

### 3.1 Resource describes the source of the log

The Logs Data Model's [Resource field in specification v1.40.0](https://github.com/open-telemetry/opentelemetry-specification/blob/v1.40.0/specification/logs/data-model.md#field-resource)
says:

> Describes the source of the log, aka resource.

The same section says that Resource is optional and may contain information
about the emitting application or the infrastructure where it runs. It does
not prescribe limpid's source-adapter policy for a relay aggregating logs
from multiple devices; that design choice is explained in §4.1 and §5.4.

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

### 3.3 partial_success does not permit retrying the request

The [OTLP specification in opentelemetry-proto v1.5.0](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/docs/specification.md),
under **Partial Success** for gRPC and HTTP, prohibits retrying the request
when the response contains a populated `partial_success`.

The [logs response definition in the same release](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/opentelemetry/proto/collector/logs/v1/logs_service.proto)
describes `rejected_log_records` as:

> The number of rejected log records.

Zero means that the request was fully accepted, but a populated response
may still carry a warning. A nonzero count does not identify the rejected
records or establish that every rejection was caused by invalid content.
Nor does a transport error prove that the receiver processed nothing.

limpid does not automatically construct a new request from an inferred
rejected subset. Its count-based recovery policy is described in §5.6.

---

## 4. Forwarding choices in limpid

The following choices describe limpid's forwarding model. They are not
additional requirements imposed on other OTLP implementations.

### 4.1 Whose attributes go on the Resource

limpid chooses to describe the original event source, rather than fill in
its own relay identity, when building Resource attributes for aggregated
logs. This is a forwarding policy, not a quoted rule about agents from the
Logs Data Model. Source identity must come from documented payload fields
or explicit operator knowledge; it cannot be inferred from the receiving
forwarder's hostname.

| Source | What it produces | limpid policy |
|---|---|---|
| Forwarder auto-detection | The receiving daemon's host or service identity | Not used to label aggregated external sources |
| Per-source adapter | Source-backed identity placed per Event | Packaged canonical path |
| Post-adapter adjustment | Operator-known target attributes | Explicit adjustment after the adapter |

The generic input and composer do not know enough to identify the original
source. Each OTLP-capable raw-source parser has a sibling
`<source>_to_otlp` adapter that places source-backed identity per Event
(§5.4). An operator may adjust that adapter output afterward when it owns
additional facts. This choice does not imply that other Resource policies
are prohibited by the specification.

### 4.2 What goes in `body`

`LogRecord.body` is an `AnyValue`. The [v1.5.0 common message definitions](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/opentelemetry/proto/common/v1/common.proto)
allow string, bool, int, double, bytes, array, and key/value-list variants.
That range does not prescribe one Body shape for every destination.

limpid's parser-owned source adapter builds the shape required by its
input and destination contract. See §5.7 for an explicit post-adapter
adjustment.

### 4.3 Whether the originating timestamp is in `time_unix_nano` or `observed_time_unix_nano`

`LogRecord` has two timestamp fields:

| Field | Defined as |
|---|---|
| `time_unix_nano` | When the event occurred, as claimed by the source |
| `observed_time_unix_nano` | When the receiver observed the event |

The [v1.5.0 LogRecord definition](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/opentelemetry/proto/logs/v1/logs.proto)
distinguishes event time from the time the collection system observed the
event. limpid preserves that distinction when it parses a source timestamp
and assigns its own input receive time.

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

These defaults are limpid's policy; a source-backed earlier observation
point can be retained through the explicit override.

### 4.4 What `Scope` means for forwarded logs

The [Instrumentation Scope glossary in specification v1.40.0](https://github.com/open-telemetry/opentelemetry-specification/blob/v1.40.0/specification/glossary.md)
describes a logical unit that emits telemetry. It can identify a library,
module, package, or class; it is not restricted to a library name.

For forwarded logs, limpid's per-source adapters set Scope only when the
source exposes a logger or instrumentation identity. Otherwise
`compose_otlp` emits a singleton ScopeLogs with no scope data. A
deployment-owned identity may be supplied through an explicit
post-adapter adjustment. The composer does not synthesize one.

### 4.5 Separate entries vs shared groups in batches (`batch_level`)

The [v1.5.0 logs message definitions](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/opentelemetry/proto/logs/v1/logs.proto)
use repeated ResourceLogs, ScopeLogs, and LogRecord fields. limpid can keep
one ResourceLogs per Event or share Resource/Scope metadata between
matching events. Its `batch_level = none | resource | scope` settings keep
the records while changing envelope grouping. An entry with an empty
`schema_url` may adopt a matching entry's non-empty `schema_url`; entries
with different non-empty URLs remain separate.

This is limpid's wire-size versus grouping-work tradeoff, rather than a
quoted specification definition of a batch. See [the output reference](./outputs/otlp_http.md#batch_level)
for the equality and schema-URL rules.

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
journal entry is one Event. Pipelines, snippets, the queue, and
`tap --json` — all assume one Event = one record. Treating an
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

**Why bytes, not a structured value.** Principle 3 of limpid's
design says "only `egress` crosses hop boundaries." The bytes are
already in limpid's selected wire representation; storing a decoded value and
re-encoding at every hop wastes work and creates a chance of
drift between the in-memory and on-the-wire representations.
Bytes on the hop, decode-on-demand in the snippet.

**Why protobuf, not JSON.** The [OTLP specification](https://github.com/open-telemetry/opentelemetry-proto/blob/v1.5.0/docs/specification.md)
defines both encodings. limpid chooses protobuf for its internal hop
contract so that both output protocols consume the same egress format.
The HTTP output re-encodes it when configured for `http_json`. This is a
representation choice, not a specification ranking or a performance
guarantee.

### 5.3 Source time vs `received_at`

The answer to 4.3 follows the parser/adapter/composer split. The input
does not parse a payload, but the semantic parser does:

- **Input** sets `received_at` from the host wall clock (always present).
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
timezone. Parsers
that accept local timestamps expose a source-specific timezone override;
their headers document the exact default and accepted values.

A forwarder must keep host observation time distinct from source event time.

### 5.4 Resource attributes are source-adapter-owned

limpid does **not** auto-detect `host.name`, `service.name`,
`os.type`, or any other Resource attribute. The parser-owned
`<source>_to_otlp` adapter places source-backed identity into
`workspace.lsis.shed.otlp.resource.attributes` per Event.

**Why no auto-detect.** limpid can aggregate records from many sources.
Copying its own hostname or service name onto all of them would identify
the relay rather than distinguish those sources. Avoiding that automatic
substitution is limpid's design choice (§4.1), not an additional OTLP
requirement.

A source adapter places known source identity into Resource attributes per
record. For example, the packaged CEF adapter takes `host.name` from the
enclosing syslog HOSTNAME. If it has no source-backed value, limpid does
not invent one from the forwarder's identity.

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

The reference snippet library ships opinionated mappings for common vendors — see
[Snippet Library](./snippets/README.md). Authors of new snippets
follow the table conventions documented there.

`compose_otlp` projects the parser's canonical
`workspace.lsis.parsed.severity_number` into `SeverityNumber`, after
an explicit `shed.otlp.log_record.severity_number` override. When the
parser also preserved exact source text in
`workspace.lsis.parsed.severity`, the composer uses it as
`SeverityText`, after the corresponding shed override. Missing values
are omitted from the encoded LogRecord.

### 5.6 Retry: whole requests only

The output modules retry the prepared `ExportLogsServiceRequest` for
connection failures, timeouts, and retryable status responses. For HTTP,
only 429, 502, 503, and 504 are retryable; other unsuccessful statuses,
including 408, 500, and received redirects, permanently exclude that peer
for this batch. The gRPC status rules, including the requirement for a
retry-delay hint with resource exhaustion, are detailed in
[HTTP and OTLP failure classification](./outputs/README.md#http-and-otlp-failure-classification).

A permanent failure immediately tries the next configured peer, without
spending the retryable-failure round budget. Exclusions survive transient
retries and reset for the next batch. Exhausting that budget or permanently
failing at every peer enters output recovery, whose saved copy and queue
progress follow the [recovery contract](./outputs/README.md#disposition-contract).
Neither a retry nor a recovery label proves that the receiver stored
nothing; retrying a request can duplicate delivery.

A populated `partial_success` response is not retried. limpid also does
not automatically resend an inferred rejected subset.

**Why.** As §3.3 explains, a populated partial-success response prohibits
retrying that request. The response reports a count rather than record
identities. limpid therefore does not infer a rejected subset to resend
automatically; it exposes the loss for operator review. A rejected count
does not by itself establish the cause or permanence of each rejection.

**What limpid does today.** The rejected subset is split out
into `events_failed` (separate from `events_written`) so the loss is
visible on dashboards, **and the trailing N events (N =
`rejected_log_records`) enter output recovery**. When a configured
`control { error_log }` write succeeds, it saves Output-flavor DLQ records (`reason = "collector reported
partial_success rejection"`) for inspection and operator-driven
replay — sharing the same dead-letter sink as retry-exhausted /
shutdown-flush payloads. Attribution is approximate (the OTLP
response gives a count, not identities — limpid splits along the
trailing N entries; see the per-output notes in
[`otlp_grpc`](./outputs/otlp_grpc.md) /
[`otlp_http`](./outputs/otlp_http.md) for the caveat). Selective
*automatic* re-send of only the rejected records on a
`partial_success` reply is not supported — today the rejected count drives metrics and DLQ
routing, not in-pipeline retry shape.

### 5.7 Body format is the snippet's call

OTLP `body` accepts string / int / bool / double / bytes / array /
kvlist. limpid's snippet author chooses, per pipeline:

- **string** for a JSON-encoded payload when the destination contract
  calls for parsing JSON from the Body
- **string** for a human-readable line
- **kvlist** for a structured body when the destination accepts that shape

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

These features are outside the scope of a forwarder.

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
`hostname()`-as-`host.name`, the daemon version as `service.version`,
or `$HOSTNAME` fallback. The per-source adapter places source-backed
identity; otherwise the attribute is absent.

### 6.4 Schema URL inference

OTLP Resource and Scope schema URLs are optional. limpid has no
`schema_url` configuration property.

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

The specification prohibits retrying a request after a populated
partial-success response. It supplies a rejected count, not record
identities. limpid does not construct a new request from an inferred
subset; its recovery policy is described in §3.3 and §5.6.

### *"Why does the source adapter author Resource attributes? OTel Collector handles this for me."*

limpid deliberately leaves source identity to the adapter for its
multi-source forwarding use case (§4.1 and §5.4). That policy does not
claim that the Collector's other Resource choices violate OTLP. If an
operator owns additional stable identity, add it through an explicit
post-adapter adjustment.

### *"Why is `received_at` not the event time?"*

Event time is what the source claimed; `received_at` is when the input
observed the event. See [timestamp semantics](#53-source-time-vs-received_at).

### *"Can I send Resource attributes from the input layer?"*

Inputs do not interpret payloads (Principle 2). The semantic parser
extracts source identity, and its sibling OTLP adapter places that
identity into Resource attributes. The generic composer does not make
that source-specific placement decision.

### *"limpid is not OTel-conformant because it does X / does not do Y."*

This page documents the supported logs transports and limpid's forwarding
choices; it is not a blanket conformance certification. Report a suspected
contradiction with a specific specification version, section, and wire
trace so that the behavior can be checked against that requirement.

---

## 8. Where to look for what

| Question | Source |
|---|---|
| How do I configure the input / output? | [otlp_http](./inputs/otlp-http.md), [otlp_grpc](./inputs/otlp-grpc.md), [otlp_http output](./outputs/otlp_http.md), [otlp_grpc output](./outputs/otlp_grpc.md) |
| What primitives are in the `otlp.*` namespace? | [Built-in Functions](./functions/expression-functions.md#otlp--opentelemetry-protocol-logs-signal) |
| What are the design principles this builds on? | [Design Principles](./design-principles.md) |

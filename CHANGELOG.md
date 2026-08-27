# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and runtime shape converge. After 1.0, changes will follow semver strictly.

## [Unreleased]

## [0.8.0] - 2026-08-27

### Added — authenticated LTP node transport

Limpid nodes can now exchange events through an unbatched `input ltp` and
`output ltp` pair authenticated with mutual TLS 1.3 raw public keys. Static peer
configuration binds each Ed25519 SPKI key to a `node_id`; connections send a
hello before events, preserve UUIDv7 event keys, reject cycles and bounded hop
histories, and retain hop stamps across disk queues and dead-letter replay.
Outputs use the existing queue, retry, shutdown, and dead-letter dispositions.

LTP telemetry pre-registers the configured peer union and reports network and
intra-node hop-latency histograms, negative-delta clamps, loop or hop-limit
drops, and undeclared-key or mismatched-identity connection rejections. Peer
labels are derived only from authenticated or declared configuration.

### Added — LTP node key provisioning

The optional top-level `node_key` selects an Ed25519 PKCS#8 private-key file.
Daemon startup and reload validate its owner, exact `0400`/`0600` mode, regular
file type, and key format through one non-symlink-following file descriptor.
`limpidctl ltp keygen <path>` creates a new `0600` key without overwriting an
existing path and prints the matching RFC 8410 SPKI public key as one base64
line.

### Added — immutable event identity

Every event now receives a UUIDv7 `key` before fan-out. The key is preserved
through clones, queue persistence, dead-letter capture, and JSON replay, and is
visible in `tap --json` without changing the default raw tap stream or the DSL.
Pre-0.8 Event JSON without a key is assigned one when first read. The dead-letter
record schema is now version 3 because its `event` object carries the key.

### Fixed — startup is transactional and shutdown honors durable work

Runtime startup now waits for input and control resources to become ready and
rolls back every acquired resource if a later step fails or startup is
cancelled. Shutdown distinguishes bounded cancel-safe network and terminal work
from file, WAL, dead-letter, and journal owners that must be joined, preserving
queue acknowledgement and exact output disposition.

### Fixed — journal readers follow rotations

Journal readers now follow the official `sd_journal_wait` contract at EOF,
handling append and invalidation notifications on the same long-lived handle so
entries continue across journal rotation. Immediate wait errors use a bounded
backoff while retaining responsive shutdown and cursor semantics.

### Fixed — RFC 3164 parsing preserves space-padded days

The syslog parser now accepts and preserves the RFC 3164 convention that pads a
single-digit day with a space. Canonical parser snippets no longer collapse or
reject that timestamp shape before downstream processing.

### Changed — dropped metrics use one rooted hierarchy (breaking)

`limpid_pipeline_events_dropped_total{pipeline}` is replaced by the
`limpid_events_dropped_total{pipeline,step,process_path,process_name}` family.
The pipeline frame is the hierarchy root and uses `step="0"`,
`process_path="/"`, and an empty `process_name`; process frames retain their
one-based root step and compiled invocation path. Dashboards and alerts using
the old family name must migrate to the unified family and select the root or
process paths they need.

`limpid-prometheus` additionally exposes
`limpid_events_dropped_own_total` with the same labels. It subtracts direct
child totals at scrape time, so the root series reports drops executed directly
in the pipeline body and process series report drops executed directly in that
process body. The daemon's source counter remains the propagated total at each
node.

### Changed — process call graphs must be acyclic (breaking)

`def process` declarations may still call other named processes, but direct
and mutual recursion are now rejected at config-load time. The same validation
applies to calls nested in control-flow branches and to definitions merged from
included files. Existing acyclic parser, normalizer, and composer chains are
unchanged. Configurations that relied on recursive process calls must rewrite
the flow as a finite process graph or use the bounded collection primitives for
collection traversal.

### Changed — metrics use one self-describing schema-v1 registry

The daemon now registers its existing input, pipeline, and output counters in a
shared self-describing registry and serves typed schema-v1 snapshots from the
existing `stats` command. `limpidctl stats` preserves the operator table,
`stats --details` provides a generic human view of counter, gauge, and histogram
families, and `stats --json` preserves the complete raw response.

`limpid-prometheus` now translates schema-v1 families generically to Prometheus
text format 0.0.4, validates complete snapshots before rendering, emits
histogram bucket/sum/count series with an implicit `+Inf` bucket, and handles a
source histogram `le` label through a lossless underscore-chain shift. The
canonical sidecar checkpoint exercises the real control-socket and HTTP scrape
path at three payload scales; detailed machine observations remain in the
benchmark harness receipt rather than this product changelog.

The new observability surface includes logical input/output byte counters,
runtime queue, retry, and in-flight gauges, process invocation counters,
`limpid_build_info`, and the three latency stages below. The
`limpid-prometheus` package also ships a bundled Grafana dashboard and exactly
four alert rules.

### Added — three-stage latency histograms

0.8.0 introduces three non-overlapping latency histograms:
`limpid_input_queue_wait_seconds{input}` measures local input arrival to the
shared pipeline dispatch start (T0→T1), with
`limpid_input_queue_wait_negative_delta_total{input}` counting wall-clock
reversals clamped to zero;
`limpid_pipeline_processing_seconds{pipeline,output}` measures that shared
dispatch start to each output snapshot (T1→T2); and
`limpid_output_delivery_seconds{output}` measures the snapshot to confirmed
delivery (T2→T3). Together they separate input queueing, pipeline work, and
output delivery without gaps or overlap.

The disk-backed output queue record format also changes: 0.8.0 records require
`emitted_ns` for delivery latency. Before upgrading from 0.7.15, drain every
disk-backed output queue backlog. Leftover 0.7.15 records do not contain the
field, so 0.8.0 rejects them and reports them as corrupted; there is no
automatic migration.

## [0.7.15] - 2026-07-20

### Changed — transport / format unwrapping separated from vocabulary parsing (breaking)

Bundled vocabulary parsers no longer unwrap their transport or format
layers internally: `parse_asa` / `parse_paloalto_syslog` stopped calling
`syslog.parse(ingress)` and `parse_fortigate_cef` / `parse_paloalto_cef`
additionally stopped calling `cef.parse(...)`. Each layer is its own
pipeline stage — a new `parsers/parse_cef.limpid` (transport/format-style
`parse_cef` + generic `cef_to_otlp` adapter for the vendor-independent
ArcSight CEF surface) joins `parse_syslog`. The separation lets a
pipeline inspect and drop events after each stage without paying the
next stage's parse cost, and keeps the stages independently composable.
`parse_fortigate_syslog` is deliberately unchanged: FortiGate's native
`<PRI>date=...` wire is not RFC 3164, so there is no meaningful
transport stage to split out (it keeps stripping the PRI itself).

Out-of-tree pipelines using these four parsers must update their chains
(extraction results are unchanged — the same primitives run at the new
stages):

| 0.7.14 chain | 0.7.15 chain |
|---|---|
| `parse_asa \| ...` | `parse_syslog \| parse_asa \| ...` |
| `parse_paloalto_syslog \| ...` | `parse_syslog \| parse_paloalto_syslog \| ...` |
| `parse_fortigate_cef \| ...` | `parse_syslog \| parse_cef \| parse_fortigate_cef \| ...` |
| `parse_paloalto_cef \| ...` | `parse_syslog \| parse_cef \| parse_paloalto_cef \| ...` |

### Changed — cef.parse isolates Extension keys from the positional header (breaking)

`cef.parse()` previously flattened the data-driven Extension key=value
pairs into the same object as the seven positionally-determined header
fields. An extension named after a header field (`severity=`, `name=`
— dialect quirk, buggy template, or log injection alike) was pushed as
a duplicate sibling key: arena field reads resolved to the header
(first-wins) but the persisted workspace snapshot resolved to the
extension value (last-wins), so downstream processes saw the header
silently replaced. Data of different trust levels must not share a
plane, so the split pairs now land in a nested sub-object and the raw
blob is renamed to avoid confusion with it:

| 0.7.14 path | 0.7.15 path |
|---|---|
| `workspace.cef.<extension-key>` | `workspace.cef.extension.<extension-key>` |
| `workspace.cef.ext` (raw blob) | `workspace.cef.extension_raw` |

Header fields (`version` / `device_vendor` / `device_product` /
`device_version` / `signature_id` / `name` / `severity`) are unchanged.
Out-of-tree pipelines reading extension keys off `cef.parse()` output
must add the `extension.` segment. The bundled CEF parsers
(`parse_cef` / `parse_fortigate_cef` / `parse_paloalto_cef`) are
updated accordingly.

### Fixed — OTLP Scope/Resource placement follows the spec's two-tier identity reading

The bundled `<source>_to_otlp` adapters shipped in 0.7.14 placed program-level
identity in the InstrumentationScope, inverting the OpenTelemetry Logs Data
Model reading in which the program (process) that produced the record is
Resource identity and only a logger / channel / stream *within* a program is
Scope identity. Program names now land in Resource `service.name` — syslog
APP-NAME, `sshd`, `sudo`, `auditd`, `named`, `suricata`, `kube-apiserver`,
Juniper SRX daemon names, the full Postfix `postfix/<program>` spelling, the
Sysmon provider `Microsoft-Windows-Sysmon`, journald's
`SYSLOG_IDENTIFIER`/`_SYSTEMD_UNIT`, the Windows event `SourceName`, and Okta
(`okta`, retiring the invented `okta.system_log` spelling) — while
`scope.name` is reserved for genuine logger/channel identities: the Windows
event `Channel`, the Sysmon `Channel` when the forwarder ships it, Zeek's
`_path`, and the static `journald` structuring layer. Sources that do not name
themselves continue to leave both unset.

### Fixed — undocumented RFC 3164-family timezone default is host-local, not UTC

Bundled parsers whose legacy timestamp format leaves the source zone
undocumented (`parse_asa`, `parse_nsp`, `parse_paloalto_cef`,
`parse_paloalto_syslog`) previously assumed UTC. No vendor specification
documents UTC wall-clock for these formats, so the pack default now follows
the ruling already applied to `parse_fortigate_cef` /
`parse_juniper_srx_syslog`: a vendor-documented UTC format keeps UTC, and a
documented device-local zone or a specification gap defaults to the limpid
host's system timezone — the most likely assumption being that the device
shares the host's zone. The `workspace.<parser>.timezone` override contract
is unchanged: IANA names and fixed offsets are accepted, explicit `local`
is still rejected.

### Fixed — cef.parse honors the header escapes `\|` and `\\`

The header splitter treated every `|` as a field separator, so a
spec-legal escaped pipe (`\|`) inside a header field shifted all
subsequent fields — for `CEF:0|V|P|1.0|sig|deny\|drop|3|act=block` the
name became `deny\`, the severity slot received the string `drop`, and
the extension section received `3|act=block`, misclassifying telemetry
severity downstream. The splitter now separates only on unescaped
pipes and decodes the two generic structural escapes that participate
in field splitting (`\|` → `|`, `\\` → `\`); sequences outside those
two (`\x` etc.) are kept literally, and the spec's field-specific
escaping for vulnerability spellings in deviceEventClassId / name is
field-internal grammar the generic primitive passes through raw. The
bug predates this release in the primitive itself; the new generic
`parse_cef` / `cef_to_otlp` path widens its blast radius to every CEF
pipeline, so it is fixed in 0.7.15. Extension-section escapes remain
undecoded (unchanged, documented scope).

### Added — block primitives iterate object entries

`map`, `filter`, `find`, and `reduce` now accept Object values in addition to
arrays. Object blocks bind each key as a String alongside its value, visit
entries in insertion order without deduplicating repeated keys, and use
type-specific arity checks so an array-shaped block cannot silently consume an
object (or vice versa). `map` returns an array of block results, `filter`
returns the retained object entries, `find` returns the first matching
`[key, value]` pair, and `reduce` folds `|acc, key, value|` over the entries.
Null inputs retain the existing array behavior.

### Fixed — analyzer contracts match runtime value shapes

Static checking now models ingress and egress as String-or-Bytes payloads,
accepts both runtime input forms for `to_string` and the OTLP decoders, and
preserves nullable returns for partial primitives instead of claiming a value
is always present. `cef.parse` limits its data-driven workspace wildcard to
`workspace.extension.*`, so unrelated workspace paths are no longer widened.
User-defined function calls now report arity mismatches during `--check` with
the same expected-versus-actual shape used by runtime diagnostics.

The `strftime` and `strptime` timezone keywords `UTC` and `local` are now
ASCII case-insensitive; IANA timezone names remain case-sensitive.

### Fixed — nullable expression inference follows runtime control flow

The static analyzer now removes skipped `Null` members from non-final
`coalesce` arguments while preserving the final argument's nullability, and
infers `len` from its concrete input type. Equality checks against `null` are
treated as presence guards rather than incompatible-type comparisons. These
rules prevent reusable composers from producing strict-check false positives
after a source adapter specializes an otherwise optional field.

### Changed — snippet headers declare file facades and member contracts

Packaged snippets now separate file-level metadata from the contracts of each
public process or function. A `Facade:` list names the externally callable
members, and an adjacent `Process:` or `Function:` block documents each public
member without forcing internal dispatch leaves into the public surface.
`Reads:` and `Writes:` accept multiple independently validated workspace roots,
so adapters and bridges can state their real cross-namespace boundaries. The
header linter rejects missing, orphaned, misplaced, and signature-mismatched
member blocks, and the generated inventory consumes the same facade metadata.

### Changed — dependency dedupe

Bumped `axum` to 0.8 and `webpki-roots` to 1.0 so the dependency graph no
longer carries two major versions of `axum` / `axum-core` / `matchit` /
`webpki-roots`. The remaining `getrandom` 0.2/0.3 split (ring vs rand 0.9)
is an upstream limitation and is documented as version-scoped skips in
`deny.toml`; the newest getrandom line (currently 0.4, dev-only via
`tempfile`) is deliberately not skipped so any future additional split
still warns.

## [0.7.14] - 2026-07-16

> source-owned OTLP semantics

### Changed — snippet severity is canonical OpenTelemetry severity

Bundled semantic parsers now write normalized OTel `SeverityNumber` values to
`workspace.lsis.parsed.severity_number` and, when the source carries a textual
severity, preserve its exact spelling in `workspace.lsis.parsed.severity`.
Source-backed parsers validate their documented input domains and fail with an
operator-readable error on unknown non-null values. Parsers for sources without
a severity concept do not infer one from status, action, event kind, syslog PRI,
or journald `PRIORITY`.

`compose_ocsf` derives its required `severity_id` at the output boundary:
canonical numeric severity wins, text-only source severity renders as Other
(99), and a record with neither renders as Unknown (0). Its legacy
`parsed.severity_id` read remains only as a lower-priority compatibility path
for out-of-tree callers. Custom parsers should migrate their canonical writer
to `parsed.severity_number` and optionally preserve exact source text in
`parsed.severity`; `compose_otlp` does not read the legacy compatibility slot.

### Fixed — OCSF timestamps use the schema's millisecond unit

`compose_ocsf` now converts LSIS epoch nanoseconds to OCSF epoch milliseconds
for every public class leaf, rather than emitting the nanosecond value directly.
The inbound `parse_ocsf` path performs the inverse conversion so OCSF
round-trips retain the canonical LSIS nanosecond scale.

### Fixed — OCSF output retains parsed status and identity facts

The OCSF composer now preserves `status_id` for Email Activity, Account Change,
Vulnerability Finding, RDP, SMB, SSH, FTP, User Access Management, and Group
Management records. The sudo parser also retains PAM session user identifiers
instead of losing them while shaping the LSIS actor and user facts.

### Added — shared snippet converters and stricter authoring checks

The snippet library adds partial, exact-domain helpers for OCSF ↔ OTel severity
and epoch nanosecond ↔ millisecond conversion. Header lint and generated
inventory now support multiple function signatures in one function-family file.
`limpid --check` now reports unknown function calls even when no near-match
suggestion exists, including namespaced calls.

### Fixed — OTLP observed time defaults to the Event receive timestamp

`compose_otlp` now populates `observed_time_unix_nano` from `received_at`
when the caller does not provide an explicit
`shed.otlp.log_record.observed_time_unix_nano` override. The Event timestamp
is passed to the OTLP encoder without an intermediate floating-point
conversion, preserving nanosecond values above 2^53 exactly. Explicit
overrides continue to take precedence.

### Fixed — OTLP severity follows canonical parsed facts

`compose_otlp` now reads normalized severity from
`workspace.lsis.parsed.severity_number` instead of the retired
`parsed.severity_id` compatibility slot. Source-backed records therefore
retain their OTel `SeverityNumber` on the OTLP wire. When a parser also
preserves the exact source spelling in `parsed.severity`, the composer uses
it as `SeverityText`; an explicit
`shed.otlp.log_record.severity_text` value still takes precedence.
Source-less records omit both fields; protobuf decoders expose the standard
zero/empty defaults.

### Breaking — OTLP placement is owned by per-source adapters

Thirty of the 31 bundled parser files now ship a sibling `<source>_to_otlp`
process. The inbound `parse_ocsf` compatibility parser is the sole parser
without this adapter.
The adapter owns source-specific OTLP Resource, Scope, Body, and LogRecord
attribute construction; the canonical pipeline is
`parse_<source> | <source>_to_otlp | compose_otlp | otlp_to_egress`.
Attribute names follow OpenTelemetry semantic conventions and ECS before a
vendor namespace, and adapters do not export internal OCSF taxonomy integers.

`compose_otlp` now accepts ten optional shed slots and only performs mappings
that are source-independent. Explicit shed scalar overrides take precedence
over canonical `parsed.time`, `parsed.severity_number`, and `parsed.severity`;
observed time defaults to `received_at`. Resource attributes, Scope fields,
Body, and LogRecord attributes are shed-only and are omitted when unset. The
composer no longer synthesizes the forwarder hostname, a fixed `limpid` Scope,
or a string Body, and event time never falls back to observation time. Body is
an OTLP AnyValue object whose variant is chosen by the adapter.

This is breaking for out-of-tree OTLP pipelines that called `compose_otlp`
without a source adapter or relied on those synthesized defaults. Insert or
author a per-source adapter first. Deployment-specific target adjustments may
replace adapter shed slots after that stage; they do not replace the adapter.
OTLP transport configuration and already encoded `egress` payloads are
unchanged. Public authoring docs, snippet headers, and the pack README now use
the same parser → adapter → composer contract.

### Changed — parsers preserve source event time in epoch nanoseconds

Semantic parsers now normalize supported source timestamps into
`workspace.lsis.parsed.time` as exact epoch nanoseconds, including values above
2^53. Sources with explicit offsets or epoch timestamps retain their stated
instant. Vendor-defined fixed zones are honored directly. For local wall-clock
formats, documented device-local formats use the limpid host's system timezone,
while formats with no authoritative timezone contract default to UTC.
Source-specific `workspace.<source>.timezone` overrides accept IANA names or
fixed offsets and reject invalid values loudly. Transport-only syslog and
journald parsers continue to leave semantic event time to the downstream source
parser.

### Breaking — `Int` arithmetic stays exact across the full i64 range

When both operands are `Int`, the `+`, `-`, `*`, `/`, and `%` operators now use checked i64 arithmetic instead of converting through `f64`. This prevents epoch-nanosecond values and other integers above 2^53 from silently losing precision. Integer division truncates toward zero (`5 / 2 == 2`, `-5 / 2 == -2`), remainder follows the dividend's sign, and integer division or remainder by zero continues to return `Int(0)`.

This changes `Int / Int` from fractional floating-point division to truncating integer division. Use a `Float` operand (`value / 2.0`) when fractional output is intended. Any i64 overflow, including `i64::MIN / -1` and `i64::MIN % -1`, is now an evaluation error rather than an approximate floating-point fallback. Mixed `Int` / `Float` expressions continue to use the existing floating-point path.

### Fixed — OTLP adapters preserve parser-owned facts and ECS classifications

Per-source OTLP adapters now read the exact LSIS paths written by every supported parser route. The fixes retain DNS, HTTP, process, file, user, and delegated Zeek facts; normalize ECS direction and lifecycle classifications; preserve RFC 5424 NILVALUE semantics; and correct source/destination byte attribution. A static read/write-path sweep across all 30 adapters plus decoded OTLP route coverage now guards against silent field omission.

OCSF Other (99) remains available through the legacy compatibility slot when no OTel numeric mapping exists, while OCSF Unknown (0) remains canonically unset. Juniper SecIntel keeps its numeric severity mapping and preserves the raw source value without synthesizing SeverityText.

## [0.7.13] - 2026-07-11

0.7.13 splits the `workspace.lsis.*` LSIS namespace into three explicit layers and reshapes the two envelope composers around the new contract. The trigger was AMP (Azure Monitor Pipeline) integration surfacing that `compose_otlp` had no way to accept caller-supplied target-vocabulary attributes (CommonSecurityLog columns) without every downstream reimplementing the OTLP encoding locally. Rather than expose a shed slot per attribute as an ad-hoc extension hatch, the LSIS namespace itself now has three sub-layers with distinct kinds of contract — a facts layer parsers write, a plumbing layer glue blocks write for the next composer, and a products layer composers write. The runtime is unchanged in behaviour; the surface differences are confined to snippet pack contents, docs prose, and the header lint that keeps the pack self-consistent. All out-of-tree configs that touched `workspace.lsis.*` will need to move to the layered names — the mapping is mechanical and the CHANGELOG's `Changed` section below lists it explicitly.

### Added — LSIS namespace stratified into `parsed` / `shed` / `composed`

The flat `workspace.lsis.*` namespace forced facts, hand-off plumbing, and finished wire-form products to share the same face, and that flatness is what produced the recurring "OCSF-shaped" confusion in the field (readers looking for a schema with required fields on `workspace.lsis.*` and not finding one, because there wasn't one — it was a dictionary, and the values were graceful-absence).

Three layers, each with a distinct kind of contract, now live under the same reserved root:

- **`workspace.lsis.parsed.*`** — facts a parser established about the event. A vocabulary contract, dictionary semantics, every field optional; the vocabulary leans OCSF but is an open set. Writers: parsers. Readers: everyone.
- **`workspace.lsis.shed.*`** — hand-off values a glue block wrote for the next composer. A per-consumer plumbing contract, no globally reserved vocabulary — names borrow the consumer's own vocabulary (`shed.otlp.log_record.body`, `shed.rfc5424.msg`), and each consuming composer's header enumerates the slots it eats and the defaults it applies. Values are scoped to one hand-off; nothing under `shed.` is a fact about the event.
- **`workspace.lsis.composed.*`** — finished wire forms, one slot per composer (`composed.ocsf`, `composed.otlp`, `composed.rfc5424`, `composed.replayable`). A registry contract, single-writer invariant. Egress terminators read from here (`egress = workspace.lsis.composed.otlp` is the whole story of shipping an event).

The pack README's LSIS section carries the canonical explanation of the three layers and the slot registries; docs/src references now link there rather than paraphrasing in place.

### Changed — `compose_otlp` reads shed slots + parsed graceful reads (instead of the old `otlp_body` per-envelope slot)

The 0.7.11 envelope composer's "envelope neutrality" doctrine — read only a declared `otlp_body` and produce a minimal envelope — is retired. Real OTLP targets need a way to feed target-specific attributes (AMP CommonSecurityLog columns, `service.name` for a specific backend, etc.) that the pack cannot know about, and forcing every downstream to reimplement compose_otlp locally is not a shape the pack should ship.

`compose_otlp` now reads four `shed` slots plus two `parsed` graceful reads, with a uniform `coalesce(shed value, default)` replacement semantics for every shed slot (no merge):

```
workspace.lsis.shed.otlp.resource.attributes    (optional Array of KeyValue)
workspace.lsis.shed.otlp.scope.attributes       (optional Array of KeyValue)
workspace.lsis.shed.otlp.log_record.body        (required String)
workspace.lsis.shed.otlp.log_record.attributes  (optional Array of KeyValue)

workspace.lsis.parsed.time         → time_unix_nano  (falls back to received_at)
workspace.lsis.parsed.severity_id  → severity_number (omitted when 0/absent —
                                                       OCSF Unknown = 0)
```

Callers that supply `resource.attributes` REPLACE the default `[host.name]` array (if they still want host.name they include it in the array they pass); callers that supply nothing get the pre-0.7.13 behaviour byte-for-byte modulo the time / severity graceful reads that were previously hard-coded to `received_at` / omitted. The AMP attribute case is demonstrated in the composer's header.

Pipelines that used the 0.7.11 shape (`{ workspace.lsis.otlp_body = workspace.lsis.ocsf } | compose_otlp | otlp_to_egress`) migrate mechanically to `{ workspace.lsis.shed.otlp.log_record.body = workspace.lsis.composed.ocsf } | compose_otlp | otlp_to_egress`.

### Changed — `compose_rfc5424` split into a generic composer + `journald_to_rfc5424` bridge

The 0.7.11 `compose_rfc5424` read `workspace.journald.*` directly, which was the same shape as the CEF-direct-read carve-out that the 0.7.11 design pass explicitly rejected for pack composers. It also had two latent DSL bugs (three zero-arg `def function`s without `()`, and function bodies reading `workspace.journald.*` in violation of the analyzer's purity contract) — neither the pack's CI nor any downstream test happened to exercise the file end-to-end, so both had been shipping quietly. 0.7.13 fixes the contract violation and the shipping bugs together.

`compose_rfc5424.limpid` now contains three cooperating processes plus one pure helper:

- **`def process compose_rfc5424`** (generic body composer) — reads eight `shed.rfc5424.*` slots (`pri` / `timestamp` / `hostname` / `app_name` / `procid` / `msgid` / `sd` / `msg`, with `msg` the only intended-required slot) and writes `workspace.lsis.composed.rfc5424`. Same `coalesce(shed value, default)` replacement semantics as compose_otlp.
- **`def process journald_to_rfc5424`** (bridge) — reads `workspace.journald.*` (produced by `parse_journald`) and populates the `shed.rfc5424.*` slots. Unset slots fall back to compose_rfc5424's defaults, so the wire output is byte-identical to the old direct-read composer.
- **`def function rfc5424_pri_from(facility_str, severity_str)`** — the PRI arithmetic (facility × 8 + severity) as a pure helper the bridge calls with journald's byte-string fields.
- **`def process rfc5424_to_egress`** — unchanged.

Pipelines that used `parse_journald | compose_rfc5424 | rfc5424_to_egress` migrate to `parse_journald | journald_to_rfc5424 | compose_rfc5424 | rfc5424_to_egress` (one extra pipe stage). Non-journald upstreams that want to emit RFC 5424 records now set `shed.rfc5424.*` from an anonymous glue block or a bespoke bridge — see the composer's header for the two supported shapes.

### Changed — namespace rename `workspace.lsis.<field>` → `workspace.lsis.<layer>.<field>` (mechanical)

Every snippet body and every docs example is updated. The mechanical mapping for out-of-tree configs that touched `workspace.lsis.*` directly:

| Old flat name | New layered name |
|---|---|
| `workspace.lsis.<any fact field>` (e.g. `.severity_id`, `.time`, `.src_endpoint.ip`) | `workspace.lsis.parsed.<same field>` |
| `workspace.lsis.otlp_body` | `workspace.lsis.shed.otlp.log_record.body` |
| `workspace.lsis.ocsf` | `workspace.lsis.composed.ocsf` |
| `workspace.lsis.rfc5424` | `workspace.lsis.composed.rfc5424` |
| `workspace.lsis.replayable` | `workspace.lsis.composed.replayable` |
| `workspace.lsis.otlp` | `workspace.lsis.composed.otlp` |
| bare `workspace.lsis = { … }` (whole-hash assign in a parser) | `workspace.lsis.parsed = { … }` |

Transport-namespace parsers (`parse_syslog` → `workspace.syslog.*`, `parse_journald` → `workspace.journald.*`) are unaffected — those namespaces live outside LSIS by design, and the pack composers no longer read them directly (bridges now do that job on the consumer side).

### Changed — xtask header lint accepts multi-segment paths

`cargo xtask lint-snippet-headers` teaches its `Reads:` grammar to accept dot-separated ascii-ident paths — both in the workspace namespace at the first line (`workspace.lsis.shed.otlp.*` is a valid intake declaration) and in the intake dot-line's `<name>` field (`.log_record.attributes (optional, Array)` describes an intake slot with a structured name). The 7-element `INTAKE_TYPES` whitelist is unchanged; type refinements like "of KeyValue" go in trailing prose after the closing `)` rather than in the type marker. A new unit test locks in the relaxed grammar so it does not silently drift back.

### Fixed — pack composer inventory table renders new slot names

`cargo xtask gen-snippet-inventory` refreshes the composer table in `packaging/snippets/README.md`. The Writes column now points at the new `workspace.lsis.composed.<slot>` outputs, and the Summary column reflects the rewritten compose_otlp / compose_rfc5424 headers.

### Changed — docs/src collapses LSIS mechanics to a single pointer

Every docs page under `docs/src/` that previously carried its own paraphrase of the LSIS contract now links into `packaging/snippets/README.md`'s LSIS section — the canonical description lives there and only there. Docs prose stays snippet-pack-facing; the language reference does not gain LSIS material. Pipeline examples that showed old slot names are updated to the layered names.

## [0.7.12] - 2026-07-11

0.7.12 is a hotfix for a daemon-fatal SIGPIPE path: a saturated stderr / systemd-journald pipe under a tracing burst (typically a downed OTLP output driving its retry loop's WARN spam) would take the whole daemon with it, and systemd would not restart it because the exit looked clean. The daemon now keeps Rust's `SIG_IGN` default for `SIGPIPE`, and the shipped `limpid.service` unit restarts on any exit reason as defence-in-depth against future variants.

### Fixed — daemon no longer dies on a broken tracing pipe

`crates/limpid/src/main.rs` previously restored `SIGPIPE = SIG_DFL` at process entry unconditionally, before CLI parsing. The intent was to make CLI-style modes (`--check`, `--test-pipeline`, `--graph`) terminate cleanly when their stdout was piped through `head` / `less`; the effect was to also arm the daemon to die on any broken pipe. Under a downed OTLP endpoint, the retry loop's per-attempt `tracing::warn!` messages saturated the stderr-to-journald forwarding pipe; when the pipe closed, the next write raised `SIGPIPE` and the process exited (systemd log: `Deactivated successfully. … signal=PIPE`). Restart was suppressed because the exit was clean from systemd's perspective.

The fix moves the `SIG_DFL` install after `Cli::parse()` and gates it on the CLI-mode flags. Daemon mode now keeps Rust's `SIG_IGN` default, so a broken pipe on the stderr side surfaces as an ordinary `EPIPE` write error the tracing subscriber drops rather than a fatal signal. The CLI-mode behaviour is unchanged: `--check | head` still terminates cleanly on downstream close.

### Changed — packaged systemd unit uses `Restart=always` and disables `StartLimitIntervalSec`

`packaging/limpid.service` moves from `Restart=on-failure` to `Restart=always` so a signal-killed exit that systemd classifies as clean (SIGPIPE being the load-bearing case) still triggers a restart. `systemctl stop` continues to stop the unit cleanly because systemd remembers operator-initiated transitions and does not restart across them. `StartLimitIntervalSec=0` disables the start-rate ceiling so a sustained-crash regression cannot wedge the unit into `failed` state; `RestartSec=5` still paces individual restart attempts. Together the two settings turn systemd into a real defence-in-depth layer around whatever the daemon itself missed to catch.

## [0.7.11] - 2026-07-11

0.7.11 formalises the snippet library's contracts. The library has grown from a handful of parsers into 39 files across four kinds (parsers / composers / filters / functions), and the ad-hoc header conventions and namespace naming had started to obscure rather than help — vendor documentation blocks and shared-intermediate blurbs were repeating themselves out of sync, the intermediate namespace's `workspace.limpid` name conflated the schema shape with the daemon crate, and the composer contract left it up to each snippet to write `egress` directly with no invariant an operator could rely on. 0.7.11 renames the intermediate to LSIS (Limpid Snippet Intermediate Schema, `workspace.lsis.*`), extracts the header schema into an xtask-enforced per-kind contract with an auto-generated inventory in the packaging README, splits composers into a schema layer that writes an LSIS slot and an envelope layer that wraps a payload, and introduces a `compose_otlp` envelope composer alongside a single-writer invariant on `egress`. Five cloud parsers (AWS GuardDuty, VPC Flow, Azure Activity, Kubernetes audit, Okta System Log) join the tracked set. The daemon runtime is unchanged in shape and throughput — the behaviour differences are confined to what shipped snippets produce, and the two user-visible breaking changes for out-of-tree configs are the namespace rename and the composer-contract split (both described in their own sections below).

### Added — LSIS (Limpid Snippet Intermediate Schema)

The parser ↔ composer canonical intermediate now has a name and a definition of its own. LSIS's vocabulary — field names, numeric class IDs (`class_uid 3002`, `4001`, …), activity / status enumerations — is borrowed from OCSF 1.3.0 so `compose_ocsf` renders LSIS to conformant OCSF JSON without translation, but LSIS is not itself an OCSF conformance claim: fields outside a class's OCSF definition are permitted (they land in `unmapped` when rendered), and future LSIS revisions can diverge from OCSF where wire realities demand it. The canonical definition and a slot registry (`workspace.lsis.ocsf` / `.rfc5424` / `.replayable` / `.otlp` / `.otlp_body`) live at the top of `packaging/snippets/README.md`; the docs pages that referenced the intermediate now link to that section rather than paraphrasing it in place.

### Changed — namespace rename `workspace.limpid.*` → `workspace.lsis.*`

Every snippet body, every doc example, and three Rust references (analyzer test DSL, `coalesce` doc comment, xtask test fixture) migrate to the new namespace. Pipelines that previously wrote or read `workspace.limpid.<field>` must rename to `workspace.lsis.<field>` — this is a breaking change for out-of-tree configs that touched the intermediate directly. CHANGELOG entries for prior releases are unchanged; they describe what shipped under the old namespace.

### Changed — schema composers write LSIS slots; `egress` is single-writer per pipeline

`compose_ocsf`, `compose_rfc5424`, and `compose_replayable` no longer write `egress` directly. Each writes its wire-form artifact to a dedicated LSIS slot (`workspace.lsis.ocsf` / `.rfc5424` / `.replayable`) and ships a companion one-line process `<slot>_to_egress` in the same file that moves the slot to `egress` when the pipeline emits that shape as its wire form. This makes `egress` single-writer per pipeline — `grep 'egress = ' packaging/snippets/` names the writer for every terminal, which is the invariant that makes envelope composition (below) possible without every schema and every envelope needing a separate bridging snippet.

Existing pipelines that used `... | compose_ocsf` at the terminal must add `| ocsf_to_egress` (or the equivalent for `rfc5424` / `replayable`) or the pipeline will no longer write `egress`. Every shipped doc and snippet example is updated; the docs sweep in this release adds the terminator explicitly to every pipeline shape that mentions a composer.

### Added — `compose_otlp` envelope composer

Introduces the envelope-composer role — a second composer layer that wraps an already-serialised payload string in a minimal OTLP-1.0.0 `ResourceLogs` proto and writes the encoded bytes to the LSIS slot `workspace.lsis.otlp`, with a companion `otlp_to_egress` for the pipeline terminal. The envelope reads a declared per-envelope input slot (`workspace.lsis.otlp_body`) and is agnostic to what the body string encodes — feeding a schema slot into the envelope is an explicit inline block in the pipeline:

```
process parse_x
      | compose_ocsf
      | { workspace.lsis.otlp_body = workspace.lsis.ocsf }
      | compose_otlp
      | otlp_to_egress
```

The same pattern works for any other schema slot (RFC 5424 record, replayable line, …), so schemas × envelopes stops producing per-pair bridging snippets and instead composes at the pipeline level. `compose_otlp` is deliberately envelope-neutral — it reads only its declared `otlp_body` slot, populates `host.name` from `hostname()`, and uses `received_at` for `time_unix_nano`; richer resource attributes and source-claimed timestamps are the caller's concern (override in a downstream process or write a `compose_otlp_<flavour>` variant that pulls more of LSIS).

### Added — five cloud parsers join the tracked set

Five cloud parsers become tracked snippets:

- `parse_aws_guardduty` — AWS GuardDuty findings JSON → LSIS Detection Finding (`2004`)
- `parse_aws_vpc_flow` — AWS VPC Flow Logs text records (v2 default + v5 custom formats) → LSIS Network Activity (`4001`)
- `parse_azure_activity` — Azure Activity Log events JSON → LSIS API Activity (`6003`)
- `parse_k8s_audit` — Kubernetes audit Event JSON (`audit.k8s.io/v1`) → LSIS API Activity (`6003`) + Authentication (`3002`)
- `parse_okta_system` — Okta System Log events JSON (System Log API v1) → LSIS Authentication / Account Change / User Management / Group Management (`3002` / `3001` / `3005` / `3006`)

The tracked vendor / cloud coverage now spans 29 vendor parsers (up from 24) plus the two transport parsers.

### Changed — every snippet header follows a per-kind schema; xtask enforces it

Header keys used to vary per snippet — some carried `Vendor:` / `Wire:` / `Category:` / `Upstream:` / `Intake:` / `Output:` / `Coverage scope:` / `Test corpus:` in various combinations. 0.7.11 introduces a strict per-kind key set (the file's parent directory selects the kind):

  parser   (5 keys): `Summary` / `Reads` / `Writes` / `Category` / `Test corpus`
  composer (4 keys): `Summary` / `Reads` / `Writes` / `Test corpus`
  filter   (4 keys): `Summary` / `Reads` / `Effect` / `Test corpus`
  function (3 keys): `Summary` / `Signature` / `Test corpus`

`Reads:` carries the stream contract as a small grammar: `ingress (raw wire) — …` for raw-wire parsers, or `workspace.<ns>.* (bridge — …)` plus one dot-line per intake field (`^\.<IDENT>\s+\((required|optional), <String|Int|Float|Bool|Object|Array|Timestamp>\)`) for bridge parsers, composers, and filters. Every free-form authored block (wire shapes, dispatch tables, sample inputs, security notes, per-leaf bridge examples) is preserved below the canonical keys as plain comment prose — the only key-shaped lines in every header are the canonical ones, so the lint's unknown-key warning stays meaningful.

`cargo xtask lint-snippet-headers` enforces seven guardrails at build time: canonical key presence and order, `Category:` value in a 17-entry whitelist (parser-only), `Test corpus:` first-token prefix (`real` / `public` / `synthetic` / `spec-only` for stream kinds; `unit` for functions), the `Reads:` dot-line grammar above, function `Signature:` ↔ `def function <name>(params)` cross-check in the same file, `Summary:` required across all kinds, and unknown keys as warnings. `cargo xtask gen-snippet-inventory` regenerates four BEGIN/END inventory tables in `packaging/snippets/README.md` from the headers; `--check` mode fails on drift so a header edit without an inventory refresh cannot merge silently.

Both commands are wired into CI's `check` job alongside `cargo fmt --all -- --check`, so the invariants stay green as a matter of course.

### Changed — snippet library README rewritten around LSIS

`packaging/snippets/README.md` gets a full rewrite: an LSIS definition and slot registry replace the earlier prose introduction, the hand-maintained per-kind tables become the four xtask-generated inventory blocks, *Design principles* grows from two to four contracts (adding the `egress` single-writer invariant and the two-layer composer contract), and *Authoring conventions* is rewritten around the per-kind header schema. The docs pages that used to define the intermediate in their own words (`docs/src/tutorial.md`, `docs/src/processing/design-guide.md`, `docs/src/snippets/README.md`) now link to the packaging README's LSIS section rather than paraphrasing it, so terminology cannot drift from a single source. `docs/src/operations/schema-validation.md`'s tap-process recipes are updated to read `.workspace.lsis.<schema>` (the slot the composer writes) instead of `.workspace.<schema>`.

### Fixed — `parse_k8s_audit` records the target resource's apiVersion, not the audit envelope's

The `parse_k8s_audit` record builder was populating `api.version` from `workspace.k8s.apiVersion`, which is the audit `Event` envelope's schema version (`"audit.k8s.io/v1"`, constant across virtually every event). The actually-useful `workspace.k8s.objectRef.apiVersion` (`"v1"`, the version part of `"apps/v1"`, …) was never read. The same field was missing from the `api.resource` path — despite the surrounding comment stating the intended shape was `apiGroup/apiVersion/resource`. Both spots now read `objectRef.apiVersion`, so an LSIS `6003` record from Kubernetes carries the API version the request actually targeted.

## [0.7.10] - 2026-07-09

0.7.10 is a focused pipeline hot-path performance recovery release. The single-core throughput on populated-workspace workloads had quietly regressed between v0.6.x and v0.7.9 as a chain of correctness-first refactors (the `refactor(queue,output): carry Event end-to-end` change in v0.7.7–v0.7.8, and the runtime-side `634cbd0` fix that followed) traded per-event heap allocation and scheduler-wake bookkeeping for structural safety. 0.7.10 keeps every correctness invariant intact and unwinds the allocation cost at the two boundaries it accumulated on (the pipeline output snapshot and the `ProcessChain` DLQ backup) plus the scheduler-wake tail on the queue consumer, and clears two per-event allocations from the `syslog_udp` write path that had a similar profile rank once the wake tail flattened. The OpenTelemetry proto stack is also lifted to the current upstream release so the DSL mapper and the batch-merge identity see the new wire fields. A single core now handles ~378k events/sec on passthrough (up from 313k at v0.7.9) and ~221k events/sec on the heaviest realistic DSL workload (OCSF Authentication compose + `to_json`, up from 62k at v0.7.9); the four-row Performance table in the README is refreshed alongside.

### Changed — pipeline `output` statement drops `workspace` from the memory-queue snapshot, and `tap output --json` no longer exposes `workspace`

Between v0.7.7 and v0.7.8 the `refactor(queue,output): carry Event end-to-end` change (`b7625bb`) collapsed the memory-queue path onto the same shape as disk-backed queues: every `output` statement started calling `event.to_owned()` on the borrowed pipeline event, which deep-clones the workspace `HashMap<String, OwnedValue>` on the hot path. Bench-harness measurements on the OCSF-compose workload (`parse_fortigate_cef | compose_ocsf | to_json | output`) show the single-core throughput dropped from ~168k eps at v0.6.0 to ~62k eps at v0.7.9 — a real regression driven almost entirely by that workspace clone plus the sibling `runtime.rs` clone the `634cbd0` fix put in.

0.7.10 moves the pipeline's `output` snapshot back onto a workspace-aware capture policy. The daemon precomputes which output names are backed by disk queues once at startup and hands the set into `run_pipeline` per event. On disk-backed queues the workspace is still captured (the WAL persists the full `Event` JSON and replay rehydrates it — skipping the capture would silently change on-disk semantics on the next restart). On memory-backed queues it is dropped: no downstream reader touches `workspace` — every sink's `consume` reads `egress` (with `file`'s dynamic path evaluator reading `source` / `received_at` and `kafka`'s optional key reading `source.ip`), every DLQ record projection stores only `OutputEvent`'s four fields (`source`, `received_at`, `ingress`, `egress`), and the analyzer already rejects `workspace` in output-side expressions at load time. `--test-pipeline` keeps the full capture so its CLI display shows what the pipeline built. Callers of the exported `pipeline::run_pipeline` gain an explicit `OutputCapturePolicy` argument (`StripAll` / `CaptureAll` / `DiskOnly`).

To make the operator-facing surface independent of the queue kind an operator happened to configure, `tap output --json` now projects `workspace` out of its JSON shape unconditionally, regardless of whether the underlying `Event` snapshot carries it. The projection lives in the tap emission layer, so disk-backed queues that still carry `workspace` on their queue payload never expose that difference to `tap output` subscribers. `tap process <name> --json` and `tap input <name> --json` are unchanged — process bodies are the only DSL construct that mutates workspace, and the named process tap emits at process exit, so any workspace observation an operator used `tap output --json` for is available one hop back via `tap process <last_named_process>`. Inline (unnamed) process blocks have no tap point; the workaround is to give a process you want to observe a `def process` name. This is a breaking change for pipelines that relied on the pre-0.7.10 `.workspace.<key>` extraction from `tap output`; README, `docs/src/operations/tap.md`, `docs/src/operations/cli.md`, `docs/src/operations/schema-validation.md`, and `docs/src/pipelines/multi-host.md` are updated to redirect their `.workspace` recipes to the equivalent `tap process` shape.

### Changed — pipeline runtime snapshots `OutputEvent` (not the whole `OwnedEvent`) before the queue send

`runtime::process_event` used to call `event.to_owned()` before `sender.send` so the sender's move would not leave the caller without the event. The `to_owned` deep-cloned every workspace value on the success path (the overwhelming majority path) only for the snapshot to be dropped once the sink took the queue-side `Event`. 0.7.10 takes a lightweight `OutputEvent` snapshot instead — the four fields the queue consumer actually reads (`source`, `received_at`, `ingress`, `egress`, with `Bytes` refcount bumps in place of deep clones). No downstream contract changes; the memory-queue hot path stops paying the workspace-deep-clone cost per event.

### Changed — `ProcessChain`'s DLQ backup is now an arena-local shallow snapshot

`PipelineStatement::ProcessChain` calls `let backup = current.to_owned();` before every `Named` / `Inline` process element so the `Err` arm has a stable owned event for the DLQ record. The correctness contract only requires the backup to survive until the `match` returns `Ok(...)`, so it doesn't need to leave the arena. 0.7.10 adds `BorrowedEvent::snapshot_in(&arena)`: the snapshot has its own arena-side workspace index vec but shares the arena-side key slices and `Value<'bump>` referents with the source view — cost model is one bump alloc + a memcpy of the workspace `(ptr, len, value)` triples + two `Bytes` refcount bumps, no `HashMap` allocation, no `String` key allocation, no `Value` tree materialization. `to_owned()`'s full heap materialization only fires on the actual `Err` arm at DLQ-record build time. Three pin tests lock the shallow-copy semantics: adding a workspace key on the source view after the snapshot does not appear on the snapshot; replacing an existing workspace slot on the source view leaves the snapshot's matching entry unchanged; reassigning `egress` on the source view leaves the snapshot's `egress` bytes untouched. DLQ record schema, `ProcessRegistry::call`'s trait signature, and every observable behaviour of the four DLQ code paths (Named/Inline × per-error/never-fired) are unchanged.

### Changed — syslog output hot path: peer display precomputed, per-event UDP send timeout retired

Two changes to the `syslog_udp` / `syslog_tcp` write path that had non-negligible rank on the profile once the workspace-clone regression was cleared:

- `Peer::address(&self)` used to re-run the `host:port` `format!()` on every call. The syslog_udp / syslog_tcp write closures called it eagerly for a value the success path never reads (only the error-context branches interpolate it into a `format!("... {}", address)`). 0.7.10 precomputes `display_address: String` once at `Peer::new(host, port, tls)` construction; `Peer::address(&self)` returns `&str`, so the write closure binds it as a cheap pointer/length rather than allocating a new `String` per event. Behaviour is preserved verbatim (IPv6-bracket handling included, pinned by existing `syslog_peers::tests`).
- `syslog_udp`'s send call sites had a `tokio::time::timeout(PEER_WRITE_TIMEOUT, socket.send(&egress))` wrapper. Even when the wrapped future returns Ready on first poll (the overwhelming common case), `Timeout` still constructs a `Sleep` on the tokio timer wheel per send and cancels it on Ready. Connected UDP has no remote-peer wait state to time out on — the sender never blocks on the receiver reading; the only terminal states are success, immediate error (`ECONNREFUSED` / `ENETDOWN` / async ICMP), and brief writable-Pending on a full local send buffer. A 10-second wall-clock cap over that pattern only fires under kernel pathology, which the syslog output path cannot recover from anyway. 0.7.10 drops the wrapper on `syslog_udp`'s two send call sites; a full local send buffer now flows back as ordinary async backpressure through the queue instead of a `syslog_udp send to <peer> timed out` → peer-cool → 5 s cooldown → retry cycle. `syslog_tcp` keeps its `PEER_WRITE_TIMEOUT` wrapper — TCP genuinely has "remote peer stopped reading" and "RST-less network partition" states where a `write_all` hangs indefinitely without a timer to interrupt it; the asymmetry is by design and the per-site comment on the syslog_udp send calls it out. `syslog_udp`'s `PEER_CONNECT_TIMEOUT` wrappers on `tokio::net::lookup_host` and `UdpSocket::connect` also stay — DNS lookups and UDP `connect` can genuinely hang on misconfigured resolvers and are pre-send phases with no wire visibility.

### Changed — queue consumer drains events in batches via `QueueReceiver::recv_many`

The bounded mpsc queue between the pipeline worker and each output's consumer task was paying a full park/wake round trip per event: `input = receiver.recv(), if accepting =>` in `run_queue_consumer` woke the consumer, drained one event, and returned to the select loop where — under a typical arrival gap that sat near the parking threshold — the consumer parked again microseconds before the next event landed. `perf stat` showed `context-switches / event` on parse-heavy workloads reaching values that no per-event CPU work justified.

A prior throwaway prototype that removed the queue boundary entirely and drained inline measured net loss on both target workloads (−7.5% and −27.2% median eps), which localised the answer: the boundary itself is worth its wake cost because it lets the producer parse the next event while the consumer sends the previous one. 0.7.10 keeps the boundary and reduces the *per-wake* cost.

`QueueReceiver` gains `try_recv` (a non-blocking peek — safe here because the outer consumer loop re-enters through `recv().await` after each batch, so a permit-holding sender whose write races the `try_recv` is picked up on the next iteration; the shutdown drain still uses `close() + recv().await`-until-`None` unchanged, and a structural pin test guards the split from a mechanical refactor) and `recv_many(&mut buf, max)` (block until one event via `recv()`, then greedily drain up to `max - 1` more via `try_recv`; cancel-safe because the only `.await` is the initial `recv()`, and the greedy phase runs to completion once the first event lands). `run_queue_consumer`'s steady-state arm replaces per-event `recv()` with `recv_many` against a reused batch buffer of capacity `RECV_BATCH_MAX = 64`. Ack / cursor semantics, backend-aware shutdown drain, wedge semantics, per-output retry loop, DLQ routing, and every output-side lifecycle contract are unchanged and pinned by the existing tests. Measurement across three test environments spanning x86_64 and aarch64 shows the passthrough / `syslog_udp` shape recovering +10.6% to +20.1% median eps and the parse / `syslog_udp` shape recovering +13.7% to +25.7%, with `context-switches / event` down 8–29% depending on shape.

### Changed — queue consumer adds an adaptive spin-before-park controller inside `recv_many`

Batching amortizes cost per wake; it does not reduce the *number* of wakes when the producer's inter-arrival gap sits near the parking threshold. 0.7.10 adds a small adaptive spin phase in `recv_many`, driven by a three-integer state machine (budget, evidence, evidence-age) with five rules:

- **R1**: a spin hit doubles the budget (saturating at `SPIN_CAP = 128`) and records the hit depth as evidence.
- **R2**: a spin miss halves the budget, but not below the deepest evidence-recorded arrival depth — a plain halving would let a single anomalous long gap decay the budget below the arrival mode, after which hits become structurally impossible and the controller sinks to an absorbing zero.
- **R3**: evidence stales after `EVIDENCE_STALE_AFTER = 16` consecutive misses (halves), so an idle daemon's budget floor eventually decays to zero.
- **R4**: a park shorter than `PSEUDO_HIT_PARK_NS = 10 µs` — a park that spinning would plausibly have caught — is treated as a hit for growth purposes, so the controller can escape the absorbing zero when load returns.
- **R5**: with `budget == 0` the spin phase is skipped entirely, and the receive path is byte-identical to the pre-controller shape.

The 60-second idle-CPU gate on the merged tip reports 0.0000% CPU across every measured cell — the R5 short-circuit does its job. The trickle-load gate at 100 events/sec is within 0.033% absolute of the pre-controller shape — R3 staleness does its job. On parse-heavy workloads the controller adds a further +4.3% to +7.3% median eps on the cloud environments where the batching alone left residual wake tail; on the shallow passthrough shape the isolated delta is inside the run-to-run noise on every environment — but that is expected, and the controller's value on today's workloads is dominated by the safety it provides against the balance-point drift that a future environment or workload will hit.

### Changed — OpenTelemetry stack upgraded to `opentelemetry-proto 0.32`, `prost 0.14`, `tonic 0.14`

`opentelemetry-proto` from 0.27 to 0.32, `prost` from 0.13 to 0.14, `tonic` from 0.12 to 0.14. Four new proto surfaces were exposed to the DSL mapper and the output batch-merge identity so wire-level fields the previous stack could not see stop being silently lost. `AnyValue::StringValueStrindex` is routed through the range-checked field helper; the OTLP schema-version string and the gRPC TLS-feature string references are updated to the versions the new stack ships with.

### Docs — README performance numbers refreshed to the v0.7.10 state

The four `events/sec/core` numbers in the Performance table are refreshed to v0.7.10 tip measurements — `passthrough 378k / syslog.parse 380k / parse + 2× regex + if/else 146k / OCSF compose + to_json 221k`. The lede sentence is retracked from ~168k to ~221k on the heaviest realistic DSL workload. The history paragraph gains a third milestone alongside v0.6.0 and v0.6.1: the v0.7.10 queue-consumer wake mitigation (batch-drain via `recv_many` plus the adaptive spin-before-park controller) that closed a wake-amplification tail on populated-workspace workloads. The multi-pipeline aggregate figure (~459k events/sec on OCSF compose) stays at the last measured value pending a future re-run.

## [0.7.9] - 2026-07-07

The 0.7.9 cycle started as a small follow-up release and grew into a substantial trust-boundary and runtime-security hardening pass across the queue lifecycle, the DLQ path, the filesystem and socket surfaces, and the systemd packaging. Every user-visible change is either additive (default off) or a fail-closed tightening of a previously silent-loss / silent-over-delivery shape. The one behaviour change that operators are most likely to notice is the default tracing fallback line becoming payload-free — see `error_log_fallback` below.

### Changed — DLQ tracing fallback is now an operator confidentiality choice (`control { error_log_fallback "off" | "meta" | "full" }`)

The runtime's tracing-side fallback for DLQ failures used to be one-shape-fits-all: whenever `error_log` was unset or its write failed, every emission site emitted the full failure JSONL through a `tracing::error!` `event_record` structured field. That shape survived because `journalctl | jq` was an easy operator recovery workflow, but it meant every operator with a non-file DLQ path — or a transient DLQ-write failure — implicitly exported ingress / egress payload bytes to whatever the tracing subscriber was attached to (journald, syslog forwarder, cloud log aggregation), even though the `error_log` file is a 0o600 boundary the operator had already tightened. 0.7.9 shifts the choice back to the operator via `control { error_log_fallback "off" | "meta" | "full" }` (default `"off"`):

| Value | Line body |
| --- | --- |
| `"off"` | (default) one-line failure summary — `kind`, `output` / `pipeline` name, `site`, `reason`, `fallback = "off"`. No payload, no metadata. |
| `"meta"` | structured metadata — adds `fallback = "meta"`, `timestamp`, `size` (bytes of the recoverable payload), and `position` (queue kind + numeric offset/seq only, no filesystem path). Still no payload bytes. |
| `"full"` | pre-0.7.9 shape — adds `event_record` carrying the full JSONL (ingress / egress bytes included). Restores the previous behaviour behind an explicit opt-in. |

Row-A rule: when `error_log` is unset the fallback value is ignored (the operator has already declared "no durable recovery needed" by omitting `error_log`, and honouring a stray `error_log_fallback "full"` on that path would contradict the declaration). Every DLQ emission site — steady-state per-event, shutdown-drain, ambiguous shutdown-drain, and the pipeline-side runtime error path — delegates to a single helper enforcing this policy, so the ladder shape is uniform across surfaces. `limpid --check` rejects invalid enum values (typos like `"Off"` or `"metadata"`) and warns on the inert combination `error_log_fallback` set + `error_log` unset. `docs/src/operations/error-log.md` documents the full ladder including the AckPosition Debug output (`Memory` or `Disk { seq, offset }` — no filesystem path leak). Operators who relied on `journalctl | jq event_record` for recovery must set `error_log_fallback "full"` explicitly to preserve that workflow.

### Changed — Batched outputs (`http`, `otlp_http`, `otlp_grpc`): wedge-aware shutdown resolves parked handles without new sends; `SIGKILL` operator runbook retired

The pre-0.7.9 batched sinks left `QueueAckHandle` entries parked inside their internal buffer unresolved when the fail-stop wedge fired, forcing the runtime's 10 s wall-clock shutdown timeout to unblock the drain and leading operators to `SIGKILL` in production. `Output` gains a new `shutdown_wedged()` trait method (default no-op for unbatched sinks); on the wedge-exit path the queue consumer calls this instead of `shutdown()`, and the batched sinks resolve their parked handles through the same `route_shutdown_batch_ambiguous_to_dlq` helper that already handles ambiguous graceful-shutdown drain. No new transport call is attempted on the wedge path — the wedge contract says "no new work through a bug-path output" — so disk queues keep the wedged cursor for next-start reconciliation, memory queues fold to `Recovered` for lack of a replay path, and the consumer exits within the graceful shutdown budget without `SIGKILL`. The `currently leaked` code comment and the `SIGKILL`-required batched-wedge operator runbook are retired across `docs/src/outputs/README.md`, `docs/src/operations/error-log.md`, and the per-output pages.

### Changed — `output http`: `tls { ... }` block on a plaintext (`http://`) URL is now rejected at load time

The `http` output's peer parser previously accepted `http://` (or scheme-less) URL + `tls { ... }` block, emitted a `tracing::warn!` at build time, and continued startup. reqwest only engages TLS for `https://`, so the configured CA / client certificate / client key were silently dropped and the daemon shipped requests in cleartext to that peer — under a config the operator had tightened to exactly the opposite. Load-time rejection is now `bail!` with the same wording used by the sibling `output otlp_http` and `output otlp_grpc` guards. Configs mixing `http://` and a non-empty `tls { }` block will fail startup / `--check`; the remediation is either drop the tls block or switch the URL to `https://`.

### Changed — Journal input `match` property is now `repeatable`; `match_add` failure fail-closes the reader instead of silently over-delivering

The `input journal` `match` property was single-valued (`repeatable: false`) and its runtime path warn-then-continued whenever `sd_journal_add_match` rejected a filter — the reader then tailed the entire journal unfiltered. An operator writing `match "syslog_identifier=myapp"` (lowercase — libsystemd rejects it) would see a single warn line at startup and their downstream would receive every log record on the system. 0.7.9 flips the schema to `repeatable: true` (libsystemd combines same-field matches as OR and different-field matches as AND; `docs/src/inputs/journal.md` documents the combining rules) and reworks the runtime path so a `match_add` rejection now terminates the reader with a loud diagnostic listing every rejected filter. Semantically: if libsystemd cannot install the filter, no journal entry can ever satisfy it, so zero events is the correct output. `limpid --check` and daemon startup both reject match strings that lack the required `FIELD=value` separator. A separate bug — the first-start seek pointer becoming implementation-defined when the match view had no past matching entries (e.g. a brand-new `SYSLOG_IDENTIFIER` that has never been logged) — is fixed by falling through to `sd_journal_seek_head` when `previous()` returns 0; new matching entries arriving after daemon start now surface correctly.

### Fixed — `input journal` / `output file` / `output unix_socket` / `input unix_socket` / `control socket`: filesystem trust boundary is fail-closed across every touched path

A cross-cutting audit of the sockets and files the daemon opens for read or write closed several silent-loss and silent-elevation shapes. Every case follows the same pattern: what used to be a `warn!` at startup is now a startup / `--check` error, and what used to be a bind-time race is now a per-connect verification.

- **`control` socket parent trust.** The parent directory of `control { socket "..." }` is now verified for both trusted ownership (root or the daemon's own euid) and safe mode (not group- or world-writable) at startup. A pre-existing symlink parent is refused (a symlink lets an attacker redirect the bind target between the preflight and the daemon's `bind`). The absent-parent branch creates the parent atomically under a trusted ancestor and re-`symlink_metadata`s to confirm shape / owner / mode. Under packaged systemd units (`RuntimeDirectory=limpid` with `RuntimeDirectoryMode=0750`) this is a no-op except for the symlink check; the change targets custom deploys where the parent was ambient.
- **`input unix_socket` peer verification.** The listener now verifies peer credentials on every accept: `{daemon euid, root}` by default, or a strict `expected_peer_uid` override when configured. Verification runs per-connect rather than once at bind, mirroring the runtime auth model canonical `/dev/log` shape uses. `chmod 0o666` on the bound socket also now fails startup (previously warn-and-continue).
- **`output unix_socket` peer verification.** The `output` side of the same surface now verifies the peer's identity every connect so a co-tenant cannot squat the socket path between daemon restarts.
- **`output file` non-regular file rejection.** The file output rejects a non-regular target (symlink, FIFO, socket, directory, device node) at open and `fstat` verifies immediately after — a stale FIFO from a debugging session or a symlink pointing at `/dev/log` no longer redirects output. Owner / group mode is applied via `fchmod` on the fresh fd rather than `chmod` on the pathname, closing the TOCTOU gap between the shape check and the mode application. Docstring and behaviour around the birth mode / owner-unset uid fallback are documented in `docs/src/outputs/file.md`.
- **`error_log` (DLQ file) preflight.** Daemon startup opens the DLQ file with `O_NOFOLLOW | O_NONBLOCK` and refuses any non-regular shape (mirroring the file-output rule). The `create` branch follows up with `fchmod(0o600)` and an `fstat` re-verify on the fresh fd; the `existing` branch runs the same `fstat` against the opened fd, closing the TOCTOU gap between an earlier `symlink_metadata` and the runtime `open(2)`.

### Fixed — `output http` / `otlp_http` / `otlp_grpc`: shutdown-cancelled batched sends route as ambiguous (Dropped-on-disk) instead of fabricating `Recovered`

The batched-sink graceful-shutdown drain resolved every drain-failure event as `Recovered` regardless of wire state. When a shutdown signal cancelled an in-flight `flush_events` mid-`send`, or when the 3 s `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` fired after the first request byte had already left the kernel, the wire state was ambiguous — the batch may have partially reached the peer — and a `Recovered` disposition on the DLQ record set up a double-delivery on next-start replay. 0.7.9 splits the drain failures by wire-state provenance. A per-event render failure or a retry-exhausted terminal drop still routes as `Recovered` (proved pre-boundary). An in-flight cancel or the attempt timeout routes through the ambiguous-DLQ helper: `Dropped` on a disk queue so the fail-stop wedge holds the cursor for next-start reconciliation against the DLQ record, folded to `Recovered` on a memory queue (no replay path exists). The public docs for the three batched outputs, `outputs/README.md`'s disposition table, and `operations/error-log.md` all state the split explicitly.

### Fixed — Sink-side DLQ helpers (`route_event_to_dlq` and friends) now carry the queue `AckPosition` through to the tracing fallback

Hardware validation of the `Meta` ladder arm surfaced a drift: for the most common DLQ route (a sink's steady-state retry-exhausted `route_event_to_dlq` call), the emitted tracing line carried `position="(none)"` even under a disk queue configuration that should have produced `Disk { seq, offset }`. The `Meta` shape is meant to give the operator the identifier they need to reconcile a wedged disk-queue cursor front against the DLQ record; without it the memory-queue-lossy vs. disk-queue-wedged distinction the ladder tries to expose collapses. 0.7.9 promotes `position` to a required parameter of the sink-side helper trio (`route_event_to_dlq`, `finalize_shutdown_singleton_disposition{,_ambiguous}`), and every sink call site now hands it `ack.position()` on the very next statement. A new structural pin refuses to compile a signature that weakens the invariant back to `Option`. The tracing-side `Full` and `Off` arms are unchanged; only the `Meta` arm gains the field it always promised.

### Fixed — Kafka output: shutdown-cancelled `producer.send` routes ambiguous; the outer wrapper that was cutting librdkafka's delivery attempt short is removed

`output kafka`'s shutdown path had two overlapping silent-loss shapes. `producer.send` on the graceful-shutdown drain was treated as pre-boundary on cancellation, but kafka's send is fire-and-forget from limpid's side — once the request byte hits librdkafka's queue the delivery status is decided asynchronously — so shutdown cancelling mid-send left the delivery ambiguous while the DLQ record marked `Recovered`, setting up a double-delivery on replay. Separately, the earlier `pre_send_or_shutdown` wrapper limpid put around `producer.send` at shutdown cut librdkafka's delivery attempt short at limpid's side at the outer 5 s, opening the same duplicate-delivery gap on any timeout that fired mid-send. 0.7.9 routes shutdown-cancelled kafka sends through the same ambiguous-DLQ helper the batched sinks now use (`Dropped` on a disk queue so the fail-stop wedge holds the cursor for next-start reconciliation against the DLQ record, folded to `Recovered` on a memory queue for lack of a replay path) and removes the outer wrapper so the pending-envelope wait is now bounded by librdkafka's own `queue_timeout` and `message.timeout.ms` — the same budget librdkafka enforces in steady state, which is the shape the retry budget is designed against. A structural pin in the kafka module forbids re-introducing the outer wrapper. The `output kafka` docs are rewritten to describe the actual shape end-to-end, so operators no longer expect a 5-second outer wrapper the code does not enforce.

### Fixed — `output syslog_udp` / `output syslog_tcp`: shutdown-drain successes no longer double-count `events_written`

The sibling unbatched-with-frames sinks each had one path that bumped `events_written` twice on a shutdown-drain success (once inside `write_payload_shutdown_aware`, once in the `Ok(())` arm of the surrounding retry loop), yielding operator counters that overshot the real delivered count when the shutdown drain fired. The bump is unified at the caller boundary; both sinks now count exactly once per successful ship regardless of steady-state / shutdown-drain path.

### Fixed — `input tail`: rotation semantics tightened; late acks under generation N never surface as post-rotation N+1 watermark

Follow-up hardening on top of the per-event-ack fix in 0.7.8. The rotation identity pin now tracks by dev/ino rather than by pathname (a hardlink swap between old and new file could otherwise mis-detect the rotation), and the generation counter on ack handles is verified against the rotation frame at cursor-write time so a late ack from an in-flight worker cannot land as the post-rotation watermark. Documented in `docs/src/inputs/tail.md`.

### Fixed — `--check`: `error_log_fallback` invalid enum value is rejected before daemon start

The runtime path parses `error_log_fallback` at daemon startup and refuses to start on an unknown value. `--check` previously did not run the value-level enum parse — the schema layer enforced only the property *type* (String), so `error_log_fallback "Off"` (uppercase — libsystemd `off` is required) or `error_log_fallback "metadata"` (typo of `meta`) passed `--check` silently and only failed at daemon restart, minutes later, from a different host. `check::recovery_readiness` now runs the same `ErrorLogFallback::parse` the runtime does and emits `Level::Error` for any unknown value, so CI catches the typo before deploy time.

### Added — Prometheus / limpidctl surface

- `events_wedged` and the sink-side `events_errored_unwritable` are now exposed on `/metrics` alongside their pipeline-side counterparts. The full set is documented in `docs/src/operations/metrics.md`.
- `limpid-prometheus` caps concurrent connections and applies a whole-conversation deadline (previously only the query itself was bounded, a slowloris-style client could pin the endpoint). The threat model is documented in `docs/src/operations/prometheus.md`.
- `limpidctl` validates names client-side and now returns reliable exit codes (previously a server-side rejection sometimes exited zero).

### Added — TLS: explicit `>= 1.2` floor across every TLS-consuming module; `verify false` is a `--check` warning

Every module that consumes TLS (`output http`, `output otlp_http`, `output otlp_grpc`, `output syslog_tcp`, `output unix_socket`, `input syslog_tcp`, `input unix_socket`) now pins its minimum TLS version to 1.2 explicitly rather than inheriting whatever the underlying crate defaults to. rustls does not implement 1.1 / 1.0, so the change is behaviourally a no-op on the rustls backend — but the floor is stated at the module boundary instead of at the crate boundary, so a future backend swap cannot silently regress. `verify false` (the top-level HTTPS certificate-verification disable) now emits a `--check` warning as well as the pre-existing startup log line, so the footgun surfaces at CI time.

### Changed — Packaging: systemd unit hardening; `limpid-prometheus` runs as `DynamicUser`

The packaged systemd units (`limpid.service` and `limpid-prometheus.service`) receive a hardening pass. `limpid-prometheus` moves to `DynamicUser=yes` with an ephemeral home so the metrics endpoint runs in a dedicated sandbox instead of sharing the daemon's `syslog` uid; `limpid.service` tightens the standard sandboxing directives (`PrivateDevices`, `ProtectSystem`, `ProtectHome`, `ProtectKernelTunables`, etc.) and documents the `/dev/log` drop-in operators need when consuming syslog on that path. `postinst` consolidates the systemd enable/start into `debhelper` invocations to avoid a race where the pre-hardened unit could start briefly under the wrong sandbox before the packaged version took effect. Refer to `docs/src/operations/packaging.md` for the full unit walkthrough.

### Changed — CI supply-chain gate; Kafka feature added to CI matrix; `cargo-audit` retired in favour of `cargo-deny`

`deny.toml` has been in the tree since day one but CI never invoked it; new dependencies that failed advisories / bans / sources / licenses only surfaced during manual pre-push audits. CI now runs `cargo-deny` pinned at `0.18.6` on every push. `cargo-audit` is retired: the current release `0.21.2` cannot parse RustSec advisory records emitted with a CVSS 4.0 vector (`RUSTSEC-2026-0124` on `libcrux-chacha20poly1305` is the concrete example), which broke every CI run when it first appeared; `cargo-deny 0.18.6`'s advisories check already handles CVSS 4.0 and covers the same ground, so running both was redundant on the advisory dimension. A new `check-kafka` job compiles the workspace with `--features kafka,journal` so a regression on either surface fails the PR gate rather than emerging at tag time; installing the shared `libsasl2-dev` / `libssl-dev` / `libsystemd-dev` / `libcurl4-openssl-dev` / `zlib1g-dev` / `libzstd-dev` / `liblz4-dev` apt packages once per PR run captures both features' build cost.

### Docs

Beyond the entries above:

- `docs/src/operations/error-log.md` grows a "Tracing fallback ladder" section, a "Disposition contract and fail-stop wedge on disk queues" writeup, and an "Ambiguous shutdown-drain" callout — the DLQ recovery model is documented top-to-bottom.
- `docs/src/outputs/README.md` reshapes the disposition contract table around the ambiguous-shutdown-drain lineage and cross-links each batched output's per-page shutdown section to the shared story.
- `docs/src/inputs/journal.md` gains the `match` combining rules table (same field → OR, different field → AND) plus the semantic model for a libsystemd-rejected filter (zero events is correct; fix the filter and restart).
- `docs/src/configuration.md` documents both `error_log` and the new `error_log_fallback` property side-by-side in the `control { }` reference.

Consult the individual per-page docs for full recipes; the runbook and metrics guides are the canonical operator-facing surface and this release brings them into lockstep with the code.

## [0.7.8] - 2026-06-27

### Changed (BREAKING) — `secondary <name>` removed from every output

The `secondary <name>` output property is removed from the schema and the runtime. Earlier in this cycle the spec was lifted from OTLP-only to every output type and hardened against typos / cycles / quoted-string shapes, but the recovery story it served has been superseded by the `control { error_log "..." }` path: retry exhaustion, runtime enqueue failure, pipeline eval errors, and shutdown-flush failures all drain through the error log with a uniform DLQ schema (see the DLQ-v2 entry below), and an operator replays via `limpidctl inject output <name>` rather than a configured fallthrough at write time. The two recovery paths overlapped — a misconfigured secondary that fell through silently was a sharper data-loss footgun than no secondary at all — so the redundant one is gone. Remediation: drop any `secondary <name>` lines from output configs; configure `control { error_log "..." }` on the daemon and route operator replays through `limpidctl inject`. The `--check` warning added earlier in the cycle for "recovery-dependent outputs without `error_log`" still fires and is the single signal to grep for at upgrade time.

### Changed (BREAKING) — DLQ wire format split into `Process` / `Output` sum type (`schema_version: 2`)

`error_log` JSONL records now carry a top-level `schema_version: 2` and a `kind: "process" | "output"` discriminator selecting between two variants. `Process` records cover `def process` invocations, inline `process { ... }` blocks, explicit pipeline-level `error` statements, and uncaught expression-evaluation errors; the captured event carries ingress / source / received_at only (egress is dropped because at a process failure point it may hold partial output of an earlier step in the chain, which would confuse `inject input` replay). `Output` records cover runtime-side enqueue failure, sink-side retry exhaustion, and batched-output shutdown drain; the captured event carries both ingress and egress so `limpidctl inject output <name>` can hand the rendered payload directly to the sink's `consume()`. The only sink-routing metadata captured is the output name — no address, dest, path, key, topic, partition, endpoint, URL, peer, target, or workspace at any level. Runtime-side enqueue failure emits one record per failed output rather than joining them, so each record is independently replayable. `jq` recipes built against the v1 `process: "<discriminator>"` layout need updating; the `schema_version` field is the operator's machine-readable signal that a migration is required. The error-log operator runbook in `docs/src/operations/error-log.md` documents the new shape and replay flow end-to-end.

### Changed — `Output` trait collapsed to a single `consume(&Event)`; queue carries `Event` end-to-end

The pre-0.7.8 `Output` trait carried three per-event entries (`render` on the hot path, `write` on the consumer side, `consume_event` as the batched opt-out) and the queue's `SinkInput { Owned(Event) | Rendered(RenderedPayload) }` discriminator decided which path each event took. The split was a relic of the early-render design that this restructure eliminates: every sink now sees `&Event` directly and decides internally whether to ship inline or buffer for a later flush. **Net silent-loss fix**: under the previous shape, retry exhaustion on a pre-rendered `Rendered` payload left the consumer with no `Event` to hand to `error_log`, so the payload was silently dropped — that path is gone, every exhausted retry now lands in the DLQ when configured. Render errors are deterministic on the event and bypass the retry budget by routing straight to recovery via a `modules::RenderError` wrapper that `write_with_retry` downcasts on. The discriminator-based `QueueSendError::RenderedOnDisk` variant and the `Owned`-vs-`Rendered` queue split are removed. Operator-visible: the `inject output <name>` replay path now works uniformly across all output types; the silent-drop edge case on Rendered-on-retry-exhausted is closed.

### Changed (BREAKING) — Output configs reject references to pipeline-mutable state

Output configuration templates can no longer reference `workspace`, `egress`, or `error`. The structural invariant after this change: transport metadata may only depend on event-intrinsic fields (`source`, `received_at`, `ingress`) and sink-internal state. The historical `path "/var/log/{workspace.tenant}/app.log"` shape compiled fine pre-0.7.8 but meant the operator-facing destination was decided by whichever DSL line happened to write `workspace.tenant` upstream — invisible from the output config and untraceable when an event went to the wrong file. Same hazard for `key_field workspace.user_id` on `output kafka` and any analogous template. Operator-facing routing decisions belong in the pipeline body — split into multiple outputs via DSL conditionals, each with a static or event-intrinsic destination. The analyzer rejects the offending references at parse time, `limpid --check` flags them, and the daemon refuses to start (and reload) configs the analyzer would reject. Remediation: move the decision into the pipeline (e.g. `switch tenant { ... output tenant_a; ... output tenant_b }`) and give each output a static path / key.

### Fixed — `input tail` / `input journal`: cursor advances only after pipeline ack

Both filesystem-cursor inputs previously saved their on-disk offset / cursor immediately after `tx.send(event).await` returned `Ok`. The return only confirmed the per-input mpsc channel accepted the event, not that the pipeline worker processed it — a process crash or `kill -9` between the send and the worker terminating lost every event in flight, and on restart the cursor sat past those events so they were never re-read. Each event now carries an optional `Arc<AckHandle>`; the pipeline worker drops the handle after the per-event work reaches `Finished` / `Dropped` / `Errored`, and the input only writes the cursor for offsets / cursors whose handles have fired. Events that did not reach a terminal state (or that panicked mid-process) leave their handle un-fired and the cursor stays put, so the restart re-reads them. DLQ replays and disk-queue replicas deliberately do not carry the ack — the upstream input-side handle has already done its job. Event size grows by 8 bytes for the `Option<Arc<AckHandle>>`.

### Fixed — `input tail`: late acks from a slow worker no longer poison the post-rotation watermark

Follow-up to the per-event ack fix above. The initial fix added a queued-ack drain at rotation / truncation time so any acks waiting in the input's mpsc receiver from the previous file would not be folded into the new file's watermark — but that drain cannot see an `Arc<AckHandle>` still held by a pipeline worker mid-processing across the rotation boundary. When such a worker eventually dropped its handle, the old absolute byte offset was sent to the input under the new file's byte namespace, the input saved it as the post-rotation watermark, and the next start silently skipped bytes `0..old_offset` of the new file. The data loss was silent: only across restart, only on the rotation path, only when the pipeline was slow enough to outlive the rotation. Each ack is now namespaced by a generation counter; the tail input bumps its generation alongside the existing offset reset on rotation / truncation, emitted events carry the current generation in their ack handle, and the ack-receive path filters out any ack whose generation does not match. Journald is unchanged — `journald` cursors are globally monotonic and uniquely identify entries across boots, so there is no namespace ambiguity to resolve.

### Fixed — `output http` / `otlp_http` / `otlp_grpc`: shutdown closes the handle-leak vectors via a long-lived flusher actor

The shutdown path on the three batched sinks had two compounding handle-leak vectors that survived the earlier `shutdown()` hook. (1) The per-flush timer task owned the in-flight batch (and the parked `QueueAckHandle`s it carried), and a runtime shutdown deadline that aborted the timer-mid-send dropped every handle unresolved — debug builds panicked on the resolve invariant, release builds silently routed through the `Dropped` fall-through (`events_failed` bumped without a DLQ record). (2) Even after the timer was restructured, the inline `consume()` path and the `batch_size <= 1` `consume_singleton` path still ran the transport await on the queue consumer's task; a stalled peer pinned the consumer past its own shutdown signal, and the runtime's 10 s timeout aborted the consumer with the stack-local shippable Vec still holding parked handles — same fall-through. The fix is a single long-lived flusher actor per sink that owns the buffer and every send: `consume()` only pushes to the buffer and notifies the actor; the actor is the only abort-able surface and is the only task that ever holds a transport await. Shutdown is fully cooperative — `is_shutting_down` plus `notify_waiters` races the retry sleep *and* the in-flight `send_batch` future, so the actor exits with every stack-local handle resolved before the bounded join in `shutdown()` completes. A required `Output::consume_shutdown` trait method (no default impl, forced override) replaces the consumer's drain-loop `consume` call so unbatched outputs make one bounded send attempt then route to DLQ rather than reentering the steady-state retry budget. After this change the queue consumer's task is never blocked in a transport await, and the audited leak class is closed across both batched and unbatched outputs.

### Fixed — `output http` / `otlp_http` / `otlp_grpc`: shutdown drain bounded at one send attempt with a per-attempt timeout

The shutdown-flush helper previously called the full retry loop on the final drain, which could spend the entire shutdown budget on a single stalled peer's exponential backoff while the rest of the buffer waited behind it. The drain helper now makes exactly one bounded send attempt per remaining payload — wrapped in a `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` ceiling so a peer that accepts the connection but never replies cannot stall the drain past the runtime's shutdown deadline. Successful attempts ack the queue; failures route to the DLQ via the same `error_log` path documented for retry exhaustion. The previous "retry retains the buffer for the next tick" shape is preserved during normal operation (only the shutdown drain is bounded).

### Fixed — `output otlp_http` / `output otlp_grpc`: `time_unix_nano` now preserves `Value::Timestamp` values

The documented operator pattern `time_unix_nano: received_at` silently encoded a wire-level 0. `received_at` evaluates to `Value::Timestamp`, but the OTLP encoder's `u64_field` helper only matched `Value::Int` / `Value::Float`, returning `None`; both call sites (`time_unix_nano` and `observed_time_unix_nano` in `hashlit_to_log_record`) fell through to `.unwrap_or(0)`, so the proto default suppressed the field on the wire with no warning. The helper now carries a `Value::Timestamp` arm that converts via `timestamp_nanos_opt()` (returning `None` outside the 1677–2262 representable range, matching OTLP's u64-nanos-since-epoch contract) and rejects pre-1970 instants through `u64::try_from`. The conversion is now the single source of truth for u64 fields in the log-record encoder, so this one-line fix covers every affected path.

### Fixed — `output otlp_http` / `output otlp_grpc`: warn once when a `time_unix_nano` value is present but uncoercible

After the `Value::Timestamp` arm above closed the type-coverage hole, `time_unix_nano` / `observed_time_unix_nano` still fell through to a silent proto-default `0` whenever the value could not be coerced — a typo'd field, the wrong type, or an out-of-range timestamp. Operators only discovered the loss by inspecting the wire output. The encoder now distinguishes the two failure shapes: key absent stays silent (legitimate per OTLP — the receiver, or a downstream pipeline, may fill the timestamp server-side); key present but uncoercible emits a `tracing::warn!` on the first occurrence per field per process (gated by an `AtomicBool::swap`) then suppresses. Per-call-site flags mean `time_unix_nano` and `observed_time_unix_nano` are reported independently; suppression prevents a broken upstream from flooding logs at wire rate; process restart re-arms the warning.

### Fixed — Docs: OTLP outputs now show `coalesce(event_time_ns, received_at)` in the pipeline contract example

A missing key encodes as `timeUnixNano: 0` on the wire (proto3 default) and the encoder only warns on present-but-uncoercible values — absent keys are silent. Operators who copy the compose example without the upstream parser snippet would otherwise ship time-stripped events without any signal. `outputs/otlp_http.md`, `outputs/otlp_grpc.md`, `functions/expression-functions.md`, and the OTLP timestamp section in `otlp.md` now all show the defensive `coalesce(event_time_ns, received_at)` pattern with an explicit cross-reference to the encoder's warn semantics.

### Fixed — Docs: OTLP `partial_success` per-event attribution called out as approximate

OTLP collectors report `partial_success.rejected_log_records` as a batch-level count, not as per-record identity. limpid splits the batch into `Delivered` + `Recovered` along the trailing N entries (where N = `rejected_log_records`) and routes the recovered tail to `error_log`, but the attribution is approximate: the collector did not identify those specific events as rejected. The `outputs/otlp_http.md` / `outputs/otlp_grpc.md` Notes sections now call this out explicitly, framed for replay decisions (metric totals are accurate; per-event attribution is not). `operations/error-log.md` adds a "partial_success attribution caveat" paragraph alongside the existing shutdown-drain caveat — DLQ records whose `reason == "collector reported partial_success rejection"` are a batch-level split, not a per-record identification.

### Fixed — `limpidctl --replay-timing` accepts the canonical i64-nanos `received_at`

`extract_timestamp` parsed `received_at` as an RFC3339 string, but the canonical Event JSON wire form (`Event::to_json_value` / `Event::from_json`) is i64 unix nanoseconds matching OTLP `time_unix_nano`. A round-trip from `tap --json` or a DLQ JSONL file into `inject --json --replay-timing` failed with a misleading "`received_at` field is not a string" error — even though the file the operator was replaying came straight out of limpid's own JSON emitters. The parser now uses `as_i64()` + `from_timestamp_nanos`, the rejection cases flip to reject strings and floats, and a canonical-shape fixture pins the `tap --json | inject --json --replay-timing` round-trip as a regression guard. The stale "Unix-seconds" wire-form comment on `event.rs` (the implementation has been nanos since v0.5.6) is also corrected.

### Fixed — `limpidctl check`: registry-declared default-block slots are scanned for `defaults { ... }` references

The parser-effects scan that resolves `defaults { ... }` references walked only the slots declared directly on the user's `def output` / `def input` block, missing slots declared by the module's registry. A config that wrote `defaults { tls { ... } }` on a sub-block whose schema came from the registry (the common shape for shared TLS / SASL defaults) passed `--check` silently while the runtime saw a missing block. The scan now consults the registry-declared slots so the diagnostic fires at `--check` time.

### Fixed — DSL parser: same-precedence binary operators fold left-associatively

`fold_by_precedence` used `position` to pick the split point, which produced a right-associative AST for chains like `1 - 2 - 3` or `10 / 2 / 5`. The eval path then computed `1 - (2 - 3) = 2` and `10 / (2 / 5) = 25` instead of the conventional `(1 - 2) - 3 = -4` and `(10 / 2) / 5 = 1`. The split point now uses `rposition` so the rightmost minimum-precedence operator becomes the divider and the left side folds first, matching the left-associative convention for arithmetic. The pre-existing eval test built its AST by hand, so the parser-side regression had been invisible; two new tests pin the AST shape and the end-to-end text-parse-eval round-trip.

### Internal — Module construction unified behind `BuildContext`

`Module::from_properties` previously had a per-module ad-hoc signature (some took the property set, some took a `&CompiledConfig`, some took both plus an `ErrorLogWriter`). The trait method now takes a single `BuildContext` carrying the compiled config, the optional error-log writer, and the function registry — so every module signs the same shape and adding new construction-time context to one module does not ripple through every other module's signature. No operator-visible change.

### Internal — Expanded unit and integration test coverage

Test coverage additions backfill paths that the 0.7.8 audit pass identified as under-exercised: `input tail` (rewind on incomplete line, state-file durability, inode helper); `input syslog_tcp` accept-loop end-to-end + `max_connections` wire enforcement; `input syslog_udp` basic event flow + PRI rejection + receiver-drop termination; parser tests for `cef` edge cases, `parse_kv`, and the `parse_json` wrapping contract; security tests for `file sanitize_path` backslash handling and `unix_socket` symlink refusal; `pipeline` + `event` render-error fallback and boundary round-trips; `queue` `write_with_retry` state machine + `disk enforce_max_size` + cursor durability; `tls` `build_server_config` + `build_client_config_sync` paths. No production code change.

### Fixed — `output otlp_http` / `output otlp_grpc`: retain drained batch on mid-stream flush failure

When `flush()` returned `Err` mid-stream, both OTLP transports dropped the events they had just drained from the in-memory buffer — the per-Event ResourceLogs bytes were popped to build the request, the request failed, and the bytes were discarded with only a `tracing::warn!`. The retry budget did not apply (the queue layer had already counted the events as delivered when `write()` returned `Ok` into the batch buffer), so the next flush started from a fresh buffer and the drained payload was silently lost. The drained batch is now restored to the in-memory buffer on `Err`; `events_failed` is **not** bumped (the events have not actually been rejected by the peer, only deferred), and the next flush picks them up alongside whatever has accumulated since. Aligns the mid-stream failure shape with the existing shutdown-flush behaviour: events stay in the buffer until either a successful flush or shutdown drains them — never dropped silently.

### Fixed — `output syslog_tcp` / `output syslog_udp`: peer cooldown anchored on failure-completion time

The peer-rotation cooldown timestamp on both syslog outputs was being captured *before* the connect / send attempt, the same shape `output http` already fixed in 0.7.8 (commit `e0484e9`) and `output otlp_grpc` / `output otlp_http` picked up via the sibling fix below. A peer that timed out mid-send recorded a cooldown that was already most of the `PEER_COOLDOWN` window in the past, so the immediately-following datagram / connection attempt reselected the same bad peer instead of rotating away. The cooldown timestamp now derives from a fresh `Instant::now()` captured on the failure branch after the failing call returns, matching the rest of the rotation-aware outputs and giving the cooldown window the wall-clock distance it needs to actually shift load to a healthy peer.

### Fixed — `output http`: stuck batch after a failed flush no longer waits for the next `write()`

When `flush()` returned `Err`, the batch was placed back into the in-memory buffer but the flush timer was not re-armed. The stuck batch then sat in the buffer until the next `write()` arrived — which on a quiet pipeline might be never — while the queue layer had already counted the events as failed (Rendered payloads do not retry). The operator saw `events_failed += 1` yet the data still lived in the HTTP buffer with no schedule to drain it. The flush timer is now re-armed on the `Err` branch so `batch_timeout` drives the retry, restoring the "no event silently parked in the HTTP buffer" invariant. Regression test covers the `should_flush` failure path.

### Fixed — `limpid-prometheus`: exporter scrape against a wedged daemon is bounded at 5 s

The Prometheus exporter previously used `std::os::unix::net::UnixStream` + blocking `BufRead::lines()` from inside async hyper handlers. A wedged limpid daemon — accepting the control-socket connection but never writing a reply — pinned a tokio worker thread until the daemon answered, with no upper bound; slow / stuck scrapes silently starved the exporter's runtime and Prometheus scrapes piled up on the broken peer. The control-socket query is now `tokio::net::UnixStream` + `AsyncBufReadExt` and the entire connect+write+read sequence is wrapped in `tokio::time::timeout(QUERY_TIMEOUT = 5 s, …)`. A scrape hitting the cap returns an error body and the next scrape gets a fresh attempt instead of waiting behind the old one. 5 s is well above local control-socket latency (typical < 1 ms) and well below Prometheus' usual `scrape_timeout` (10 s).

### Fixed — `input tail`: saved offset zero now resumes from 0, and send failure rewinds the cursor

Two silent data-loss bugs on the cursor-persistence side, fixed together. (1) `load_position().unwrap_or(0)` plus a follow-up `if offset == 0` collapsed "no state file" and "saved `Some(0)`" into the same path, sending the cursor to EOF and skipping every line appended between the save and the next start — the typical recovery shape after rotate/truncate. The path now keeps the `Option<u64>`: `Some(n)` resumes from `n` (including 0), `None` falls back to EOF. (2) `read_new_lines` advanced `current_offset` past each line before sending it downstream. If `tx.send()` failed (consumer gone) the loop broke out and `run()` persisted the already-advanced offset, silently dropping the un-sent line. The send-failure path now mirrors the incomplete-line rewind so the line is retried on the next poll.

### Fixed — `input journal`: blocking reader exits promptly on shutdown while idle

The journal input runs its libsystemd-backed reader inside `tokio::task::spawn_blocking`, and on shutdown the orchestrator called `journal_handle.abort()`. tokio's abort cannot cancel a `spawn_blocking` task that has already started executing, so the reader's only escape route was the next `tx.blocking_send()` returning `Err` — which requires a fresh journal entry to arrive. On a quiet host that may not happen for a long time, leaving the blocking thread (and its journald file handle) parked indefinitely past daemon shutdown. Shutdown is now signalled explicitly via an `Arc<AtomicBool>` the reader polls between iterations, and the per-poll sleep is replaced with `interruptible_sleep` that naps in 100 ms quanta re-checking the same flag. Shutdown latency is bounded by one quantum regardless of `poll_interval`.

### Fixed — `output http` / `otlp_http` / `otlp_grpc`: in-memory batch is flushed on shutdown

The three batched sinks return `Ok` from `write()` once the event lands in their in-memory buffer (so the memory queue counts it as delivered), and the flush either happens when `batch_size` is hit or when the per-output timer fires. On daemon shutdown those sinks' `Drop` impl aborted the timer and the process exited with the buffer contents still resident; the existing log line claiming events "will be re-delivered from queue" is not true for the memory queue. `Drop` cannot fix this because it is synchronous and the sink I/O is async. `Output` gains an async `shutdown()` hook with a default no-op, overridden on the three batched sinks to abort the timer and run one final flush. `run_queue_consumer` calls it once the consume loop exits, so both the shutdown-signal and queue-closed break paths fall through the same shutdown call.

### Fixed — `runtime`: queue-enqueue and pipeline eval-error failures now reach the DLQ

Two High pipeline-side error-path gaps closed together. (1) `run_pipeline_with_outputs` discarded the bool returned by `QueueSender::send`. On enqueue failure (memory-queue receiver dropped, disk serialise/write error, Rendered-on-Disk routing bug) the pipeline counted the event as `events_finished` even though it had reached neither the queue nor the secondary nor the error log — the event was effectively deleted in silence. The bool is now captured, failed output names collected, and termination overridden to `Errored` so the existing Errored-arm DLQ machinery catches it. The per-output `events_failed` metric is also bumped so operators see the failure on each output's dashboard regardless of the pipeline-level routing decision. (2) `process_event` matched `Err(e)` returned from `run_pipeline` with a log-only branch — no `events_errored` bump, no DLQ entry — breaking the docs' promise that runtime errors go to `events_errored` and the error log. Both paths now construct an `ErroredEventContext` and route through a shared `write_errored_to_dlq` helper.

### Fixed — `queue`: disk cursor commits on consumer ack, not on `recv()`

High-severity audit finding. The disk queue saved its read cursor inside `recv()` immediately after each event was returned, so from the queue's POV the event was consumed before the queue consumer even handed it off downstream. A crash between `recv` and the output write lost the event: on restart the persisted cursor sat past the un-shipped record and it was never replayed — defeating the "retry / restart re-delivers" contract the disk queue exists for. The queue now follows the standard durable-queue contract (Kafka/RabbitMQ shape): `recv()` only advances an in-memory cursor, and a new `ack()` hook persists progress + reclaims consumed segments. The queue consumer calls `ack()` after every event's disposition is decided — delivered, routed to secondary, or retries exhausted — so the on-disk cursor only moves once the event has reached a terminal state. Memory-queue's `ack()` is a no-op. This shifts the disk queue from at-most-once to at-least-once; downstream sinks that can't tolerate duplicates need idempotent ingestion.

### Fixed — `queue`: disk cursor uses per-event ack position (regression fix)

The `DiskQueueReceiver::ack()` method introduced in this cycle saved the receiver's current `read_seq` / `read_offset` instead of the position of the event whose handle was being acked. With batched outputs holding multiple `(Event, QueueAckHandle)` pairs in flight, a single event ack advanced the cursor past **all** buffered events; a crash before the remaining events flushed silently lost them — defeating the at-least-once guarantee the same cycle's "disk cursor commits on consumer ack" entry advertised. `QueueAckHandle` now carries its position, the receiver tracks an in-flight position queue, and the persisted cursor only advances through the contiguous acked prefix from the front. Memory queue is unaffected (no persistent cursor).

### Fixed — `queue`: retry-exhausted and unrecoverable payloads now flow to `error_log`

Output retry exhausted previously dropped the payload silently — the consumer ack'd, the event left the disk queue's replay window, and only a `tracing::warn!` plus an `events_failed` increment remained. Retry exhaustion now writes the payload as a JSONL record to the configured `control { error_log "..." }` — the same DLQ that pipeline / process eval errors flow into. Output-failed records ride the same writer with the `kind: "output"` discriminator (see the DLQ-v2 schema entry above) so operators reading the DLQ stream see them alongside pipeline failures and can replay each via `limpidctl inject output <name>`. When no `error_log` is configured the previous warn-only fallback is preserved (no regression).

### Fixed — `output http` / `otlp_http` / `otlp_grpc`: shutdown-flush failures drain to `error_log`

The batched outputs' final `shutdown()` flush retained the in-memory buffer on failure for "retry", but there is no next retry tick at process exit — so the retained buffer was equivalent to dropping the events. The shutdown trait signature now takes an optional `&Arc<ErrorLogWriter>`; when the final flush fails the helper walks the remaining buffer items and persists each as an `ErroredEventContext` with `process="(output <name> shutdown)"` (distinct from the retry-exhausted discriminator so operators can tell mid-stream from at-shutdown failures). When `error_log` is not configured the shutdown error propagates unchanged (0.7.7 parity); when the error_log writer itself fails the helper swallows the secondary error to avoid recursion and the original shutdown error still surfaces.

### Behavior changes (non-breaking)

#### Outputs: `retry { ... }` is now accepted on every output type

The runtime has always honoured `retry { ... }` on every output (the queue layer reads it uniformly via `RetryConfig::from_output_properties`), but the property schema only declared it on `output otlp_grpc` and `output otlp_http`. Writing `retry` on `kafka`, `file`, `http`, `stdout`, `syslog_tcp`, `syslog_udp`, or `unix_socket` failed `--check` with "unknown property", even though the documented `outputs/README.md` examples implied it was universally available. The schema was the gap, not the runtime and not the docs. `RETRY_PROPERTY_SPEC` is now lifted into `queue/mod.rs` and spliced into every output's schema; the prior OTLP-local `RETRY_BLOCK_PROPERTIES` (with `max_attempts` / `initial_wait` / `max_wait` / `backoff`) is preserved unchanged for the existing call sites. (The matching `secondary <name>` spec was also lifted mid-cycle but was subsequently removed outright — see the breaking-change entry at the top of this release.)

### Upgrading — additional configs that now fail-fast (0.7.8 cycle, second batch)

Two further config shapes are rejected at parse / startup time in 0.7.8. Each is individually described in the `Fixed —` entries above; this list is a single place for operators to scan before upgrading.

- **`switch` arms with `default` not last, or with more than one `default`.** The runtime walks arms in source order and `default` matches everything, so any arm after a `default` is unreachable and multiple defaults are ambiguous (only the first runs). Pre-0.7.8 `--check` was silent on this shape — configs that meant "case 6 → tcp, otherwise null" but accidentally put `default` first sent every event to the default branch with no diagnostic. Both shapes now fail `--check` as `DiagKind::Dataflow` errors. Remediation: move `default` to the last arm and remove duplicates.
- **Recovery-dependent outputs without a configured `control { error_log "..." }`** now emit a `--check` warning. The retry-exhaustion and shutdown-flush recovery paths added in this cycle only activate when `error_log` is configured; an operator who configures `retry { ... }` or any batched output (`http` / `otlp_http` / `otlp_grpc`) but forgets `error_log` gets the same 0.7.7 silent-drop behaviour. The new warning fires once per affected configuration. Under plain `--check` the warning is informational; under `--check --strict-warnings` (and `--ultra-strict`) it is promoted to a hard fail per the existing strict-warnings ladder.

(Two transient mid-cycle additions — schema-level rejection of unknown / cyclic / quoted-string `secondary <name>` references — are now moot since `secondary` itself was removed before release. See the breaking-change entry at the top of this release for the migration shape.)

### Internal — Unified switch / if-chain dispatch across pipeline and process contexts

The runtime executed switch and if-chain dispatch twice — once for pipeline context (`pipeline.rs` PipelineStatement::Switch / `exec_pipeline_if`) and once for process context (`dsl/exec.rs` ProcessStatement::Switch / `exec_if_chain_process`). Both walked arms / branches in source order with first-match semantics; the divergence was entirely in the surrounding execution context. The dispatch algorithm is now factored into two pure helpers in `dsl/eval.rs` (`select_switch_arm` and `select_if_branch`) that take an `eval_*` closure capturing the caller's context-specific state and return the matched body as a slice for the caller to execute. `exec_pipeline_if` and `exec_if_chain_process` are eliminated outright; the per-context dispatch shrinks to a 4-line match. The free `is_truthy` wrapper in `eval.rs` is also removed; the canonical `Value::is_truthy` impl stands alone. No user-visible behaviour change.

### Internal — Queue I/O boundary functions return typed outcomes instead of `bool`

`QueueSender::send`, `DiskQueueSender::send`, and `write_with_retry` previously returned `bool`, where `false` could mean any of several distinct things (queue closed, disk write failed, serialization failed, retry exhausted) and callers had no type-level signal that the value mattered. Two new outcome types in `queue/outcome.rs` replace the booleans: `QueueSendError` (an enum of the failure modes) and `WriteDisposition` (`Delivered` / `Dropped` / `DroppedToRecovery`, the third for the `error_log` routing path). Both are `#[non_exhaustive]` and `WriteDisposition` is `#[must_use]`, so call sites are forced to handle the disposition and future variants will surface as compiler errors. No operator-visible behaviour change at the time of this refactor — it is the type-level foundation the subsequent recovery-routing fixes build on.

### Fixed — `output otlp_grpc` / `output otlp_http`: route `partial_success.rejected_log_records` to `events_failed`

When the OTLP receiver returned 2xx-equivalent with `partial_success.rejected_log_records > 0`, both transports counted the entire batch as `events_written`, hiding server-side data loss from operator dashboards. `otlp_grpc` parsed the response and logged a warning but did not split the metric; `otlp_http` did not parse the response body at all. The OTLP transport-success path now splits the batch's events between `events_written` (accepted) and `events_failed` (rejected) using the receiver's `partial_success.rejected_log_records`. `otlp_http` learned to decode the response body in both protobuf and JSON forms — peers returning empty bodies or undecodable bodies are still treated as fully accepted (the lenient default). Selective re-send of *only* the rejected records remains queued for a later release, as documented in the existing `send_once` doc comments; this change is purely metrics accuracy.

### Fixed — `output otlp_grpc` / `output otlp_http`: stop silently dropping distinct `schema_url`s when merging by Resource / Scope

`merge_by_resource` (and the inner Scope-level pass in `merge_by_scope`) keyed merges only on Resource (or Resource + InstrumentationScope) equality. Two entries sharing a Resource but declaring *different non-empty* `schema_url`s — semantically: "the same resource described under two different schemas" — collapsed into a single bucket and the second `schema_url` was dropped on the floor. Per OTLP semantics they should remain distinct. The merge key now also requires `schema_url` compatibility (equal, or at least one side empty), so different non-empty `schema_url`s keep their own bucket. The existing "promote empty acc → take incoming schema_url" behaviour is preserved (and now regression-guarded).

### Upgrading — configs that now fail-fast (action required if matched)

0.7.8 turns three previously-tolerated misconfigurations into hard parse-time errors. If a 0.7.7 config matches any of the patterns below, the daemon will refuse to start on 0.7.8 and limpidctl check will reject it:

- **`output kafka` with `mechanism plain` and no `tls { ... }` block.** SASL/PLAIN sends credentials in clear text, so 0.7.8 requires a TLS transport. Remediation: add a `tls { ... }` block (CA only is fine for a server-cert-validated peer) or switch to `mechanism scram_sha_256` / `scram_sha_512`, which use challenge-response and never put the password on the wire.
- **`output otlp_http` with a `tls { ... }` block on an `http://` endpoint** (and now `output otlp_grpc` too, added in the 0.7.8 sibling-regression follow-up). reqwest and tonic only engage TLS when the URI scheme is `https`, so the previous behaviour silently dropped the TLS block and shipped in clear text. Remediation: change the endpoint to `https://...`, or drop the `tls { ... }` block if plaintext is intended.
- **`output http` with a `method` other than POST / PUT / an extension token reqwest accepts.** 0.7.7 silently downgraded unknown methods to POST; 0.7.8 fails fast at parse time. Remediation: spell the method correctly. The set of accepted methods matches reqwest's `Method::from_bytes` — uppercase, ASCII.

No CHANGELOG entry intentionally hides any of these; they are individually called out in the `Fixed —` entries below. This summary just gives operators upgrading from 0.7.7 a single place to scan before the upgrade.

### Internal — End-to-end timeout-firing tests for the 0.7.8 export and TLS-handshake timeouts

The three timeout constants introduced in 0.7.8 — `GRPC_REQUEST_TIMEOUT` / `HTTP_REQUEST_TIMEOUT` (30 s, on the OTLP sinks) and `TLS_HANDSHAKE_TIMEOUT` (10 s, on `input syslog_tcp`) — previously had bound-check assertions only. A regression that removed the `tokio::time::timeout(…)` wrap, or pointed it at a much larger duration, would not have been caught by a constant-value check. Three new paused-time tests (`export_timeout_fires_against_stalled_peer` in each of `output/otlp/grpc.rs` and `output/otlp/http.rs`, plus `tls_handshake_timeout_fires_against_stalled_client` in `input/syslog_tcp.rs`) exercise the actual firing path against a stalled TCP peer / client. Each uses `tokio::time::advance` past the documented timeout and asserts the call surfaces a timeout-flavoured error rather than hanging. `tokio`'s `test-util` feature is added to `[dev-dependencies]` to enable virtual time control. No production code change.

### Fixed — `output http` / `otlp_http` render a placeholder when error bodies are gzip/brotli/deflate encoded

limpid's `reqwest` build excludes the `gzip` / `brotli` / `deflate` decompression features, so when a peer (or upstream proxy) returns an error response with `Content-Encoding: gzip` the still-compressed bytes were running through `from_utf8_lossy` and ending up as replacement-char soup in the daemon log. The shared `error_snippet` helper in `modules/output/http_util.rs` now inspects `Content-Encoding` and substitutes `<gzip-encoded body, N bytes>` (or whatever the advertised encoding is) when it's not `identity`. The byte count is retained so an operator can still see the peer is returning *something*. `identity`, missing header, and the existing 4 KiB cap path all keep their previous behaviour.

### Fixed — `output syslog_udp` walks every resolved address on connect, restoring DNS-level failover

The 0.7.8 family-aware bind rewrite kept v6-only destinations working but regressed DNS failover: `lookup_host(host:port).next()` committed to the first resolved `SocketAddr` and gave up if that one didn't connect. Pre-0.7.8 `socket.connect(host:port)` walked the whole resolution list internally and succeeded on the first reachable address — common during a partial v6 outage or a stale AAAA record on a dual-stack host. The connect path now iterates every resolved `SocketAddr`, binding a fresh ephemeral socket of the matching family per attempt and breaking on first success. On exhaustion the most recent error is returned with both the original hostname and the specific address that failed, so an operator can see which records were tried.

### Fixed — `output kafka` reports PLAIN-without-TLS before reading the password file

A misconfiguration with both a broken `password_file` path and `mechanism plain` without a `tls { ... }` block surfaced the file-read error first, masking the more important credentials-on-the-wire problem. The operator would fix the file path, get the daemon to start, and only then discover their PLAIN config was unsafe. `kafka.rs` now does a cheap pre-check on the mechanism ident before `parse_sasl_block` touches the filesystem, so the PLAIN-without-TLS diagnostic always fires first. The post-parse `require_tls_for_plain` guard stays as a belt-and-braces check, and the new pre-check explicitly avoids leaking the password-file path into the error wording. Two new tests cover both branches.

### Internal — `limpidctl check`: OneOf schema edge cases documented + multi-block guard

Two follow-ups on the OneOf branch-picking logic that landed in 0.7.8. (1) `check_one_of` now documents — with a regression test — the deliberate fallback to `OneOfMismatch` when 0 or 2+ variants structurally match. Two-scalar-variant OneOf given a wrong-type literal (e.g. `OneOf[String, Int]` with a Bool) keeps the "expected String | Int, got Bool" wording, which is more useful than picking one variant's TypeMismatch and hiding that the other shape was also allowed. (2) `inner_block_schema_of` in `check/outputs.rs` previously returned the first block-shaped OneOf variant via `find_map`. Today only `OneOf[Block(TLS_CLIENT_BLOCK_PROPERTIES), String]` exists, so "first block wins" is unambiguous — but a future `OneOf[Block(A), Block(B)]` (e.g. inline tls vs inline mTLS configs) would silently validate against the wrong schema. The function now returns `None` when more than one block-shaped variant exists, falling back to expression-level checks until a per-OneOf resolution rule is encoded explicitly. No user-visible behaviour change today.

### Fixed — `sum()` decides accumulator type from the whole array, not the first Float

The 0.7.8 i64-overflow fix tripped on `[i64::MAX, 1, 0.5]`: the second integer overflowed the i64 accumulator before the third element (a Float) had a chance to promote the result. The eventual return type was clearly going to be `Float`, but the operator got a hard error instead of the float total they were summing toward. `sum()` now pre-scans the array for any Float and picks the accumulator type up front — Int-only arrays still use a checked `i64` accumulator (overflow surfaces a typed error with a remediation hint suggesting `* 1.0` promotion); any-Float arrays use a single `f64` accumulator and follow IEEE 754 semantics (overflow saturates to ±Infinity, NaN propagates). The expression-functions doc note is corrected at the same time — the prior `map(...) { |x| x as f64 }` suggestion referenced an `as` cast operator the limpid DSL does not implement; the working idiom is `map(...) { |x| x * 1.0 }`. Five new tests cover the boundary (mixed int+float past i64::MAX, float-only, float overflow → +Inf, NaN propagation, and the remediation hint in the overflow error).

### Fixed — `output otlp_http` now warns loudly when `verify false` is paired with an https endpoint

`output http` already emits a one-line, greppable `tracing::warn!` when `verify false` is paired with an https URL, so operators can audit the daemon log for MITM-vulnerable peers. `output otlp_http` exposes the identical `verify` knob but had no such warning — `verify false` toggled `danger_accept_invalid_certs(true)` silently, so the same security-relevant misconfiguration was visible in one output and invisible in the other. The warn now fires once per https peer at startup with the same wording as the `output http` message.

### Fixed — `output otlp_grpc` rejects `tls { ... }` on plaintext `http://` endpoints

Same trap `output otlp_http` already closes for itself in 0.7.8: tonic only engages the TLS layer when the URI scheme is `https`, so a `peer { endpoint "http://otel:4317"; tls { ca ...; cert ...; key ... } }` configuration silently dropped the entire TLS block and shipped gRPC in clear text — exactly the misconfiguration an operator who took the trouble to write a `tls` block was trying to avoid. The mismatch is now rejected at parse time with the same error wording as the `otlp_http` guard: switch the endpoint to `https://` or drop the `tls` block.

### Fixed — `output otlp_grpc` and `output otlp_http` now bound peer cooldown from failure time, not request start

The peer-rotation cooldown timer was being measured from a pre-request `Instant::now()`, the same bug `output http` already fixed in 0.7.8 (commit `e0484e9`) and which was not propagated to the OTLP sinks. With the newly-introduced 30 s export timeout and a 5 s `PEER_COOLDOWN`, a peer that timed out wrote a cooldown that was already 25 s in the past, so the immediately-following batch reselected the same bad peer instead of rotating away. The cooldown timestamp now derives from a fresh `Instant::now()` captured on the failure branch in both `otlp/grpc.rs` and `otlp/http.rs`, matching the `output http` fix and giving the rotation budget the wall-clock distance it needs to actually shift load to a healthy peer.

### Fixed — `output otlp_http` no longer buffers unbounded error bodies into memory

The peer-failure diagnostic path used `resp.text().await` and then trimmed the resulting `String` to 500 chars. Because `text()` buffers the entire response body before returning, a peer (or upstream proxy) emitting a multi-MB error body forced the daemon to allocate / decode the full payload on every failure — an availability footgun the matching fix in `output http` already closed. `output otlp_http` now reads via the shared `read_body_capped` helper with the same 4 KiB cap, so the cost of a failing peer is bounded regardless of how chatty its error responses are.

### Internal — `read_body_capped` extracted to a shared helper

`output http` and the soon-to-be-aligned `output otlp_http` both need to bound how many bytes of an error response body they read into memory, so the helper moved from `modules/output/http.rs` to a new `modules/output/http_util.rs` module. No behaviour change for `output http`. The lingering misleading comment that claimed the connection "returns to the pool" after a mid-chunk break is also corrected: reqwest/hyper closes the underlying TCP connection when the `Response` is dropped without reaching EOF, and that's an accepted trade-off (bounded memory matters more on a failing peer than reusing its connection).

### Fixed — Docs: fenced code blocks now tagged for markdownlint MD040 compliance

`docs/src/{dsl-syntax,functions/expression-functions,processing/user-defined,inputs/syslog-tcp,outputs/syslog-udp}.md` had unannotated fenced code blocks. mdbook-style consumers tolerate this, but markdownlint MD040 flags them and standard syntax-highlighting falls back to "no language". All 93 bare fences across these 5 files are now tagged `limpid` (the contents are uniformly limpid DSL — `def input/output/process { … }`, `workspace.x = …`, expression-function call sites). The accompanying `tls.rs` doc comment on `TLS_CLIENT_BLOCK_PROPERTIES` is also corrected: it claimed "empty `tls {}` block is rejected by callers", but the actual contract is module-specific (`output otlp_http` rejects on plaintext endpoints; other callers accept empty blocks as "use system CA roots"). Doc-only — no code path touched.

### Fixed — `sum()` now reports i64 overflow as a typed error

The integer accumulator used unchecked `+=` and depended on the build profile for overflow behaviour: debug builds panicked, release builds wrapped silently and produced bogus (often negative) totals for large arrays. The accumulator now uses `checked_add` and surfaces a typed error `sum() overflowed i64 (accumulator …, element …)` regardless of build mode, catching the bug in tests / `--check` instead of production. Nine new unit tests cover the function (no inline tests existed before): integer / mixed-numeric / empty-array happy paths, type-error rejections (non-array input, null input, non-numeric element), and the overflow boundaries at `i64::MAX` + 1 and `i64::MIN` − 1.

### Fixed — `limpidctl check`: nested-block expression diagnostics + OneOf branch-specific errors

Two diagnostic-quality fixes from the PR #9 (release 0.7.4) review:

- **Expression-level diagnostics inside nested output blocks no longer silenced.** A typo like `peer { host "${upperr(workspace.msg)}" }` used to skip `expr_types::check_types` for `host` — the analyzer inherited the parent block's `schema_owned=true` flag through every recursion level and silenced every inner key, masking unknown functions, type mismatches, and similar expression errors inside any schema-declared nested block. The skip is now narrowed to the only case it actually targets — a bare top-level `ExprKind::Ident` value like `framing non_transparent` (= an enum-shaped value the schema validator owns) — so template interpolations inside nested output properties get checked again.
- **`OneOf` schema mismatches now surface the specific inner error when exactly one variant matched structurally.** Previously, when no variant matched cleanly, every failure collapsed to `OneOfMismatch` ("expected Block | Ident, got Block") — actively misleading when the user wrote the right outer shape and the real problem was one missing inner key. If exactly one variant matches the outer shape (no `ExpectedBlock` / `ExpectedValue` failure), the analyzer now surfaces that variant's specific inner error (e.g. `MissingRequired` for the missing `cert`). When zero or multiple variants structurally match, the generic `OneOfMismatch` still fires so the operator sees the full variant list.

### Fixed — `output syslog_tcp` / `output syslog_udp`: IPv6 + parse-path correctness

Three fixes from the PR #9 (release 0.7.4) review that surfaced once this PR audit ran end-to-end on the current codebase:

- **`Peer::address` now brackets IPv6 literals.** A peer configured with `host "::1"` previously produced the address string `::1:514`, which Rust's `SocketAddr` parser rejects (it reads the trailing `:514` as part of the address). Both TCP `TcpStream::connect` and UDP `UdpSocket::connect` hit this. The formatted address now reads `[::1]:514`; IPv4 and hostnames are left unbracketed; an already-bracketed literal is preserved.
- **`output syslog_tcp` / `output syslog_udp` reject `peer` + `peers` in `from_properties` too.** The schema-validating `Module::build` path already caught this, but `from_properties` (called directly from snippet expansion and inline test fixtures) silently took the first `peer` block and discarded the `peers` block. The exclusivity contract is now enforced on every entry point.
- **`output syslog_udp` no longer forces an IPv4 ephemeral socket.** The previous hard `UdpSocket::bind("0.0.0.0:0")` meant any peer that resolved only to AAAA failed before the first datagram left. The output now resolves the peer first, picks `0.0.0.0:0` or `[::]:0` to match the resolved address family, then connects.

### Fixed — `input syslog_tcp`: TLS handshakes are now bounded at 10 s

A client that opened TCP but never completed the TLS handshake would otherwise pin a task on `acceptor.accept().await` forever and consume one of the `max_connections` slots. With enough stalled handshakes an attacker (or a misbehaving client) could exhaust the slot pool and deny service to legitimate peers. Handshakes now have a hard 10 s ceiling; on timeout the connection is dropped with a `WARN` log naming the peer address and the timeout duration.

### Fixed — `output http`: four correctness fixes from the 0.7.6 review

- **`verify false` no longer drops the client identity.** A `tls { cert key }` block on a peer used to be discarded entirely when `verify false` was set on the output, so mTLS silently broke whenever the operator disabled server-cert validation. The client identity is now preserved regardless of `verify`; only the `tls.ca` portion is ignored (with a warning) under `verify false`.
- **Peer cooldown now measured from the failure time.** With the new 30 s per-request timeout and the 5 s peer-cooldown window, capturing `now` *before* the request meant a timed-out failure could record an already-expired cooldown and immediately reselect the bad peer. `Instant::now()` is now read after the call returns.
- **Method honored end-to-end.** Methods other than `POST` and `PUT` used to silently degrade to `POST`. The configured method is now parsed into `reqwest::Method` at config-load time (invalid verbs fail fast with a clear error) and sent verbatim via `client.request(method, url)` — `PATCH`, `DELETE`, `MKCOL`, RFC-compliant extension tokens all reach the peer as intended.
- **Error response body capped at 4 KiB.** A malicious or misconfigured peer used to be able to return an unbounded error body, which `response.text().await` would buffer in full before the caller trimmed it. The new `read_body_capped` helper stops reading at 4 KiB via `Response::chunk()` so the failure diagnostic stays bounded regardless of peer behaviour.

### Fixed — `output otlp_grpc` / `output otlp_http` / `output http`: Owned events no longer get silently merged into a batch

Disk-queue replay and control-socket inject events (`SinkInput::Owned`) need a per-event ship verdict from the output module — `Ok` ⇒ drop from the queue, `Err` ⇒ retry / disk-replay / secondary. The batched outputs previously routed Owned events through the same buffer as the memory hot path and returned `Ok` after only enqueueing the event, so the caller never saw a per-event verdict. If the eventual flush failed the buffered events were silently lost (the queue had already dropped them).

The three batched outputs now override `write_owned` to ship a single event inline, bypassing the batch, so the caller's queue retry / disk replay semantics work as designed. The memory hot path (Rendered) continues to batch as before.

### Fixed — `output otlp_grpc`: per-export 30s timeout

`client.export(request)` is now wrapped in `tokio::time::timeout(30s)`. A collector that accepted the connection but never returned a HEADERS frame would previously hold the flush future open indefinitely, blocking rotation and starving retry. Matches the existing per-call timeouts used elsewhere (syslog input, etc.).

### Fixed — `output otlp_http`: per-export 30s timeout + reject `tls { ... }` on plaintext endpoints

Two related corrections:

- The reqwest client now carries a 30s `timeout(...)` so a peer that accepts the connection but never replies counts as a failure and yields to the next peer in the rotation. Without this, a stalled collector blocked flush indefinitely.
- A `tls { ... }` block paired with an `http://` endpoint is rejected at config-load time. reqwest only negotiates TLS on `https://` URLs, so the previous behaviour silently shipped in clear text while pretending the tls block was active.

### Fixed — `output kafka`: reject `mechanism plain` without a `tls { ... }` block

SASL/PLAIN puts the username and password in clear text on the wire — the only safe transport for that mechanism is TLS. Previously `mechanism plain` paired with an absent `tls` block selected librdkafka's `sasl_plaintext`, sending credentials to the broker in clear text. limpid now refuses this combination at config-load time and the daemon will not start until either a `tls { ... }` block is added or the mechanism is switched to `scram_sha_256` / `scram_sha_512` (SCRAM uses challenge-response and never puts the password on the wire).

### Fixed — `output kafka`: SASL `password_file` handles CRLF / bare CR

Trailing-newline stripping now matches `\r\n` and bare `\r` in addition to bare `\n`, so password files written on Windows hosts (or with an editor that defaults to CRLF) authenticate correctly. Previously a CRLF-terminated file left a `\r` on the password and produced a `bad credentials`–shaped failure that looked like an operator typo.

## [0.7.7] - 2026-06-22

### Fixed — `cef.parse` now emits the raw extension blob as `ext`

`cef.parse` previously split the CEF Extension section into individual `key=value` siblings of the header keys (`src` / `dst` / `act` / …) and discarded the raw blob. There was no way to recover the original extension string — needed for passthrough / re-emission, debugging the splitter, and dialect-specific extension content the splitter doesn't decode (escape sequences, custom separators).

The function now emits **both** forms: the split per-key form (the documented authoring surface, unchanged) **and** the raw blob as `workspace.cef.ext` (the new field). The raw form is omitted when the Extension section is empty, mirroring `syslog.parse`'s treatment of empty `msg`. `cef.parse` also gained the unit-test coverage that was missing before — eight tests pin the header parse, extension split, raw-`ext` emission, empty-extension omission, non-numeric severity fallback, value-with-spaces splitter behaviour, and the two error paths.

## [0.7.6] - 2026-06-21

> syslog TLS folded into `syslog_tcp` on both sides (output: per-peer, input: optional block); `otlp_http` gains TLS / mTLS; `output kafka` gains TLS / mTLS / SASL; `output otlp` split into `otlp_http` / `otlp_grpc` and both gain per-peer rotation + mTLS; `output http` gains per-peer rotation + mTLS

### Added — `output http` per-peer rotation + mTLS

`output http` now accepts a `peer { url tls{...} }` (single destination shorthand) or `peers { peer { url tls{...} } ... }` (multi-destination) block in place of the previous top-level `url`. On each send the rotation picks the next available peer (cooldown expired) and tries it; a peer that fails the request is marked cooled-down for the shared 5-second window and skipped on subsequent sends until the cooldown expires. When every peer is currently cooled the rotation falls back to the cursor start — the queue layer's per-event retry then handles longer-term re-delivery (consistent with the existing `output http` retry semantics, which never had an internal retry loop).

Per-peer `tls { ca cert key }` enables mTLS. `cert` and `key` are paired (both-or-neither, enforced at parse time by `ClientTlsConfig::validate`). PEM files for the cert and key are loaded once at startup; chmod 600 the key, the daemon already refuses to run as root.

This is a **breaking change** for any existing `output http` config that used a single top-level `url`:

```text
# before
def output es {
    type http
    url "https://es:9200/_bulk"
    tls { ca "/etc/limpid/ca.crt" }
}

# after (single peer — shorthand mirrors output syslog_tcp / otlp_http)
def output es {
    type http
    peer {
        url "https://es:9200/_bulk"
        tls { ca "/etc/limpid/ca.crt" }
    }
}

# after (round-robin across multiple endpoints)
def output es {
    type http
    peers {
        peer { url "https://es01.example.com:9200/_bulk"; tls { ca "/etc/limpid/ca.crt" } }
        peer { url "https://es02.example.com:9200/_bulk"; tls { ca "/etc/limpid/ca.crt" } }
    }
}
```

`verify` stays top-level — disabling certificate validation is an output-wide debug switch, not a per-peer one. `method`, `content_type`, `compress`, `headers`, `batch_size`, `batch_timeout` also remain top-level (they apply across all peers).

### Added — `output otlp_http` / `output otlp_grpc` per-peer rotation + mTLS

Both OTLP output transports now accept a `peers { peer { endpoint tls{...} } ... }` block in place of the previous top-level `endpoint`. On each flush the rotation tries peers in round-robin order; a peer that fails the request is cooled-down for the standard 5-second window (shared with the syslog outputs) and skipped on subsequent flushes until the cooldown expires. Inside one flush the `retry { … }` budget still governs total attempts, but the rotation transparently picks the next available peer for each retry.

Per-peer `tls { ca cert key }` enables mTLS. `cert` and `key` are paired (both-or-neither, enforced at parse time); `ca` alone adds a custom CA on top of the system root store. PEM files for the cert and key are loaded once at startup; chmod 600 the key, the daemon already refuses to run as root.

This is a **breaking change** for any existing `output otlp_http` or `output otlp_grpc` config that used a single top-level `endpoint`:

```text
# before
def output o {
    type otlp_http
    endpoint "https://collector.example.com:4318/v1/logs"
    tls { ca "/etc/limpid/ca.crt" }
}

# after
def output o {
    type otlp_http
    peers {
        peer {
            endpoint "https://collector.example.com:4318/v1/logs"
            tls { ca "/etc/limpid/ca.crt" }
        }
    }
}
```

The shared `crate::tls::TLS_CLIENT_BLOCK_PROPERTIES` schema was extended from `ca`-only to `ca` / `cert` / `key` (all optional, with the paired invariant enforced by `ClientTlsConfig::validate`). `output syslog_tcp` (per-peer) and `output kafka` were both already carrying their own ca/cert/key block constants and have been migrated to the shared schema — no user-visible config change for those two, but the duplicated `PropertySpec` definitions are gone.

### Changed — `output otlp` split into `output otlp_http` and `output otlp_grpc` (breaking)

The single `output otlp { protocol grpc | http_* }` module is replaced by two independent modules — one per transport. The DSL no longer has a `protocol` switch that flips request-shape, header semantics, and endpoint conventions inside the same module.

Migration:

```text
# before (0.7.5)                 # after (0.7.6+)
def output o {                   def output o {
    type otlp                        type otlp_http        # or otlp_grpc
    protocol "http_protobuf"         protocol "http_protobuf"   # otlp_http only;
    endpoint "..."                   endpoint "..."             # otlp_grpc has no `protocol`
    ...                              ...
}                                }
```

Old configs (`type otlp` + `protocol grpc | http_*`) are rejected at startup. Wire-level behaviour is unchanged — the existing `ExportLogsServiceRequest` encoding, retry semantics, `batch_level` merging, headers / metadata handling, and TLS surface all carry over byte-for-byte. Only the DSL surface and module registration changed: the shared bits live under `crates/limpid/src/modules/output/otlp/` (internal helpers), and the public modules are `output/otlp/http.rs` (`OtlpHttpOutput`, `type otlp_http`) and `output/otlp/grpc.rs` (`OtlpGrpcOutput`, `type otlp_grpc`), mirroring the input side which has shipped split modules since 0.7.0.

Why split rather than keep one knob: every `protocol`-conditional property — `headers` (HTTP) vs gRPC metadata, `verify false` (HTTP only — tonic refuses), endpoint path conventions, compression sets, peer round-robin semantics (the future addition) — turned into a `protocol`-dependent check at parse time and a footnote in docs. Splitting collapses each module's surface to what its transport actually supports.

### Added — `output kafka` `tls { ... }` and `sasl { ... }` blocks

`output kafka` now accepts optional `tls { ca cert key }` and `sasl { mechanism username password_file }` blocks. The `security.protocol` is derived from which blocks are present (`plaintext` / `ssl` / `sasl_plaintext` / `sasl_ssl`), so the most common production setup (SASL/SCRAM over TLS) is a single config change away.

`cert + key` in the `tls` block are both-or-neither: present them together for mTLS, omit both for one-way TLS. `ca` alone is fine for private-CA broker certs.

Supported SASL mechanisms: `plain`, `scram_sha_256`, `scram_sha_512`. The DSL ident grammar forbids `-`, so the SCRAM mechanisms are spelled with underscores in the config and mapped to librdkafka's hyphen spelling (`SCRAM-SHA-256` / `SCRAM-SHA-512`) at parse time.

SASL credentials are split intentionally: `username` is inline (not secret), `password_file` points to a separate file (chmod 600) — the same disposition limpid uses for TLS private keys. Inline `password` is **not** supported, so credentials never end up in config diffs, backups, or pretty-printed log output. Empty `password_file` is rejected as a misconfiguration.

`brokers` is still a single comma-separated bootstrap list — librdkafka handles broker discovery / partition routing / leader failover internally, so unlike the syslog / http / otlp outputs there is no per-peer rotation layer to add here.

```limpid
def output secure {
    type kafka
    brokers "kafka1.example.com:9094"
    topic "syslog-events"
    tls { ca "/etc/limpid/kafka-ca.pem" }
    sasl {
        mechanism scram_sha_512
        username "limpid-producer"
        password_file "/etc/limpid/kafka.pw"
    }
}
```

### Added — `input otlp_http` optional `tls { ... }` block (HTTPS + mTLS)

`input otlp_http` now accepts the same `tls { cert key ca }` block that `input syslog_tcp` and `input otlp_grpc` already use. With the block present, the listener accepts HTTPS only (no HTTP fallback on the same port). `ca` enables mTLS — clients without a valid certificate signed by the configured CA are rejected at handshake.

The OTLP/HTTP default port (4318) is unchanged regardless of the block; there is no separate "secure" port in the OTLP spec.

```limpid
def input otlp_in {
    type otlp_http
    tls {
        cert "/etc/limpid/cert.pem"
        key  "/etc/limpid/key.pem"
        ca   "/etc/limpid/client-ca.pem"   # optional; enables mTLS
    }
}
```

Internals: `otlp_http` now drives the axum `Router` through the `axum-server` crate (the bundled `axum::serve` is hardcoded to plaintext `TcpListener` in 0.7), giving the same HTTP/1+2 + graceful shutdown shape on both transports.

### Changed (BREAKING) — `output syslog_tls` removed, TLS is now per-peer on `syslog_tcp`

The standalone `output syslog_tls` module that shipped in 0.7.4 is removed. The `output syslog_tcp` module now accepts a per-peer `tls` block (inline or named-profile reference); peers without `tls` use plaintext on the same output. A single relay can therefore fan out to a mix of TLS-encrypted and plain destinations.

Default port is per-peer: 6514 (RFC 5425) when `tls` is set on that peer, 514 (RFC 6587) otherwise.

Migration — rename `type syslog_tls` to `type syslog_tcp`. The existing top-level `tls { profile { ca cert key } }` map and the per-peer `tls { ... }` / `tls <profile_name>` forms work as-is:

```diff
def output secure {
-    type syslog_tls
+    type syslog_tcp
    framing octet_counting
    tls {
        corporate_ca { ca "/etc/limpid/corp-ca.pem" }
    }
    peers {
        peer { host "a.example.com" tls corporate_ca }
        peer { host "b.example.com" tls corporate_ca }
    }
}
```

### Changed (BREAKING) — `input syslog_tls` removed, TLS is now an optional block on `input syslog_tcp`

The standalone `input syslog_tls` module is removed; `input syslog_tcp` now accepts an optional `tls { cert key ca }` block. mTLS (client cert verification) is enabled by setting `ca` in the block — exactly the same shape as `input otlp_grpc`, which has worked this way since 0.7.0.

Default bind port flips with the block: **6514** (RFC 5425) when `tls` is configured, **514** (RFC 6587) otherwise.

Migration — rename `type syslog_tls` to `type syslog_tcp`. The existing `tls { ... }` block works as-is:

```diff
def input secure {
-    type syslog_tls
+    type syslog_tcp
    tls {
        cert "/etc/limpid/certs/server.crt"
        key  "/etc/limpid/certs/server.key"
        ca   "/etc/limpid/certs/client-ca.crt"   # mTLS
    }
}
```

A latent rustls panic (`CryptoProvider not installed`) that triggered when running `input syslog_tls` alone is fixed as a side effect — the new `syslog_tcp` code calls `install_default_crypto_provider()` before the rustls server config is built.

## [0.7.5] - 2026-06-07
>
> array primitives and expression chaining

### Added — block-argument array primitives

Arrays can now be transformed with expression-level block arguments: `map(array) { |x| ... }`, `filter(array) { |x| ... }`, `find(array) { |x| ... }`, and `reduce(array, init) { |acc, x| ... }`. The block body follows the same expression-function shape as `def function`: optional `let` bindings followed by a required return expression. Block locals are scoped to the block evaluation and do not leak into event workspace.

### Added — expression pipe operator

The `|>` operator chains expression-shaped transforms by inserting the left-hand value as the first argument to the function on the right. For example, `events |> filter { |e| e.kind == "auth" } |> map { |e| e.user }` is parse-time sugar for nested ordinary function calls; no runtime pipe object is introduced.

### Added — array helper primitives

New collection helpers cover common whole-array operations: `first`, `last`, `concat`, `distinct`, `sum`, `max`, `min`, `entitle`, `path`, and `is_array`. Existing `append`, `prepend`, and `len` remain.

### Changed (BREAKING) — remove `find_by` and statement-form `foreach`

`find_by(array, key, value)` is removed in favour of `find(array) { |x| x.key == value }`, which supports arbitrary predicates. Statement-form `foreach` and the magic `workspace._item` binding are removed; use `map`, `filter`, `find`, or `reduce` instead.

## [0.7.4] - 2026-06-03
>
> multi-destination syslog outputs + TLS

### Added — `syslog_tls` output

A new output module sends syslog over TLS-encrypted TCP. Default port is 6514 (RFC 5425). Supports server verification against a custom CA or the Mozilla root store, and optional mutual TLS via a client certificate. Named TLS profiles can be defined at the output level and referenced from individual peers; per-peer inline TLS blocks are also supported.

### Added — multi-destination peer lists with round-robin failover

The `syslog_tcp`, `syslog_udp`, and new `syslog_tls` outputs now accept a `peers { peer { ... } ... }` block in addition to the single `peer { ... }` form. Events are distributed across peers in round-robin order. A peer that returns a send, connect, or (for TLS) handshake error is taken out of rotation for a 5-second cooldown; the existing queue layer handles retry when every peer is unavailable.

### Changed (BREAKING) — output module rename: tcp/udp → syslog_tcp/syslog_udp

The `output` modules previously named `tcp` and `udp` are renamed to `syslog_tcp` and `syslog_udp`, matching the input-side naming. Both modules have always implemented RFC 6587 syslog framing, so the new names are honest about their scope. No alias is retained.

Configs that used `type tcp` or `type udp` in `def output { ... }` must be updated:

```diff
-    type tcp
+    type syslog_tcp

-    type udp
+    type syslog_udp
```

### Changed (BREAKING) — DSL: `address` / `host`+`port` replaced by `peer { ... }`

The top-level `address "host:port"` (and `host` + `port`) properties on `syslog_tcp` and `syslog_udp` are removed. Configs must use the new `peer { host port }` form (single destination) or `peers { peer { ... } ... }` (multiple). Mixed-form configs are rejected by the schema validator.

```diff
-    type syslog_tcp
-    address "10.0.0.1:514"
+    type syslog_tcp
+    peer { host "10.0.0.1" port 514 }
```

## [0.7.3] - 2026-05-17
>
> property-schema parity — `--check` and runtime now read the same surface

### Fixed — `--check` OK / runtime fail asymmetry on every `def input` / `def output`

0.7.2's declarative property schema was applied at two points: the analyzer (`--check`) and the runtime (`ModuleRegistry::create_input` / `create_output`). The analyzer stripped the structural `type` key before validating against the Module's schema; the runtime did not. The result was that every config with a `type tcp` (or any other type) line passed `--check` cleanly but was rejected by the daemon at startup with:

```text
output 'forwarder' (type 'tcp') has invalid configuration:
  - unknown property 'type' — aborting startup
```

The fix is structural. A new `ModuleProperties` type extracts `type` into a typed slot at parse time, and the Module trait's `from_properties` / `ModuleRegistry::create_*` factory closures both consume only `properties.user_properties()` — there is no `Vec<Property>` view that still contains `type` for anyone to forget to strip. The bug class is impossible to re-introduce without changing the type signatures.

`property_schema()`'s contract is unchanged; every Module schema continues to describe its own user properties only. Configs that pass `--check` on 0.7.2 now also start the daemon on 0.7.3 — no operator action required beyond upgrading the binary.

### Fixed — missing `type` is now a parse-time error

Previously `def input foo { ... }` without a `type` key was silently skipped by `module_props.rs` and surfaced as a confusing "input '...' has no type" error only at daemon start. The parser now constructs `ModuleProperties` for every def block; a missing, duplicated, or non-ident `type` becomes a parse-time error with the def name in the message:

```text
input 'foo': missing required property 'type'
```

Same loudness as a syntax error, same location attribution.

## [0.7.2] - 2026-05-17
>
> declarative property schema — `--check` now loudly rejects every config typo

`--check`'s coverage extended from pipeline / process DSL to every property surface in the configuration: Module properties on `def input` / `def output`, their nested sub-blocks (`queue`, `tls`, `retry`, `headers`), and the top-level `control` / `geoip` / `table` blocks. Each Module advertises its accepted shape as a `&'static [PropertySpec]`; the analyzer and the runtime read the same declaration, so unknown keys, type-mismatched values, out-of-set enum values, and missing required fields surface as rustc-style errors with `did you mean ...?` suggestions instead of being silently defaulted away.

### Added — `dsl::schema` + per-Module `property_schema()`

- New `dsl::schema` module declares `PropertyValueKind` (`String | Int | Bool | Duration | Size | Enum | Block | StringMap`) and `PropertySpec`. Modules splice these into a single static schema; `dsl::schema::validate` walks any property surface against it and collects every finding in one pass.
- `Module::property_schema()` trait method (default `None` for a gradual migration; every built-in carries a schema after this release). `Module::build()` is the convenience entry that runs validation before construction for direct callers (tests, snippet libraries) — the `ModuleRegistry` does the equivalent step at `create_input` / `create_output` time.
- Shared `queue::QUEUE_PROPERTY_SPEC` declared once next to the queue parser and spliced into every output schema, so the `queue { type | path | max_size | capacity }` block is checked uniformly across all sinks.

### Added — analyzer wiring

- New `check::module_props` pass validates every `def input` / `def output` against the Module's schema, including a Levenshtein-based did-you-mean hint drawn from the registered type names when `type tcsp` or similar misses every known module.
- New `check::global_props` pass applies the same treatment to the top-level `control { socket | error_log }`, `geoip { database }`, and `table { <name> { load | max | ttl } }` blocks.
- New `DiagKind::PropertySchema` keeps these findings filterable separately from existing `UnknownIdent` / `TypeMismatch` / `Dataflow` categories.

### Fixed — `framing non_transparent` false-positive

The expression-level "unknown identifier" walk on output properties used to flag legitimate bare-ident enum values (`framing non_transparent`, `queue { type disk }`) as unbound — it had no idea the bare ident was an enum member. The walk now consults the Module's schema and skips its own shape check for keys the schema already owns. Workspace references inside template values (`address "${workspace.x}:1"`) are *not* skipped — the dataflow reference check still applies on every property regardless of schema coverage.

### Fixed — silent fallback on unknown enum values

Previously `framing non_trasnaprent` (a typo) silently fell back to the default framing. The schema layer rejects unknown enum values at both `--check` time and runtime startup, with a `did you mean ...?` hint.

### Fixed — `include` matching zero files is no longer silent

`include "path/that/does/not/exist.limpid"` (and any glob that expands to zero matches) used to pass `--check` silently and then surface at runtime as confusing "unknown process" errors with no obvious tie back to the typo'd include line. The loader now bails loudly with `include path '...' (resolved to '...') matched no files` at config-load time, before `--check` even runs the analyzer. Same posture as rsyslog / syslog-ng on a missing include directive.

### Security / hardening

- **Daemon mode now refuses to start as root (euid 0).** limpid is a network-listening daemon and an event-processing engine; both surfaces have meaningful blast radius if compromised, so the principle is "drop privileges before reading any event". The canonical operational shape is systemd `User=limpid` plus `AmbientCapabilities=CAP_NET_BIND_SERVICE` for listeners on privileged ports (< 1024). The check applies only to daemon mode; `--check` / `--test-pipeline` / `--graph` are read-only and run fine as root. Operators who genuinely need to run the daemon as root (containerised init, debugging) can set `LIMPID_ALLOW_ROOT=1` to override.

### Notes

- Configs that pass `--check` today still pass. Configs that previously slipped through with silent fallbacks (typo'd keys, unknown enum values, mis-typed `type` ident) now fail loudly. This is a bug fix, not a breaking change in the semver sense — the previous behaviour silently corrupted the operator's intent.
- `dsl::schema::levenshtein` consolidates the two Levenshtein implementations the codebase used to carry; the analyzer's `suggestions` module now re-exports the same routine the schema validator uses.

## [0.7.1] - 2026-05-17
>
> journal input LOTL + transport-agnostic vocabulary parsers + datetime primitives + additional SIEM and OSS NDR parsers (real-traffic verified)

The journal input is rewritten to emit `journalctl -o json`-equivalent JSON on `ingress`, replacing the synthesised `"IDENTIFIER[PID]: MESSAGE"` string and the silent loss of every non-MESSAGE journald field. Downstream snippets (`parse_journald`, `parse_openssh`, `parse_sudo`, etc.) now see PRIORITY, _PID,_HOSTNAME, __REALTIME_TIMESTAMP,_SYSTEMD_UNIT,_SELINUX_CONTEXT, and the rest by their journald-canonical names.

Vocabulary parsers (`parse_openssh`, `parse_sudo`, `parse_postfix`, `parse_combined_log`) are decoupled from their transport. Each now reads from a vocabulary-named workspace namespace (`workspace.openssh.*`, `workspace.sudo.*`, …) that the pipeline writer populates via an inline bridge from whichever transport actually arrived. The vocabulary parser does not enumerate transports — that knowledge stays in the pipeline. OCSF records grow `time`, `device.hostname`, and `actor.process.pid` from the trusted source the transport provides.

Plus three new datetime parsers: `parse_datetime_rfc3339` and `parse_datetime_rfc2822` as Rust primitives, `parse_datetime_rfc3164` as an LPL snippet. The split mirrors the design principle line — spec'd atomic parsers live in Rust; policy / heuristic / fallback live in LPL.

### Fixed — journal input is dumb transport again (Principle 2)

`crates/limpid/src/modules/input/journal.rs` previously synthesised `"IDENTIFIER[PID]: MESSAGE"` on `ingress` and discarded every other journald field. The synthesis violated Principle 2 (input is dumb transport, no interpretation) and forced every downstream `wrap_*` process to re-extract pid / identifier with regex against the synthesised string. PRI on the wire was lost entirely because facility/severity were thrown away by the input.

Live off the land: `ingress` is now byte-equivalent to one line of `journalctl -o json`. All enumerated data fields are preserved under their journald-canonical names; trusted-address metadata (`__CURSOR` / `__REALTIME_TIMESTAMP` / `__MONOTONIC_TIMESTAMP`) is surfaced via the libsystemd metadata APIs. Non-UTF-8 byte values become JSON arrays of integers (journalctl convention).

`__SEQNUM` / `__SEQNUM_ID` are not surfaced — the `systemd-0.10.x` crate exposes no equivalent API. Add when upstream support lands.

Workspace stays empty on input. Downstream snippets (`parse_journald` etc.) decode the JSON in the process layer.

**Breaking**: any pipeline that consumed the old synthesised string needs to switch to `process parse_journald` + an inline bridge.

### Added — datetime parser primitives

Three layered datetime parsers, picked by what the wire actually carries:

- **`parse_datetime_rfc3339(text)`** — Rust primitive. Strict internet profile of ISO 8601 used by RFC 5424 syslog, OTLP, OCSF `time`, AWS CloudTrail `eventTime`, and most modern cloud audit logs. Accepts `Z` / `±HH:MM` / `±HHMM` transparently — solves the `strptime("...Z", "%z")` gotcha (`chrono` rejects the bare `Z` literal under `%z`).
- **`parse_datetime_rfc2822(text)`** — Rust primitive. Email `Date:` headers and legacy HTTP-date-style wires.
- **`parse_datetime_rfc3164(text)`** — LPL `def function` shipped as `packaging/snippets/functions/parse_datetime_rfc3164.limpid`. RFC 3164 wire (`Apr 30 01:23:45`) carries neither year nor timezone; the parser encodes the standard policy (current-year + future-clamp + UTC assumption — what rsyslog / syslog-ng / Vector / Fluent Bit all converge on) in DSL so operators on non-UTC senders can fork and edit without a rebuild.

### Added — additional vendor parsers

Five additional vendor / vocabulary parsers covering Juniper SRX, Check Point, Trellix NSP, Sysmon, BIND, and auditd:

| Parser | Source | OCSF class(es) |
| --- | --- | --- |
| `parse_juniper_srx_sd_syslog` | Juniper Junos SRX in `set security log format sd-syslog` mode — covers all daemons that emit a `[junos@<EID> ...]` SD block: **RT_FLOW** (SESSION_CREATE/CLOSE/DENY + APPTRACK_SESSION_*), **RT_IDP** (IDP_ATTACK_LOG_EVENT + IDP_APPDDOS_*), **RT_IDS** (RT_SCREEN_*), **RT_UTM** (AV / Antispam / Content / Webfilter), **RT_AAMW** (Sky ATP), **RT_SECINTEL** (threat-feed). Verified against the elastic/integrations juniper_srx corpus (66/66 emit, 0 error) | 4001 / 2004 / 4002 |
| `parse_juniper_srx_syslog` | Juniper Junos SRX RT_IDP / IDP_ATTACK_LOG_EVENT (RFC 3164 unstructured syslog — `set security log format syslog` default mode) — real-traffic verified | 2004 |
| `parse_nsp` | Trellix / McAfee Network Security Platform (NSP) IPS alerts. **Real-traffic verified**: 72/72 alerts emit cleanly across HTTP / SSH / SSL / NETBIOS-SS / TELNET / NTP / BACKDOOR categories. Real wire turned out to emit unquoted multi-word values (`attack_name=NETBIOS-SS: Windows SMB Remote Code Execution Vulnerability` without the documented quotes); the parser now uses a single fixed-order regex over the full Trellix standard template, which is the only robust extraction strategy for unquoted KV with embedded spaces | 2004 |
| `parse_checkpoint_leef` | Check Point LEEF 2.0 traffic events (Accept / Drop / Reject / Block) inside a syslog wrapper. Renamed from `parse_checkpoint`; targets the LEEF wire format used by QRadar bridges. Synthetic-verified only | 4001 |
| `parse_checkpoint_syslog` | Check Point Syslog Exporter wire format (`[key:"value"; ...]` SD with `sys_message::"..."` double-colon convention; also handles R81+ `Log [Fields@<EID> ...]` `=` variant). **Real-corpus verified** against elastic/integrations checkpoint (91/91 events emit across firewall / threat / auth / audit dispositions) | 4001 / 2004 / 3002 |
| `parse_sysmon` | Microsoft Sysmon EventID 1 (ProcessCreate) / 3 (NetworkConnect) / 11 (FileCreate), as JSON via NXLog / Vector / Winlogbeat. Synthetic-verified only — the elastic sysmon_linux corpus uses a different field-path convention (`winlog.event_id` / `winlog.event_data`) so it cannot exercise this parser as-is | 1007 / 4001 / 1001 |
| `parse_bind` | ISC BIND 9 `querylog` text format (`category queries`). Synthetic-verified only — no public corpus discovered for the format | 4003 |
| `parse_auditd` | Linux auditd, covers ~45 type codes across 7 OCSF classes (3002 Authentication / 3001 Account Change / 1007 Process Activity / 1001 File System / 4001 Network / 2002 Vulnerability Finding / 2004 Detection Finding). Handles `node=<host>` prefix injected by RHEL `audisp-remote` dispatcher. **Real-corpus verified** against elastic/integrations auditd (68/69 emit, 1 corrupt record errors loudly) | 3002 / 3001 / 1007 / 1001 / 4001 / 2002 / 2004 |

Junos security logs ship in two distinct wire formats — the `sd-syslog` structured form is rare in practice (most SRX deployments stay on the default `syslog` mode), so both formats get a dedicated parser per the library's one-file-per-`(vendor, format)` convention. `parse_juniper_srx_sd_syslog` is synthetic-verified only; `parse_juniper_srx_syslog` is verified against live RT_IDP traffic (RT_IDP / IDP_ATTACK_LOG_EVENT → OCSF 2004 Detection Finding with `finding_info` / `attacks` / `connection_info` populated).

Each parser follows the v0.7.1 intake-schema convention (`workspace.<vocab>.{body, …}` with hostname / time from the upstream bridge) and surfaces `device.hostname` and `actor.process.pid` in the emitted OCSF record where the wire provides them. Coverage scopes are documented per file — only the authentication subset of auditd's type codes is in scope this release (USER_LOGIN / USER_AUTH / USER_ACCT / USER_LOGOUT / CRED_ACQ / CRED_DISP); SYSCALL / EXECVE / PATH multi-record assembly is out of scope.

Compose_ocsf leaves extended for the new parsers' fields:

- 1001 / 1007 / 4001 / 4003 gain `status_id`, `actor`, `device`, and `unmapped` forwarding
- 2004 Detection Finding gains `status_id`, `connection_info`, `device`, `actor`, `attacks`, and `unmapped` forwarding (for the new Juniper SRX IDP parser)

Real-corpus verification pass on elastic/integrations and (for NSP) real wire traffic completes the trustworthiness story for most of these vendor parsers — every parser's header `Coverage scope` section now lists the exact corpus / dataset it was exercised against, and what residual gaps remain (Sysmon and BIND have no usable public corpus; CheckPoint LEEF has no public corpus distinct from the Syslog Exporter form).

### Added — OSS NDR parsers (Suricata + Zeek)

The de-facto open-source NDR pair. Suricata raises alerts via signatures; Zeek records per-protocol telemetry exhaustively. Operators deploying either ship event volumes that dwarf any single vendor source, so both get first-class snippet coverage.

| Parser | Source | OCSF class(es) |
| --- | --- | --- |
| `parse_suricata` | OISF Suricata Extensible Event Format (EVE) JSON, dispatched by `event_type`: alert → 2004, dns → 4003, http → 4002, flow / tls / fileinfo → 4001, stats → drop. **Real-corpus verified** against elastic/integrations suricata (61/63 emit + 1 stats drop + 1 corrupt JSON in corpus) | 2004 / 4001 / 4002 / 4003 |
| `parse_zeek_default` | Zeek default-enabled scripts: conn / dns / http / ssl / files / x509 / weird / notice. **Real-corpus verified** against elastic/integrations zeek (61/61 emit, all 8 streams) | 2004 / 4001 / 4002 / 4003 |
| `parse_zeek_soc` | Adds auth / protocol scripts most SOC deployments enable: ssh / smtp / ftp / dhcp / kerberos / ntlm / radius / smb_{mapping,cmd,files} / dce_rpc / snmp / rdp. Transitively includes `parse_zeek_default`. **Real-corpus verified** (85/85 emit, 20 distinct streams) | + 3002 / 4004 / 4005 / 4006 / 4007 / 4008 / 4009 |
| `parse_zeek_full` | Adds the rest (signature / intel / traceroute / tunnel / pe / mysql / irc / sip / dnp3 / modbus / socks / syslog / ntp / ocsp / rfb / dpd) + drops low-value operational streams (stats / capture_loss / known_hosts / known_services / known_certs / software) + a **catch-all** that wraps any remaining unknown `_path` into a 4001 record's `unmapped` (zero data loss guarantee). Transitively includes `parse_zeek_soc`. **Real-corpus verified** against the full 43-stream elastic/integrations zeek corpus (120/135 emit + 15 expected drops) | + catch-all |

Zeek's scope layering is **nested**: an operator picking `parse_zeek_soc` automatically gets default coverage (the SOC file includes default); picking `parse_zeek_full` gets soc + default + everything else. One include line, one process name in the pipeline.

Each Zeek scope file also ships **convenience entry points** with `_native` / `_flat` suffixes that fold the intake step into the parser itself:

- `_native` — Zeek's own JSON output (5-tuple nested under `id`), the expected production shape.
- `_flat` — Filebeat / Logstash-flattened form (`"id.orig_h"` etc.), the dotted-keys shape downstream ES pipelines emit. Runs `nest_dotted_keys` first to recover the native nested shape before dispatch.

Pipeline becomes a single stage: `process parse_zeek_soc_native
| compose_ocsf` (no separate `process { workspace.zeek = ... }`
intake block needed).

Suricata's EVE format does not vary by downstream shipper, so it ships only one entry point following the existing intake-separate convention.

### Added — `nest_dotted_keys` primitive

Some upstreams (Filebeat / Logstash JSON emitters used by zeek and suricata modules, certain Splunk HEC sources, OpenSearch ingest pipelines) flatten nested JSON for Elasticsearch indexing conventions: `{"id": {"orig_h": "1.1.1.1"}}` becomes `{"id.orig_h": "1.1.1.1"}`. The limpid DSL deliberately does not expose bracket-subscript access (`body["id.orig_h"]`), so dotted keys are unreachable from a parser without normalising first.

`nest_dotted_keys(obj)` recursively un-flattens dotted keys back into nested Objects, with loud-fail on collisions (`{"a": 1, "a.b": 2}` errors out clearly). Generic across vendors — used by parse_zeek_*_flat variants, and equally applicable to any other Filebeat-flattened JSON.

### Added — shared `proto_num` / `http_method_activity_id` LPL helpers (DRY across vendor parsers)

Two cross-vendor OCSF mappings were duplicated across parsers (9 × `*_proto_num` and 3 × `*_http_activity_id`), each with the same semantic body. Extracted to two shared LPL functions:

- `packaging/snippets/functions/proto_num.limpid` — IANA protocol number lookup, case-insensitive (`lower()`-folded), covering tcp / udp / icmp / icmpv6 / sctp / gre / esp / ah. Replaces `zeek_proto_num` / `suricata_proto_num` / `checkpoint_{syslog,leef}_proto_num` / `juniper_srx_{sd_syslog,syslog}_proto_num` / `paloalto_{syslog,cef}_proto_num` / `sysmon_proto_num`.
- `packaging/snippets/functions/http_method_activity_id.limpid` — HTTP request method → OCSF 4002 activity_id, the spec-standard mapping. Replaces `suricata_http_activity_id` / `zeek_http_activity_id` / `combined_log_activity_id`.

Parsers now `include "../functions/<name>.limpid"` and call the shared helper directly. Behaviour identical (case-insensitive proto_num is a superset that accepts every previous vendor-specific case style; HTTP method mappings were already byte-identical across the three sites). 12 callsites updated, ~80 lines of duplicated helper deleted.

### Fixed — `parse_auditd` system-lifecycle section header drift

The "1008 Application Lifecycle" section header in `parse_auditd.limpid` suggested the function emitted `class_uid: 1008`, but the code emits 1007 Process Activity (OCSF 1008 is actually Windows Registry Key Activity, not application lifecycle, which was confirmed during the auditd parser was first written). Header rewritten with the correct class plus the rationale.

### Fixed — `nest_dotted_keys` enforces depth limits (stack-overflow DoS mitigation)

`nest_dotted_keys` walked dotted keys and nested values recursively with no depth bound. An attacker-controlled JSON like `{"a.a.a...(100K dots)": 1}` would have recursed 100,000 deep into `insert_path` and overflowed the thread stack — a denial-of-service on any pipeline exposed to untrusted input (Zeek `_flat` operators ingesting Filebeat-processed logs are the natural attack surface).

Two limits added:

- `MAX_DOTTED_DEPTH = 32` — segment count per dotted key. Filebeat / Logstash typically flatten 2-4 levels, so 32 leaves headroom without enabling unbounded recursion.
- `MAX_VALUE_DEPTH = 64` — defence-in-depth bound for `nest()` walking into Object / Array values. `parse_json` already caps JSON parse depth at 128 (serde_json default), but this protects calls from other Value sources.

Both limits raise loud parser errors (route to error_log) rather than panic. Three new tests cover at-the-limit, above-the-limit, and value-nesting cases.

### Fixed — `parse_datetime_rfc3339` accepts `±HHMM` offset (was strict colon-only)

`chrono`'s `parse_from_rfc3339` is strict per RFC 3339 and requires the offset as `±HH:MM` (colon form) or `Z`. Many real emitters (Suricata EVE, journald JSON export, `jq -r` default, some CloudTrail regions) omit the colon and emit `±HHMM`. The primitive's doc claimed both forms were accepted, but the existing implementation only called the strict parser, so `±HHMM` bodies routed silently to error_log.

The primitive now composes a small fallback chain: `parse_from_rfc3339` → on failure → `parse_from_str` with `%z` (which accepts both shapes). Documented surface is therefore exactly `Z` / `±HH:MM` / `±HHMM`; deviations (space separator instead of `T`, ISO 8601 basic form without dashes, abbreviated offset `+09`, named zones) remain rejected and must be normalised upstream.

### Added — transport parsers + RFC 5424 composer

Three new snippets that pair with the journal LOTL fix to express transport stacking explicitly in pipelines:

- **`parsers/parse_syslog.limpid`** — thin wrapper around the `syslog.parse(ingress)` primitive that populates `workspace.syslog.*`. Lets a pipeline write `process parse_syslog | <bridge> | parse_<vocabulary>` rather than having every vocabulary parser inline its own `syslog.parse(ingress)` call.
- **`parsers/parse_journald.limpid`** — `workspace.journald = parse_json(ingress)`. Pairs with the LOTL change to expose all journald fields downstream by their canonical names.
- **`composers/compose_rfc5424.limpid`** — `workspace.journald.*` → RFC 5424 syslog wire. Replaces the hand-rolled `wrap_*` patterns on edge boxes that previously synthesised a frame from a regex-parsed string. Preserves the originating host via `coalesce(workspace.journald._HOSTNAME, hostname())` so relayed events keep their source identity instead of being stamped with the relay's hostname.

### Changed — vocabulary parser intake schemas

`parse_openssh`, `parse_sudo`, `parse_postfix`, and `parse_combined_log` no longer call `syslog.parse(ingress)` internally. Each reads from an explicit intake schema under its vocabulary namespace that the pipeline writer populates:

| Parser | Intake schema |
| --- | --- |
| `parse_openssh` | `workspace.openssh.{body, pid, hostname, time}` |
| `parse_sudo` | `workspace.sudo.{body, pid, hostname, time}` |
| `parse_postfix` | `workspace.postfix.{body, hostname, time}` (pid lives inside the body's postfix tag) |
| `parse_combined_log` | `workspace.combined_log.{body, hostname}` (CLF carries its own time + IP) |

The bridge from a transport into the intake is a one-process inline block in the pipeline; see each parser's file header for worked syslog / journald / tail examples.

OCSF records emitted by these parsers now include `time`, `device.hostname`, and `actor.process.pid` (where applicable) — values come from the trusted source the transport provides (journald `_PID` and `_HOSTNAME` are kernel-verified; syslog procid / hostname are sender-claimed). `compose_ocsf` leaves for 3002 / 3003 / 4002 / 4009 are extended to forward `device` and `actor` into the egress JSON.

`filter_openssh_journal` is rewritten to read `workspace.journald.MESSAGE` (set by upstream `parse_journald`) instead of doing `syslog.parse(ingress)` against a now-JSON ingress.

**Breaking**: existing pipelines must insert `process parse_syslog | { workspace.<vocab> = { … } } | parse_<vocab>` or the journald counterpart ahead of the vocabulary parser. The "just call `parse_<vendor>`" shortcut against syslog ingress no longer works.

### Added — design rationale and snippet authoring convention

- `docs/src/design-principles.md` gains a new operating rule "Workspace is event-scoped, not message-passed". Records that `process A | B` is sequential composition over a shared workspace, not an object pipe. Cites the "openssh over CEF over syslog over JSON over OCSF over OTLP" stack as an example of where the library explicitly stops covering and pushes the wiring decision to the pipeline writer.
- `docs/src/processing/design-guide.md` codifies the `// Upstream:` header convention. A vocabulary parser binds implicitly to a finite set of upstream stacks; spelling them out in the file header is the closest we get to a checkable contract without growing the DSL.

### Notes

- DSL syntax: unchanged.
- `cargo build --release` green; `cargo test --workspace` green. Datetime primitives gained an extra `accepts_microsecond_fractional_no_colon` test for the Suricata-shape `±HHMM`+microsecond case. `nest_dotted_keys` ships with 9 unit tests covering simple nesting, sibling merging, three-level nesting, recursion into nested objects / arrays, leaf/branch collision rejection, empty-segment rejection, and pass-through of non-Object inputs.
- Snippet library now also ships `packaging/snippets/functions/` for LPL `def function` helpers (currently `parse_datetime_rfc3164.limpid`).
- Snippet file count this release: 4 Zeek scope files (`parse_zeek_default` / `parse_zeek_soc` / `parse_zeek_full` + the `_native` / `_flat` convenience variants live inside each) + 1 Suricata + 1 CheckPoint Syslog Exporter + 2 Juniper SRX format variants + 1 Trellix NSP + expanded `parse_auditd` / `parse_openssh` = 9 new parser files on top of the v0.7.0 baseline.

---

## [0.7.0] - 2026-04-30
>
> snippet library v1 — 11 vendor parsers, OCSF 27-class composer; DSL fix for sub-process error propagation

The snippet library debut. Eleven vendor / format parsers ship, covering the operational vocabulary of the dominant unix and network-device log sources, plus a 27-class OCSF composer that maps the parser-canonical `workspace.limpid.*` shape to OCSF 1.3.0 JSON on `egress`. Operators can drop a single `include` into their config and immediately ship vendor logs into a SIEM / data lake in OCSF form.

Plus a DSL runtime fix that turned out to be load-bearing for the nested-dispatch parsers in this library: `error` from inside a sub-process now propagates correctly to the pipeline boundary instead of being swallowed at the `process` call.

### Added — Snippet library

Eleven parsers in `packaging/snippets/parsers/` (installed under `/usr/share/limpid/snippets/parsers/`):

| Parser | Source | OCSF class(es) emitted |
| --- | --- | --- |
| **Security devices / cloud audit** | | |
| `parse_fortigate_cef` | FortiGate (CEF wrap) | 4001 / 2004 / 3002 / 6002 |
| `parse_fortigate_syslog` | FortiGate (native KV syslog) | (same as CEF) |
| `parse_paloalto_cef` | PAN-OS (CEF wrap) | 4001 / 2004 / 6004 / 3002 |
| `parse_paloalto_syslog` | PAN-OS (native CSV syslog) | (same as CEF) |
| `parse_asa` | Cisco ASA / FTD-in-ASA-mode (syslog) | 3002 / 4001 |
| `parse_cloudtrail` | AWS CloudTrail (JSON) | 6003 API Activity |
| **Server / host systems** | | |
| `parse_openssh` | OpenSSH `sshd` (syslog / journald) | 3002 Authentication |
| `parse_sudo` | sudo (syslog / journald) | 3003 Authorize Session |
| `parse_combined_log` | Apache / Nginx access log (combined format) | 4002 HTTP Activity |
| `parse_postfix` | Postfix MTA (syslog) | 4009 Email Activity |
| `parse_winevent_json` | Windows Security event log (NXLog / Vector / Winlogbeat JSON) | 3002 / 1007 / 3001 / 3006 |
| **Vendor-neutral** | | |
| `parse_ocsf` | OCSF JSON inbound (any vendor's prior compose_ocsf output) | passthrough (any class) |

Two composers in `packaging/snippets/composers/`:

- `compose_ocsf` — dispatches by `workspace.limpid.class_uid` to per-class leaves, covering the OCSF 1.3.0 priority set (27 classes: 1001 / 1007 / 1008 / 1009 / 2002 / 2003 / 2004 / 2005 / 3001 / 3002 / 3003 / 3005 / 3006 / 4001 / 4002 / 4003 / 4004 / 4005 / 4006 / 4007 / 4008 / 4009 / 4010 / 6003 / 6004 / 6005 / 6007). Reads only `workspace.limpid.*` per the parser ↔ composer contract; vendor intermediates (`workspace.cef`, `workspace.syslog`) are not composer-visible.
- `compose_replayable` — minimal `{received_at, source, ingress}` shape that round-trips through `inject --json` for parser regression / replay capture.

One filter in `packaging/snippets/filters/`:

- `filter_openssh_journal` — drops `pam_unix(sshd:session): session opened/closed` PAM noise that journald sources before they reach `parse_openssh` (sshd already emits its own `Accepted ...` / `Disconnected ...` lines that cover the same authentication fact, so the PAM duplicate would double-count).

Field naming follows the parser ↔ composer contract: `workspace.limpid.<canonical-OCSF-field>` — the parser picks vendor fields off the wire and writes them to a single canonical scratch namespace, the composer reads only that namespace and emits OCSF JSON. Vendor intermediates (`workspace.cef`, `workspace.syslog`, `workspace.pf`, etc.) are parser-private.

Verified against real / public test corpora where available (playground sshd, FLAWS CloudTrail dataset, OTRF Mordor Windows event JSON, miroslav-siklosi Cisco ASA syslog generator, real Postfix mail.log slice). Each parser's docstring records the specific dataset and its parse-rate, plus `NOTE`-flagged subtypes that are documented but not yet exercised against live data.

### Fixed — sub-process `error` propagates past the `ProcessCall` boundary

`error` from inside a sub-process (`def process A { ... process B }` where `B` fires `error`) was being swallowed at the caller's `ProcessCall` arm in `crates/limpid/src/dsl/exec.rs`. Pre-fix the caller restored the event from a workspace snapshot and continued the pipeline as if nothing happened — making the operator-explicit DLQ routing invisible at the pipeline boundary. Downstream processes (typically `compose_ocsf`) then ran on the half-populated workspace and produced a confusing secondary error like `compose_ocsf: unsupported class_uid` that shadowed the original.

The fix removes the swallow: the sub-process Err propagates up through `exec_process_body` to the pipeline-level handler, which routes the event to the configured `error_log` (DLQ) exactly once with the operator's original message intact, and the rest of the pipe is skipped.

`try { process foo } catch { ... }` continues to work as before for fail-soft on a specific call — the catch body now actually runs after the sub-process error (pre-fix the swallow happened before `try`/`catch` could see the Err).

The bug shipped in v0.5.5 (the release that introduced the `error` keyword) and was present in v0.5.6 / v0.5.7 / v0.5.8 / v0.6.0 / v0.6.1. None of those releases routed sub-process errors to the DLQ correctly. Operators upgrading should expect their dispatcher-style parsers (`switch ... default { error "..." }` with `process X` in non-default arms) to start emitting DLQ entries that pre-fix were silently absorbed; configure `control { error_log "..." }` if you haven't already to capture them.

### Notes

- DSL syntax: unchanged.
- Public Rust API: unchanged. The fix is internal to `exec.rs`'s ProcessCall arm — no signature changes, no trait extensions.
- 361 tests pass (`cargo test --workspace`), `cargo build --release` green.
- Snippet library installation path: `/usr/share/limpid/snippets/` (the `_smoke-*.limpid` scaffolding under the repo root is the consumer-side `tail` config used to verify each parser locally; not packaged).
- Two regression tests added covering the sub-process error propagation contract: `test_exec_process_error_propagates_to_caller` (single-tier propagation) and `test_exec_try_catch_on_error` (try/catch still catches a sub-process Err post-fix).

---

## [0.6.1] - 2026-04-30
>
> perf: multi-pipeline scaling — 4-pipeline D-pipeline aggregate 374k → 459k events/sec (+23%, scaling 2.27× → 2.73×)

A short follow-up to v0.6.0 closing the multi-pipeline scaling gap that the perf-milestone profile surfaced after release. Three small changes that compound:

1. **Per-worker bump-arena recycling** — the per-event `bumpalo::Bump::new()` introduced in v0.6.0 became a contention point on the macOS xzm allocator's per-zone lock once multiple pipelines ran concurrently. Hoist the `Bump` into the per-input pipeline-worker task's local state and recycle via `Bump::reset()` between events. Steady state: zero allocations on the hot path.
2. **Pass the input event by reference through fan-out** — when multiple pipelines fan out from one input, the dispatcher used to `Event::clone()` per worker (workspace `HashMap` rebuild). The input event is read-only after `view_in` copies it into the per-event arena, so a `&Event` borrow is sufficient.
3. **`tracing/release_max_level_info`** — `trace!` / `debug!` macros compile to no-ops in release builds, eliminating per-event instrumentation cost (roughly half a percent of on-CPU on the multi-pipeline profile traced back to `mach_absolute_time` calls from tracing-event timestamps). Operators relying on `trace!` / `debug!` output need a debug build; `info!` / `warn!` / `error!` continue to fire.

### Changed — `pipeline::run_pipeline` signature

- New trailing parameter `bump: &mut bumpalo::Bump` — caller-supplied arena, reused across events. In-tree callers (`runtime`, `--test-pipeline` in `main`, unit tests) are migrated. Out-of-tree code that calls `run_pipeline` directly (rare; this is an internal API) passes `&mut bumpalo::Bump::new()`.
- `event` is now `&OwnedEvent` instead of `OwnedEvent`. Read-only access — `view_in` copies into the arena, the DLQ path constructs a fresh `OwnedEvent` from the borrowed view via `to_owned()`.

### Performance — single + multi pipeline (D pipeline, OCSF compose)

Same harness as v0.6.0. macOS, 16 physical cores. 3 reps each.

| Pipeline shape         | v0.5.7 | v0.6.0 | **v0.6.1** | Δ vs v0.6.0 |
|------------------------|-------:|-------:|-----------:|------------:|
| A passthrough          | 306k   | 303k   | **312k**   | +3%         |
| B `syslog.parse`       | 181k   | 282k   | **305k**   | +8%         |
| C parse + regex + if   | 73k    | 112k   | **115k**   | +3%         |
| D OCSF compose (UDP)   | 46.3k  | 168k   | **168k**   | ±0%         |
| D OCSF compose (TCP)   | n/a    | 170k   | **168k**   | ±0%         |
| **D 4-pipeline aggr.** | n/a    | 374k   | **459k**   | **+23%**    |

(eps/core for single-pipeline rows; eps aggregate for the 4-pipeline row. 4-pipeline is 4× independent inputs / pipelines / outputs sharing one process.)

Scaling on the 4-pipeline configuration improves from 2.27× the single-pipeline number on v0.6.0 to **2.73×** on v0.6.1. Single-pipeline throughput is essentially unchanged — there's no concurrency to expose the contention this patch removes, and the remaining levers are noise-magnitude individually. The lift comes when the daemon is actually running multiple pipelines, which is the production deployment shape.

The remaining 4-pipeline gap to true linear scaling (~3.5–4× of single-pipeline) is dominated by allocator activity in `OwnedEvent::clone` and HashMap operations in workspace handling that the per-event arena doesn't reach (event metadata between input task and pipeline worker, queue boundaries, etc). Closing it is a multi-day refactor — Linux native bench + `Arc<Event>` between input and pipeline worker — and not in scope for this patch.

### Notes

- DSL surface, config surface, and CLI surface: unchanged.
- The `Output` plugin trait is unchanged; out-of-tree output sinks written against v0.6.0 work without modification.
- 384 tests pass. `cargo build / clippy --release` green.
- Operators with genuinely high pipeline counts (≥ 16) can still override the default tokio worker thread count via `TOKIO_WORKER_THREADS=…` if their workload benefits — this release does not cap it (an earlier draft did, and it backfired in benches that had > 8 active tokio tasks).

## [0.6.0] - 2026-04-30
>
> perf milestone — D pipeline 46.3k → 168k eps/core (+263%); per-event arena, direct serializer, key interning, `CompactString`, and the `Output` boundary refactor

The v0.6.0 release closes the perf milestone framed in the v0.5.7 → v0.6.0 plan: collapse per-event allocation cost on the DSL hot path to the point that real work (I/O + tokio scheduling + the actual serializer) becomes the bottleneck. The headline number on the D pipeline (OCSF Authentication compose + `to_json`) is **168k eps/core**, up from 46.3k at v0.5.7 baseline — past the 100k milestone target by 65%.

DSL-surface and config-surface compatibility: **unchanged**. Every `def process / def pipeline / def input / def output` written against v0.5.x continues to parse, type-check, and run. The breaking changes in this release are confined to the **`Output` plugin trait**; in-tree sinks (`file`, `tcp`, `udp`, `unix_socket`, `stdout`, `http`, `otlp`, `kafka`) are migrated. Out-of-tree custom output sinks need to migrate (see "Output trait — breaking change" below).

### Performance — cumulative result

| Pipeline | DSL shape | v0.5.7 | **v0.6.0** | Δ |
| --- | --- | ---: | ---: | ---: |
| A | passthrough | 306k | 303k | ±0% |
| B | `syslog.parse(ingress)` | 181k | 282k | +56% |
| C | parse + 2× regex + if/else | 73k | 112k | +54% |
| **D** | **OCSF compose + to_json** | **46.3k** | **168k** | **+263%** |

(eps/core, single-pipeline single-input, channel-direct injection, UDP discard sink. 3 reps each, run-to-run spread ≤ 3.4%. Local measurement; raw data is not committed to the repo.)

Flamegraph composition flipped vs v0.5.7 baseline:

| Category | v0.5.7 | **v0.6.0** |
| --- | ---: | ---: |
| `malloc / free` | 42.99% | **14.93%** |
| `HashMap` / `IndexMap` rebuild | 11.77% | **4.00%** |
| `Clone` | 2.89% | **0.09%** |
| `__sendto` (output I/O) | n/a | 17.85% |
| tokio runtime | n/a | 10.40% |

`Value::to_owned_value`, `IndexMap::insert_full`, and the `OwnedValue` `drop_in_place` chain — the top-three alloc-related leaves at v0.5.7 — have all dropped out of the top 25 on v0.6.0.

### Added — bumpalo per-event arena (`crates/limpid/src/dsl/arena.rs`)

Every event entering `run_pipeline` gets a fresh `EventArena<'bump>` whose lifetime ends when the event finishes processing. All transient `Value::Object` / `Value::Array` / `Value::String` / `Value::Bytes` payloads allocate from this arena; the per-allocation `drop_in_place<Value>` chain (~23% of allocator samples on the v0.5.7 D pipeline) collapses into a single chunk-group free at event end.

The DSL `Value` enum is now lifetime-bound (`Value<'bump>`) — internal API change for embedders and out-of-tree DSL extensions (see "Out-of-tree extension migration" below). DSL configs are unchanged.

### Added — direct `serde::Serialize for Value<'bump>`

`to_json(workspace.x)` and other JSON-emit paths previously routed through an intermediate `serde_json::Value` tree. Implementing `Serialize` directly on the arena-backed `Value` skips that copy, collapsing `value_view_to_json` (1.11% of profile on the prior revision) to zero.

### Added — static-literal key interning in DSL hashes

`HashLit` keys (the `metadata`, `actor`, `src_endpoint`, … leaves of an OCSF compose) are interned at construction so the per-event `arena.alloc_str(...)` cost runs once at registry-build time, not once per event. This was the single largest unexpected win of the milestone (+13% on D, ~3× the planned estimate).

### Added — `CompactString` for `OwnedValue::String`

Short owned strings (≤ 24 bytes — covers most metadata fields: hostnames, IP strings, schema names, status enums) inline into the enum payload, eliminating a heap allocation per leaf for the common case. Long strings still spill to the heap unchanged.

### Changed — boundary refactor: `Output` trait split

**This is the only operator-visible breaking change in v0.6.0**, and it only affects out-of-tree output sinks. In-tree sinks are migrated in this release.

The pre-v0.6.0 `Output` trait took a fully-owned `&Event` at the sink boundary, which forced `BorrowedEvent::to_owned()` on every output statement — rebuilding the workspace HashMap (~10% on-CPU on the prior profile).

The new shape:

```rust
#[async_trait]
pub trait Output: HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static {
    /// Hot path: build a sink-specific payload from a borrowed event,
    /// using the per-event arena for any DSL eval (template paths,
    /// dynamic keys, etc.).
    fn render(
        &self,
        ev: &BorrowedEvent<'_>,
        arena: &EventArena<'_>,
    ) -> anyhow::Result<RenderedPayload>;

    /// Hot path: consume the rendered payload (downcast to the sink's
    /// concrete payload type) and perform I/O.
    async fn write(&self, payload: RenderedPayload) -> anyhow::Result<()>;

    /// Cold path (disk-queue replay): consume an `Event`. Default
    /// impl builds a transient arena, calls `view_in -> render ->
    /// write`. Sinks with a faster owned-form may override.
    async fn write_owned(&self, ev: &Event) -> anyhow::Result<()> { /* default */ }
}
```

`RenderedPayload` is a type-erased `Box<dyn Any + Send>` that each sink defines a concrete payload struct for (`FilePayload`, `UdpPayload`, …) and downcasts inside `write` — out-of-tree plugin sinks remain fully extensible without changes to the core. `Module` is no longer a supertrait of `Output` (`Module::from_properties` is `Sized`-bound and would forbid `dyn Output`); construction sites carry the `Module` bound separately.

`SinkInput { Owned, Rendered }` carries either form across `QueueSender`. Memory queues flow `Rendered` (no `to_owned` cost on the hot path); disk queues flow `Owned` only (Serialize/Deserialize survives restart). `CompiledConfig` exposes `outputs_queue_kind` so the pipeline executor routes at the output statement without consulting runtime state.

Retry semantics: `Owned` retains the full N-attempt retry loop (event is cloned up front); `Rendered` is single-shot (a `Box<dyn Any>` is consumed on first `write`). Sinks needing full retry should configure a disk queue. Documented at the `write_with_retry` call site.

### Out-of-tree extension migration

If you maintain an out-of-tree DSL function or output sink, the following internal API surfaces changed:

- **DSL functions** (in-tree primitives are migrated): the closure signature passed to `FunctionRegistry::register*` now takes `(arena, args, event)` (was `(args, event)`). `Value` is `Value<'bump>` and `Copy`. `FunctionRegistry::call` takes a `&BorrowedEvent<'bump>` and `&'bump EventArena<'bump>` in addition to the prior args.
- **Output sinks**: implement `render` / `write` / (optionally) `write_owned` per the trait shape above. `Module::from_properties` is unchanged for construction.
- **Custom processes**: `ProcessRegistry::call` takes `BorrowedEvent<'bump>` + `&'bump EventArena<'bump>` instead of an owned `Event`.

### Carried over from v0.5.8

The v0.5.8 release line is fully present in v0.6.0:

- `coalesce(a, b, c, ...)` first-non-null variadic primitive
- `syslog.parse` RFC 3164 TAG anchor fix (CEF inner-`": "` payload no longer absorbs into TAG/MSG split)
- `let f = <Object>; f.x.y` resolves through the local scope (read-side dot-access on let-bound Objects)

### Notes

- Build dependency: `bumpalo` (per-event arena), `compact_str` (small-string optimisation for owned values).
- Test count grew to 384 — coverage on the syslog/CEF parsers and `coalesce` was rebuilt from scratch for the new arena-shaped API (the v0.5.x pre-arena tests did not migrate cleanly).
- `--test-pipeline` / `--check` modes fall through to `SinkInput::Owned` when no live sinks are wired (no behavioural change for users).

## [0.5.8] - 2026-04-29
>
> `coalesce(...)` built-in for first-non-null fallback chains, plus a follow-up fix for dot-access on `let`-bound Object values

### Added — `coalesce(a, b, c, ...)` built-in (variadic)

A flat primitive that returns the leftmost non-null argument, or `null` when every argument is null. Designed to replace the verbose `switch true { x != null { x } default { y } }` pattern that snippet composers had to repeat per OCSF leaf for the "use the parsed value when present, fall back to an environment value otherwise" idiom:

```limpid
// before — per leaf, 4 lines plus indentation:
let event_time = switch true {
    workspace.limpid.time != null { workspace.limpid.time }
    default { received_at }
}
// after:
let event_time = coalesce(workspace.limpid.time, received_at)
```

Semantics:

- accepts ≥ 1 argument; the analyzer rejects zero-arg calls and the runtime returns the same arity error
- all arguments are evaluated (DSL has no short-circuit at call sites); since DSL identifiers and built-ins are pure, eager evaluation has no observable difference from short-circuit
- only `null` is "passed over" — empty strings, zero, empty objects, and empty arrays are real present-but-empty values and are returned as-is. Callers who want "blank string is also absent" express that explicitly

Implementation note: this is the first variadic built-in. The `Arity::Variadic { min }` enum variant was reintroduced (it had been removed earlier as unused). Adding the variant is a non-breaking extension — every existing built-in continues to use `Fixed` or `Optional`. The analyzer's argument type-check uses the single declared element type for every actual argument slot.

This is the fourth DSL gap surfaced and fixed mid-snippet-library work — alongside `error` (v0.5.5), the `source` reshape (v0.5.6), and `null_omit` (v0.5.7).

### Fixed — `let f = <Object>; f.x.y` resolves correctly

`let f = regex_parse(...); f.user` was failing at runtime with `unknown identifier: f.user`. The local-scope path-resolver in `crates/limpid/src/dsl/eval.rs` only consulted let bindings for single-segment idents (`parts.len() == 1`), so any multi-segment access whose root happened to be let-bound (`f.user`, `f.a.b`, `f.list[0].kind`) skipped scope lookup entirely and fell through to the catch-all "unknown identifier" arm. The analyzer's UnknownIdent warning had the same gap.

The fix extends both code paths: when the first segment matches a let binding, the runtime walks the bound value via the same `resolve_workspace_path` Object/Array walker used for `workspace.x.y.z`, and the analyzer suppresses the warning for the whole path. Missing keys yield `Null` to match the workspace path-walker contract — callers handle absence via `coalesce` or explicit null comparison.

```limpid
// before — runtime "unknown identifier: f.user":
def process parse_xxx {
    let f = regex_parse(workspace.body, "(?P<user>\\S+)")
    workspace.limpid = { user: f.user }     // ← runtime error
}
// after — works as written:
def process parse_xxx {
    let f = regex_parse(workspace.body, "(?P<user>\\S+)")
    workspace.limpid = { user: f.user }     // ✅ "alice"
}
```

Surfaced while writing parse_asa (Cisco ASA syslog parser) — every per-message-ID leaf does `let f = regex_parse(workspace.asa.body, "...")` and reads named captures via `f.user` / `f.src_ip` / etc.

Two regression tests added covering the happy path and the missing-key (Null) path.

### Notes

- No DSL syntax change. `coalesce` is a regular flat primitive call. The let-bound dot-access fix is a behaviour change in path resolution semantics: before, `f.x` failed; after, it walks into the bound Object.
- No breaking changes (the only behaviour shift is the previously-failing case starting to work).

---

## [0.5.7] - 2026-04-29
>
> `null_omit` built-in to drop `null` keys from HashLit composer output

### Added — `null_omit(value)` built-in for HashLit cleanup

A flat primitive that recursively strips `null` from objects and arrays. Designed for the OCSF-shape composer pattern (build a HashLit from parser-populated workspace fields, then `to_json` for `egress`). Without it, every absent field renders as `"key": null` in the output — OCSF schema validation in Sentinel / Splunk DM often chokes on that.

```limpid
workspace.limpid = {
    class_uid: 4001,
    src_endpoint: { ip: workspace.cef.src, port: to_int(workspace.cef.spt) },
    dst_endpoint: workspace.cef.dst_endpoint,   // may be null on this event
    traffic: workspace.cef.traffic              // may be null on this event
}
egress = to_json(null_omit(workspace.limpid))
//  → {"class_uid":4001,"src_endpoint":{"ip":"...","port":...}}
//    (dst_endpoint and traffic dropped cleanly)
```

Semantics (recursive, single pass):

- `null` keys are dropped from objects (or top-level `null` returns `null`); the function recurses into the remaining values
- arrays are **not** compacted — a `null` slot in an array survives unchanged, because that's often the parser's placeholder ("this slot was unknown") and silently dropping it would hide the signal. The function recurses into non-null elements only. Use a dedicated array primitive when array compaction is the goal
- empty containers (`{}` / `[]`) are kept — the function strips `null` keys, it doesn't collapse a structure that just became empty
- scalars (`String`, `Int`, `Float`, `Bool`, `Bytes`, `Timestamp`) pass through unchanged

This is the third DSL gap surfaced and fixed mid-snippet-library work — alongside `error` (v0.5.5) and the `source` reshape (v0.5.6). The pattern is "implement broadly across vendors, surface DSL gaps, fix in 0.5.x patches before locking 0.6.0", and it's working as intended.

## [0.5.6] - 2026-04-27
>
> `source` reshaped to `{ip, port}` across DSL, wire, and tooling

### Changed (breaking) — `source` is now an Object with `.ip` and `.port`

The reserved DSL identifier `source` previously resolved to a flat `String` containing only the peer IP. Starting in 0.5.6 it resolves to an `Object { ip: String, port: Int }`, mirroring how `workspace` is already structured. This unlocks two things the IP-only form couldn't:

- Discriminating between two log originators bound to different source ports on the same host (a common multi-tenant pattern): `source.port == 5140` separates them.
- Faithful event capture for replay: a composer can write `${source.ip}:${source.port}` to produce a record `inject --json` accepts without losing the port to a `:0` placeholder.

```limpid
// Before (≤ 0.5.5):
if source == "192.0.2.10" { drop }
output file { path "/var/log/${source}/events.log" }

// After (0.5.6+):
if source.ip == "192.0.2.10" { drop }
output file { path "/var/log/${source.ip}/events.log" }
```

Migration: every site that compares `source` to a String, interpolates `${source}` into a path/template, or concatenates `source` with `+` needs `.ip` appended. The analyzer surfaces the mismatch via the existing type-check pass — bare `source` is now `Object`, and an `Object == String` comparison or string-context interpolation flags as a type warning.

### Changed (breaking) — wire format `source` matches the DSL shape

`tap --json`, `inject --json`, the error_log (DLQ), and the `--test-pipeline --input` parser now emit and accept `source` as the same `{ip, port}` object the DSL ident exposes:

```jsonc
// Before (≤ 0.5.5):
{ "source": "192.0.2.10:5140", ... }

// After (0.5.6+):
{ "source": { "ip": "192.0.2.10", "port": 5140 }, ... }
```

This eliminates the DSL/wire shape mismatch and lets a composer write `source: source` to round-trip cleanly. JSONL files captured by limpid 0.5.5 or earlier are no longer replayable on 0.5.6 without preprocessing — operators with archived captures can convert with `jq` (`'.source |= (split(":") | {ip:.[0], port:(.[1]|tonumber)})'`) before piping into `inject --json`.

The breaking surface stays bounded: operator-facing DSL and the JSONL wire shape are the only two places `source` is exposed. Pre-1.0 lets us reshape both together while the snippet library is still being authored, rather than later when external configs and captures depend on the old form.

## [0.5.5] - 2026-04-27
>
> `error` routing keyword for explicit DLQ routing

### Added — `error` routing keyword for explicit DLQ routing

Process and pipeline bodies now accept an `error` statement alongside `drop` and `finish`:

```limpid
def process parse_fortigate_cef {
    workspace.cef = cef.parse(workspace.syslog.msg)
    switch workspace.cef.name {
        "traffic" { process parse_fortigate_cef_traffic }
        "utm"     { process parse_fortigate_cef_utm }
        default   { error "unsupported FortiGate CEF subtype: ${workspace.cef.name}" }
    }
}
```

`error` takes an optional message expression — anything an `${...}` template can render — and routes the event to the [error log](./operations/error-log.md) exactly like a runtime process failure: counted as `events_errored`, written to `control { error_log "..." }` if configured, otherwise emitted as a structured `tracing::error!` line. The message lands in the DLQ entry's `reason` field so the operator sees *why* an event was rejected without reverse-engineering the bytes.

This fills a gap that snippet libraries hit immediately: a parser dispatcher that can't recognise the input subtype previously had to choose between `drop` (silent loss, looks intentional) and a hand-rolled runtime panic. Neither matches the intent of "this event was supposed to be processable but I cannot — operator action needed." `error` makes that intent first-class.

The keyword is rejected inside `def function` bodies (function body grammar is `let* + trailing expression`, no statement forms allowed) — pure expression functions stay pure.

## [0.5.4] - 2026-04-27
>
> User-defined pure functions (`def function`) with let-form bodies

### Added — `def function` for pure expression functions

User-defined functions are now a top-level definition kind, alongside `def input` / `def output` / `def process` / `def pipeline`. The body is zero or more `let` bindings followed by a required trailing expression that becomes the return value. Designed for the small mapping / lookup helpers that vendor parsers reuse — protocol number → name, severity string → OCSF `severity_id`, action string → activity_id — and for the small chains of intermediate values that make those mappings readable.

```limpid
def function normalize_proto(num) {
    switch num {
        6  { "tcp" }
        17 { "udp" }
        1  { "icmp" }
        default { null }
    }
}

def function severity_id_from_label(s) {
    let lowered = lower(trim(s))
    switch lowered {
        "critical" { 5 }
        "high"     { 4 }
        "medium"   { 3 }
        "low"      { 2 }
        "info"     { 1 }
        default    { 1 }
    }
}

def process parse_fortigate_cef_traffic {
    workspace.limpid = {
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        severity_id: severity_id_from_label(workspace.cef.severity),
        ...
    }
}
```

User-defined functions register into the same `FunctionRegistry` as built-in primitives — call sites dispatch through the standard `(namespace, name)` lookup, the analyzer arity-checks them the same way, and they compose anywhere an expression goes (HashLit values, function arguments, binary operands, output templates, pipeline-level `if` conditions). Function names must be bare identifiers; the dot namespace is reserved for schema-bound built-ins.

`let` is the assignment form for local-scope variables in the body — each `let x = …` line binds (or reassigns) `x` in the same scope. Re-binding the same name simply overwrites the prior value; there is no separate declaration step, no `let mut`, and no `x = …` re-assignment syntax. Each let RHS sees parameters and earlier lets; the trailing expression sees everything.

To keep functions pure, the analyzer rejects function bodies that:

- read from the Event (`ingress`, `egress`, `source`, `received_at`, `error`, any `workspace.*` path) — anywhere in the body, including inside a `let` RHS;
- reference a free variable that's neither a parameter nor an Event-bound name (a `config.foo` or bare `result` typo surfaces at `--check` time instead of failing at runtime);
- call into a user-defined `def process` (process bodies have side effects functions can't tolerate); or
- participate in a function-to-function call cycle (direct self-recursion or mutual recursion through a chain). If recursion is genuinely needed, use `def process` instead.

All four are hard errors at `--check` time — the config fails to load and the daemon won't start until they're fixed.

Side effects (`workspace.x = …`, `egress = …`, `drop` / `finish` / `output` routing, statement-form `if` / `switch` / `foreach` / `try-catch`) are rejected at the parser level — function body grammar accepts only `let` bindings and a trailing expression, so those statement forms simply aren't in the grammar.

A new expression-form `switch` lands at the same time. Each arm body is one expression; the matching arm's value is the value of the whole `switch`. Distinct from the statement-form `switch` in process / pipeline bodies (which routes events / mutates workspace). Use the expression form inside `def function` bodies, inside `let` RHS, or anywhere a value is expected.

## [0.5.3] - 2026-04-27
>
> limpidctl stats surfaces errored counters

### Fixed — `limpidctl stats` shows `events_errored` / `events_errored_unwritable`

The 0.5.2 pipeline metrics gained `events_errored` and `events_errored_unwritable` but the human-readable `limpidctl stats` renderer wasn't updated — the JSON form (`limpidctl stats --json`, control socket, Prometheus) carried both, the default text form silently dropped them. Operators saw zero on `stats` while the real number was hiding in the JSON.

The columns now render when they're non-zero:

```text
Pipelines:
  ama_forward         89 received  35 finished  23 dropped   0 discarded  31 errored
  splunk_archive      62 received  38 finished  24 dropped   0 discarded
```

Steady-state pipelines (no errors) keep the compact row — a column of zeros across every pipeline in the common case is just noise. A non-zero `events_errored_unwritable` adds a second column on top of `errored`.

## [0.5.2] - 2026-04-27
>
> Dead-letter queue for process errors

### Changed — process runtime errors route to a dead-letter queue (revising 0.5.1)

0.5.1 changed the pipeline so that a `process` runtime error caused the event to be **discarded** with a counter increment. That was appropriate for surfacing the silent corruption that 0.5.0's "warn-and-continue" produced, but for a log pipeline default-discard is itself a strong failure mode — security telemetry should not lose events to a config bug at the receiving SIEM.

The 0.5.2 default sets the failed event aside in a **dead-letter queue** (DLQ) so the operator can audit, fix the offending config, and replay:

- New `control { error_log "/var/log/limpid/errored.jsonl" }` property opts in to a JSONL file. Each errored event becomes one line:

```json
{
    "timestamp": "...",
    "reason": "...",
    "process": "wrap_journal",
    "pipeline": "journal_forward",
    "event": {"source": "...", "received_at": ..., "ingress": "..."}
}
```

The `event` sub-object is exactly what `limpidctl inject --json` needs to reconstruct a fresh Event, so replay is:

```bash
jq -c '.event' /var/log/limpid/errored.jsonl \
      | limpidctl inject input <name> --json
```

- When `error_log` is **unset**, the same record is emitted as a structured `tracing::error!` line so the data is never silently lost — it just lives in journald / stderr instead of a dedicated file. Operators using the daemon under systemd can still recover via `journalctl -u limpid -o json | jq …`.

- New `events_errored_unwritable` counter (and `limpid_pipeline_events_errored_unwritable_total` Prometheus metric): subset of `events_errored` for which the DLQ write itself failed (disk full, permissions, rotation race). The runtime falls back to the tracing channel; alarm on this counter — non-zero means the replay path may be incomplete.

- The pipeline-runtime trace now reads `event → error_log` instead of `event discarded`. `--test-pipeline` prints the would-be JSONL record after the trace so operators can rehearse the replay recipe without booting the daemon.

The downstream behaviour is unchanged from 0.5.1: errored events still don't reach any output, so there is no shape regression in the production stream. What changes is that the events are now **recoverable**.

### Fixed — DLQ writer hardening (audit follow-up)

- **Concurrent line interleave**: multiple pipeline workers calling `ErrorLogWriter::write` no longer race. POSIX `O_APPEND` atomicity only covers writes ≤ `PIPE_BUF` (Linux: 4 KiB), and DLQ records carrying base64-encoded binary `ingress` easily exceed that. An in-process `tokio::sync::Mutex` serialises the open + write sequence so each JSONL line is written whole.
- **Startup path validation**: `error_log` parent directory is stat()'d at daemon start; a typo'd / missing path is rejected before any event reaches the failure path. Previously the typo surfaced as `events_errored_unwritable` ticks at first failure.
- **Rotation guidance**: `operations/error-log.md` now ships a recommended `logrotate` configuration (`copytruncate` + `maxsize 1G`) so the DLQ has a documented disk-fill ceiling. In-process rotation is deferred to v0.6.0; operator-side `logrotate` covers the realistic blast radius for v0.5.2.

## [0.5.1] - 2026-04-27
>
> Analyzer strictness + pipeline error handling

### Breaking — process runtime errors discard the event

When a `process` statement raises a runtime error (unknown identifier, type mismatch, regex compile failure, …) the pipeline now **discards** the event and increments a new `events_errored` counter, instead of emitting a `WARN` and forwarding the event with its original `ingress` unchanged.

The previous fallback ("warn-and-pass-through") combined poorly with the analyzer gap that let unresolved bare identifiers slip past `--check`: a config that referenced a renamed Event field (e.g. pre-0.5 bare `timestamp`) loaded fine, then failed every event at runtime — but the original ingress was forwarded downstream, so the operator's wrap / enrichment process was silently bypassed.

Operators now see the failure in `events_errored` (and via the new `limpid_pipeline_events_errored_total` Prometheus metric / per-trace `error: ... (event discarded)` line), rather than discovering it hours later at the receiving SIEM. Configs that intend partial processing should use `try { ... } catch { ... }` to express that intent explicitly.

The same routing applies to inline `process { ... }` bodies, which previously bubbled the error up to the runtime as a Result and lost the event without incrementing any pipeline counter.

### Added — analyzer flags unknown bare identifiers

`--check` now warns when a `process` body or expression references an identifier that doesn't resolve to a reserved event ident (`ingress`, `egress`, `source`, `received_at`, `error`), a `let` binding, or a `workspace.*` path. The warning carries `DiagKind::UnknownIdent` so `--ultra-strict` promotes it to an error in CI.

A bare `timestamp` reference — the most common 0.4→0.5 migration miss — gets a targeted help line pointing at both alternatives: `received_at` for the wall-clock event time, `timestamp()` for the current instant. Other unknown idents fall back to the levenshtein suggestion engine ("did you mean `ingress`?").

The `type` property of an `output` block (its bare-ident value is a module-name reference resolved at config-load time, not a runtime expression) is exempt — flagging `stdout`, `tcp`, etc. as unknown would be a false positive.

## [0.5.0] - 2026-04-26
>
> OTLP transport + DSL surface freeze

### Changed — design principles restructured (still five)

The five design principles have been reorganised so each one carries its own architectural weight, rather than mixing principles with operating rules. The renumbered set:

1. **Zero hidden behavior** *(unchanged)*
2. **I/O is dumb transport** *(unchanged)*
3. **Only `egress` crosses hop boundaries** *(was Principle 4)*
4. **Atomic events through the pipeline** *(new)* — formalises the invariant that the pipeline never operates on bundles or fans out: inputs split wire-level batches into atomic Events, process snippets are 1-in-1-out (or 0 via `drop` / `finish`), outputs rebundle at the emit boundary. The OTLP envelope split, the `syslog_*` line split, the `batch_level` mode on the OTLP output — all are this one principle in different transports.
5. **Safety and operational transparency** *(new)* — formalises the software-construction stance that surfaces in every limpid feature: `--check` static analysis, `tap`/`inject`/`--test-pipeline` for verify-and-replay, `SIGHUP` atomic reload with rollback, retry + secondary + disk-WAL on outputs, `Drop` hooks for shutdown visibility. Principle 1 covers config-time transparency; Principle 5 covers runtime transparency and recoverability.

What used to be Principles 3 (domain knowledge in DSL) and 5 (schema identity by namespace) are now under a new *Operating rules* section in the same document — they are concrete consequences of Principles 1 and 2 rather than independent architectural commitments. Anything that previously cited *"per Principle 3"* should now cite *"per the Domain knowledge in DSL operating rule"* or, more usefully, the Principle the rule is derived from.

This is a docs-only change in v0.5.0; no code is affected. Pre-1.0, this kind of clarification is expected.

### Added — OpenTelemetry Protocol (OTLP) support

OTLP becomes a first-class transport across both ingest and emit, with all three OTLP wire formats supported:

- **Inputs**: [`otlp_http`](docs/src/inputs/otlp-http.md) (`POST /v1/logs`, `application/x-protobuf` and `application/json`) and [`otlp_grpc`](docs/src/inputs/otlp-grpc.md) (`opentelemetry.proto.collector.logs.v1.LogsService.Export`). Each LogRecord becomes one Event with `ingress` set to a singleton ResourceLogs (1 Resource + 1 Scope + 1 LogRecord), preserving full upstream context per Principle 2.
- **Output**: [`otlp`](docs/src/otlp.md) with `protocol "http_json" | "http_protobuf" | "grpc"`, `batch_size`, `batch_timeout`, `headers {}`, and TLS via system roots / custom CA.
- **Primitives** (in the new `otlp.*` namespace): `otlp.encode_resourcelog_protobuf` / `otlp.decode_resourcelog_protobuf` / `otlp.encode_resourcelog_json` / `otlp.decode_resourcelog_json`. HashLit shape mirrors the proto3 tree with snake_case keys; JSON form applies the canonical OTLP/JSON conventions (camelCase, u64-as-string, bytes-as-hex).

The hop contract is "egress = singleton ResourceLogs proto bytes": the process layer owns semantic conversion (severity mapping, OCSF→OTLP shape) via DSL snippets; Rust ships only the mechanical wire encode / decode (Principle 3).

### Added — OTLP throughput controls

Four orthogonal defense / throughput layers on the OTLP/HTTP input, each opt-in (default unlimited) so existing configs are unaffected:

- **`body_limit`** *(default `16MB`)* — bytes per request. Larger bodies are rejected with HTTP 413 *Payload Too Large* before any decode work runs. axum's `DefaultBodyLimit` shows up in the layer chain, replacing axum's own 2 MiB default which is too small for collector-to-collector batches.
- **`max_concurrent_requests`** — in-flight request cap (semaphore). Worst-case decode memory becomes `max_concurrent_requests × body_limit`, turning the open-ended decode-amplification path into a known quantity. Excess requests fail-fast with HTTP 503 *Service Unavailable* (OTLP senders retry, so backpressuring the socket would amplify overload).
- **`request_rate_limit`** — sustained req/sec (token bucket, reuses the existing `RateLimiter`). Smooths burst above the configured rate; pairs with the concurrency cap because a token bucket allows full burst-equal-to-rate at idle.
- **`rate_limit`** — sustained events/sec, per-emitted-LogRecord. Same implementation as `syslog_*`, applied after request decode and split, so it caps pipeline-send rate independent of how the events arrived.

`otlp_grpc` gets `rate_limit` on the same axis. Per-RPC throttling on the gRPC side relies on tonic's HTTP/2 stream limits and the existing `rate_limit` after split — no new property.

### Added — `otlp_grpc` server-side TLS / mTLS

Optional `tls { cert key ca }` block on the input. With `cert` + `key` the server presents a certificate; adding `ca` switches into mutual TLS mode where every client must present a certificate signed by that CA root. Mirrors the same block shape as `syslog_tls` (now parsed via a shared `TlsConfig::from_properties_block` helper). PEM files are loaded via `spawn_blocking` so a slow disk does not stall the tokio reactor at startup.

For the output, gRPC client-side TLS already shipped in the initial OTLP push; this release closes the symmetric server-side gap.

### Added — `otlp` output `batch_level` merging

Three settings, all producing OTLP that is semantically identical at the receiver — they differ only in wire framing and CPU/wire-size trade-off:

- **`none`** *(default)* — one ResourceLogs entry per buffered Event. Cheapest CPU, largest wire. Suitable when `batch_size = 1` or the collector tolerates redundancy.
- **`resource`** — Events sharing a Resource collapse into a single ResourceLogs entry; their ScopeLogs sit side-by-side under it.
- **`scope`** — as `resource` plus Events sharing a Scope inside the same Resource collapse into a single ScopeLogs whose `log_records[]` accumulates everything. Smallest wire, slightly higher CPU (Resource and Scope equality scans).

Resource and Scope equality is order-insensitive on attribute lists because proto3 makes no canonical-order promise on the wire.

### Added — `otlp` output retry with exponential backoff

`retry { max_attempts initial_wait max_wait backoff }` block on the output, parsed via the same `RetryConfig` shared with the file / tcp / http outputs. Internal retry is necessary specifically for the OTLP output because it batches Events from multiple `write()` calls into one request — without an internal retry, a single transient ship failure would lose the entire drained batch (the queue layer's per-event retry only re-pushes the most recent Event). Exhausted retries bubble the error up so the queue's secondary / drop policy still applies. Doubling under exponential backoff is `saturating_mul` for explicit overflow safety.

### Added — `Value::Bytes` variant in the DSL

The DSL runtime value type gains a first-class `Bytes(bytes::Bytes)` arm, replacing the `serde_json::Value`-based representation that silently corrupted non-UTF-8 byte streams via `from_utf8_lossy` / `String::into_bytes()`. User-facing surface is preserved:

- DSL syntax / semantics unchanged.
- `ingress` / `egress` reads return `Value::String` for UTF-8-clean data (the historical case) and only switch to `Value::Bytes` for non-UTF-8 content (which the previous code was already mangling).
- Existing primitives keep their return shapes.
- `tap --json` / persistence still emit JSON; `Value::Bytes` is encoded as `{"$bytes_b64": "..."}` with `$`-prefix key escaping for round-trip safety. The marker is internal; `to_json` / `parse_json` reject it.

Cross-primitive Bytes rules: text-only primitives (`upper`, `lower`, `regex_*`, `contains`, `format`, `to_int`, `to_json`, template interpolation, property traversal) error on Bytes — the "no-implicit-coercion" rule. Hash primitives (`md5`/`sha1`/`sha256`) and `len` accept Bytes natively. `Bytes + Bytes` concatenates byte-wise.

New conversion primitives at the text/binary boundary:

- **`to_bytes(s, encoding="utf8")`** — `utf8` (default) / `hex` / `base64`.
- **`to_string(b, encoding="utf8", strict=true)`** — `utf8` strict (errors on invalid UTF-8) or lossy, plus `hex` / `base64` printable forms.

### Breaking — `Event.timestamp` renamed to `Event.received_at`

The `Event` struct field, the reserved DSL identifier, the `format()` template placeholder, and the JSON serialisation key are all renamed from `timestamp` to `received_at`. The semantic clarification is that this field is **strictly the wall-clock time at which this hop received the event** — input modules never overwrite it from payload contents (Principle 2: input is dumb transport). Source-claimed event times, when extractable from the wire, surface in workspace fields like `syslog_timestamp` / `cef_rt` / `pan_generated_time` via parser primitives.

The old name was generic enough that some snippets and configs were treating it as if it carried the source-claimed event time, which it never reliably does.

**Migration** (mechanical sed across configs and any captured `tap --json` files):

```sh
find /etc/limpid -name '*.limpid' -exec sed -i \
    -e 's/\${timestamp}/\${received_at}/g' \
    -e 's/%{timestamp}/%{received_at}/g' \
    -e 's/strftime(timestamp,/strftime(received_at,/g' \
    {} +

# Captured tap --json files: rewrite the top-level key
jq -c '.received_at = .timestamp | del(.timestamp)' \
    old-capture.jsonl > new-capture.jsonl
```

There is no deprecation alias — `${timestamp}` and `%{timestamp}` are hard errors (analyzer / runtime) on v0.5.0+. The 0.5.0 release window is the right moment for the cut because pre-1.0 breaking changes are still expected.

### Breaking — schema parsers no longer prefix workspace keys

`syslog.parse` and `cef.parse` previously emitted keys with a `<schema>_` prefix (`syslog_hostname`, `cef_name`, …) on the rationale that workspace dumps would stay self-describing when several parsers populated the same event. In practice the prefix collided with the *capture* idiom — `workspace.s = syslog.parse(ingress)` produced `workspace.s.syslog_hostname`, double-prefixed — and made schema parsers behave inconsistently with format primitives (`parse_json`, `parse_kv`) which always emit raw keys.

Both schema parsers now return un-prefixed keys (`hostname`, `appname`, `version`, `name`, …). Namespacing is the operator's job and is the recommended pattern:

```limpid
workspace.syslog = syslog.parse(ingress)   // workspace.syslog.hostname, ...
workspace.cef    = cef.parse(ingress)      // workspace.cef.version, workspace.cef.src, ...
```

Bare invocation still works (`syslog.parse(ingress)` merges keys flat into `workspace`) but is collision-prone and discouraged. CEF extension keys (`src`, `dst`, `act`, …) were never prefixed — those names are part of the CEF spec and continue verbatim.

**Migration**: rewrite any references to `workspace.syslog_*` / `workspace.cef_*` in configs and snippets. The capture form is mechanically equivalent and clearer:

```sh
# 1. capture once at the top of each process body:
#      workspace.syslog = syslog.parse(ingress)
#      workspace.cef    = cef.parse(ingress)
# 2. rewrite the references:
sed -i 's/workspace\.syslog_/workspace.syslog./g; s/workspace\.cef_/workspace.cef./g' \
    /etc/limpid/**/*.limpid
```

### Breaking — `cef.parse` requires `CEF:` at position 0

Previously `cef.parse` located `CEF:` anywhere in the input (via `find`) so a `<PRI>` syslog wrapper was silently skipped. This overlapped responsibilities — header stripping is syslog's job, not CEF's — and could match the literal string `CEF:` if it appeared elsewhere in the payload.

`cef.parse` now requires the input to start with `CEF:`, erroring with `cef.parse(): input does not start with \`CEF:\`` otherwise. The canonical pattern when CEF is transported over syslog is:

```limpid
workspace.syslog = syslog.parse(ingress)
workspace.cef    = cef.parse(workspace.syslog.msg)
```

CEF arriving on transports without a syslog wrapper (HTTP, file tail, …) is unaffected — `CEF:` is at position 0 already.

### Breaking — `syslog.parse` PRI parsing aligned with RFC 5424 §6.2.1

`syslog.parse` now validates the leading `<PRI>` header strictly: 1–3 ASCII digits, value 0–191, framed by `<` and `>` at the start of the input. Inputs the previous parser tolerated silently — `<malformed text>...` (non-digit content), `<999>...` (out-of-range), `<>...` (empty PRI) — now error with `syslog.parse(): no PRI header`, matching the behaviour of the sibling `syslog.strip_pri` / `syslog.set_pri` / `syslog.extract_pri` primitives which already used the strict scanner.

If you have a flow that depended on the old lax behaviour to ingest non-syslog payloads via `syslog.parse`, switch to a different parser (`parse_kv`, `regex_parse`, or a snippet) — calling `syslog.parse` on something that isn't syslog has no defined output anyway.

### Added — `syslog.parse` emits `pri`, `facility`, `severity`, `timestamp`

Beyond the structural fields, `syslog.parse` now returns:

- **`pri`** (Int, 0–191) — the raw `<PRI>` value
- **`facility`** (Int, 0–23) — `pri / 8`
- **`severity`** (Int, 0–7) — `pri % 8`
- **`timestamp`** (String) — the source-claimed wire timestamp from the RFC 5424 / RFC 3164 header (previously dropped silently)

`pri` / `facility` / `severity` are always present (the parser errors when no valid PRI is found, per the breaking change above). The timestamp surfaces source-claimed event time for snippets that need it — e.g. for the OCSF `time` field or the OTLP `time_unix_nano` — without forcing a separate `extract_pri` + parse pass. The lighter `syslog.extract_pri` is still available for callers that only need the PRI byte without tokenising the rest of the header.

### Breaking — `output file` path templates are stricter

The `path` template renderer in the `file` output gained four guards that reject configs the previous lax renderer accepted silently. Each fires before any byte hits disk, per Principle 1 (zero hidden behaviour).

- **Per-interpolation slash strip.** Every `${...}` result has forward and back slashes replaced with `_`, so an interpolation cannot smuggle a path separator into the rendered path. The invariant is "one interpolation = one path component"; directory structure has to live in the literal parts of the template.
- **`..` rejected anywhere in the rendered path.** After all interpolations resolve, the path is split on `/` and any component exactly equal to `..` causes the write to error rather than being silently rewritten.
- **Empty interpolation rejected.** An interpolation that evaluates to the empty string errors instead of producing surprise paths like `/foo//bar` or `/foo/.log`.
- **Trailing-slash / no-filename rejected.** A rendered path that ends in `/` (no filename component) errors before the auto-mkdir runs, so a stray template like `/var/log/${workspace.host}/` cannot create empty directories silently.

Configs that depended on any of these silent rewrites should sanitise the inputs upstream (`regex_replace`, explicit fallbacks in a `process` block) and reference the cleaned workspace key from the template. Worked examples are in the [`output file`](docs/src/outputs/file.md) reference.

### Breaking — `format()` primitive removed

The `format(template)` primitive — which expanded `%{...}` placeholders against the current event — has been removed. The `${expr}` interpolation that any string literal supports is strictly more capable: it accepts any DSL expression rather than the limited `%{event.x}` / `%{workspace.x}` set, and it's resolved at parse time so typos are caught by `--check`.

**Migration**: rewrite `format("...")` calls to interpolated string literals.

```limpid
// before
egress = format("[%{source}] %{workspace.cef_name}: %{egress}")

// after
egress = "[${source}] ${workspace.cef.name}: ${egress}"
```

The `%{...}` syntax is gone entirely; `${expr}` is the single template form.

### Breaking — `to_json()` requires an argument

`to_json()` (no argument) used to serialise the entire `Event` (received_at + source + ingress + egress + workspace) as JSON — the same shape as `tap --json`. In practice operators almost always wanted the workspace alone (the parsed/enriched form to ship downstream), so the no-arg default was a hidden footgun.

`to_json` now requires exactly one argument. The most common pattern:

```limpid
egress = to_json(workspace)
```

For the old whole-event behaviour, build the shape explicitly: `to_json({received_at: received_at, source: source, workspace: workspace})`.

### Added — `parse_kv` separator argument

`parse_kv(text, separator)` lets the caller pass a single-byte separator (default `' '`). Comma-separated KV payloads — common in Cisco ASA, Microsoft Defender, and various OEM telemetry — now parse without a regex pre-pass:

```limpid
workspace.kv = parse_kv(workspace.syslog.msg, ",")
// "a=1,b=2,c=\"three,four\"" → {a: "1", b: "2", c: "three,four"}
```

Quoted values still work and may contain the separator (e.g. a comma inside a quoted string when separator is comma). The defaults hash literal can sit either as the second argument (when separator is the default space) or as the third (after an explicit separator).

### Breaking / Added — `Value::Timestamp` first-class DSL type

The DSL gains a typed `Value::Timestamp(DateTime<Utc>)` value arm. Inputs in any timezone (RFC3339 with offset, naive + explicit `tz` argument, etc.) are normalised to UTC at the boundary, so the runtime never has to reason about mixed offsets.

Previously every timestamp travelled through the runtime as an RFC3339 `Value::String` — type-unsafe, repeated parse cost, and easy to typo into `contains(received_at, "2026")` (silently false because of substring semantics).

Now:

- **`received_at`** → `Value::Timestamp` (was `Value::String`)
- **`timestamp()`** (new, replaces `now()`) → `Value::Timestamp`
- **`strptime(value, fmt[, tz])`** → `Value::Timestamp` (was String)
- **`strftime(timestamp, fmt[, tz])`** — first argument must be a `Value::Timestamp` (was String, parsed RFC3339 internally). Passing a string is a clear type error: `strftime(): first argument must be a timestamp, got string`.
- **`to_int(timestamp)`** → unix nanoseconds (`i64`), matching OTLP `time_unix_nano`. So `to_int(received_at)` is the natural way to get an epoch-nanos number.
- **String coercion** of `Value::Timestamp` (e.g. `${received_at}`, `to_string()`-style paths) renders RFC3339 — the user-visible surface is unchanged from 0.4 for type-correct configs.

DSL syntax does **not** change. Existing type-correct expressions (`strftime(received_at, "%Y-%m-%d", "local")`, `${received_at}`) keep working byte-for-byte. Only code that round-tripped timestamps through string operations (`contains(received_at, "...")`, `len(received_at)`, regex on `received_at`) errors at the analyzer or runtime — those were always meaningless on a timestamp and now fail loudly.

`now()` is removed; rename call sites to `timestamp()`. The new name matches the value type it returns and reads consistently with `received_at`.

### Breaking — `tap --json` and `inject --json` use unix nanoseconds for `received_at`

`tap --json` previously emitted `received_at` as an RFC3339 string; it now emits an `i64` of unix nanoseconds, matching OTLP `time_unix_nano`. `inject --json` reads the same wire form. Pre-0.5 captures (`*.jsonl` files holding RFC3339 strings) need to be migrated before replay:

```bash
jq -c '.received_at = (.received_at | sub("\\.\\d+"; "") | strptime("%Y-%m-%dT%H:%M:%S%z") | mktime * 1000000000)' \
    old-capture.jsonl > new-capture.jsonl
```

(For sub-second precision use a real script — `jq` doesn't carry nanos. The simpler migration is to discard old captures; nothing about pipeline correctness depends on replaying historical traffic through the new format.)

### Added — host / version primitives

- **`hostname()`** → `String` — the local machine's hostname, resolved at every call via `gethostname(2)`. Useful for tagging events with the forwarder's identity (`workspace.forwarded_by = hostname()`) and populating OTLP `host.name` resource attributes.
- **`version()`** → `String` — the limpid daemon's version baked in at compile time (e.g. `"0.5.0"`). Useful for provenance markers and OTLP `service.version`.

`hostname()` was previously referenced in the OTLP example block in the docs but was not actually implemented — that drift is closed.

### Added — `starts_with` / `ends_with` string predicates

Two new flat primitives complement `contains`:

- **`starts_with(haystack, needle)`** — `true` if `haystack` begins with `needle`.
- **`ends_with(haystack, needle)`** — `true` if `haystack` ends with `needle`.

Use these when *position* matters — e.g. dispatching to the right parser based on a leading prefix (`starts_with(workspace.syslog.msg, "CEF:")`) — rather than `contains`, which matches anywhere and would fire on a literal `CEF:` string buried elsewhere in the payload.

### Added — DSL primitives

- **`to_int(x)`** — coerce a value to `i64` (strings, floats, bools, nulls); returns `null` on unparseable input. Primary use: casting CEF extension values and CSV column strings to numeric OCSF fields (ports, session IDs).
- **`find_by(array, key, value)`** — locate the first object in an array whose `key` field equals `value`. No type coercion; `null` on no match. Designed for identity-based access to schemas that ship arrays-of-objects (MDE evidence, OCSF observables).
- **`csv_parse(text, field_names)`** — parse a single CSV row into an object keyed by the supplied field names, with RFC 4180 quoting. Replaces the `regex_parse` workaround for vendors (most notably Palo Alto) that emit 100+-field positional CSV syslog records.
- **`len(x)`** — cardinality for `Array` (elements), `String` (Unicode characters), `Object` (top-level keys). Scalars return `null`.
- **`append(arr, v)` / `prepend(arr, v)`** — return a new array with `v` added at the back / front. Input is unchanged; callers re-bind.

### Added — DSL arrays (positionless collections)

- **Array literals** (`[a, b, c]`, `[]`, mixed types, nesting, trailing commas) are now first-class expressions, evaluating to `Value::Array` at runtime. Grammar, AST (`ExprKind::ArrayLit`), parser, evaluator, and analyzer (`FieldType::Array`) all updated.
- **No positional access.** `arr[n]` and `arr[n] = v` are intentionally absent from the grammar. Arrays are addressed by identity (`find_by`, `foreach`) and mutated by "back / front" semantics (`append`, `prepend`). Numeric indexing drifts under insert / delete; identity addressing survives. See `docs/src/processing/user-defined.md#arrays` for the rationale.

### Fixed — security hardening from the v0.5.0 audit

- **OTLP output: header values no longer logged on validation failure.** The configured `headers { ... }` block typically holds bearer tokens / API keys. Previously, a malformed value would produce a `tracing::warn!` containing both key and value verbatim — leaking the credential into the log stream on misconfiguration. Now logs the key only, with explicit `value redacted`.
- **OTLP output: graceful-shutdown buffer warning.** `OtlpOutput` gained the `Drop` impl that `HttpOutput` already had: aborts the pending deferred-flush task and warns operators about events still in the buffer at shutdown. The events are not actually lost (the queue layer re-delivers from spool), but the count is now visible.
- **OTLP/HTTP: bounded decode-error log line.** `serde_json` / `prost` error wording is capped at 256 characters in the warn log to remove a pathological-payload log-amplification primitive.
- **OTLP gRPC input: panic-free peer fallback.** The `remote_addr()` fallback for non-TCP transports now constructs the unspecified `SocketAddr` directly instead of parsing a constant — removes a panic seed that any future refactor of the literal could revive.
- **OTLP output retry: saturating doubling.** `wait * 2` under exponential backoff is `saturating_mul(2)`. The realistic reach of `Duration` overflow is "never" (~584 years) but the explicit bound removes another panic seed.
- **`hostname()` panic-safe.** The `gethostname` 0.5.x crate panics on `gethostname(2)` syscall failure (chroot / namespace edge cases — vanishingly rare in practice). The primitive now wraps the call in `catch_unwind` and degrades to `Value::Null` on unwind, so a tokio task can't take the daemon down.
- **`to_int(Float)` rejects non-finite values.** `NaN` and `±∞` used to slip through `as i64` (NaN → 0, ∞ → `i64::MIN`/`i64::MAX`), both of which violate Principle 1. Finite-but-out-of-range floats still saturate (matching the documented `as`-cast semantics); non-finite values fall through to the same partial-data `Null` path as unparseable strings.

### Refactored — TLS helper centralization

`crate::tls` now owns the `tls { cert key ca }` block parser (`TlsConfig::from_properties_block`) and the rustls `CryptoProvider` installer (`install_default_crypto_provider`), both of which were duplicated across `syslog_tls`, `otlp_grpc` (input), and `otlp` (output) after the OTLP push. Consolidation keeps error wording uniform across modules and removes the only direct duplication flagged by the v0.5.0 abstraction review.

### Known limitations

- **`otlp_http` server-side TLS** is not implemented; front the input with a TLS-terminating proxy (envoy / nginx / traefik) or use `otlp_grpc` for native TLS. Native HTTPS support is queued for v0.5.x.
- **Selective re-send of OTLP `partial_success.rejected_log_records`** is logged as a warning only; the dedicated retry-just-the-rejects path is queued for v0.5.x. Transport-level retry shipped in this release covers hard failures (connection refused, 5xx, …).

## [0.4.0] - 2026-04-24

Testability release. Builds the static analyzer and observability tooling on top of the DSL finalised in v0.3.0. No DSL breaking changes — `limpid --check` does more, pipelines behave the same.

### Added — `limpid --check` static analyzer

- Full type-aware analyzer lives in `crates/limpid/src/check/` and runs whenever `limpid --check <config>` is invoked. It replaces the former "syntax OK" pass with real dataflow and type checking.
- Static type inference: `FieldType` + `Bindings` thread structural types through pipelines; function argument / return type signatures (`FunctionSig`), assignment type conflicts, operator type checks, and parser-function return shapes are all verified.
- Parser functions (`parse_json`, `parse_kv`, `syslog.parse`, `cef.parse`, `regex_parse`) declare the workspace keys they produce via `ParserInfo`; downstream references to those keys are verified.
- Diagnostic rendering: rustc-style source snippet + caret, "did you mean" Levenshtein suggestions for unknown identifiers / functions, and clear summary + footer lines.
- Expr-level span: diagnostics carry precise source spans from expression nodes (not just statements), so the caret points at the offending sub-expression (`lower(workspace.count)` → carets the arg).
- `include "<glob>";` in configs is expanded by the analyzer with a cycle-safe source map, and summary counts (input / output / process / pipeline) are emitted per check.
- Footer: clean configs end with `<path>: Configuration OK (N pipeline(s), M process(es); dataflow check passed)`; configs with warnings include the warning count; configs with errors exit 1 with `error: N error(s) found`.

### Added — CLI flags

- `--strict-warnings`: promotes warning count to exit-2 (diagnostic level stays warning). CI-friendly switch for "warnings are failures."
- `--ultra-strict`: promotes **unknown-identifier** warnings to errors (exit 1). Distinct axis from `--strict-warnings` — this one changes the diagnostic level, not just the exit code. The two flags compose: unknown idents become errors, other warnings can still trigger exit-2. Category is tagged via `DiagKind`; `UnknownIdent` is the currently promoted class.
- `--graph[=<format>]`: emits a structural view of every pipeline to stdout. Formats: `mermaid` (default, GitHub-renderable), `dot` (Graphviz), `ascii` (terminal-only tree). Analyzer output stays on stderr so `--graph | pbcopy` etc. works cleanly.

### Added — documentation

- `docs/src/operations/schema-validation.md` — operations guide for schema validation. Covers the design decision to not ship an in-tree validator, the `limpidctl tap --json | <validator>` recipe (OCSF / ECS / custom JSON Schema), and the alternatives that were rejected (in-tree validator, DSL schema annotations, runtime per-event checking). Cross-linked from `operations/tap.md`.

### Changed — internals

- `Module::schema()` removed. Input / output modules no longer declare a data contract: they are I/O-pure (bytes in / bytes out) and have nothing to advertise. Schema information is carried by `FunctionSig` / `ParserInfo` on the function registry, which is where the analyzer looks. `modules/schema.rs` now only exports the `FieldType` / `FieldSpec` vocabulary.
- AST `Expr` became a wrapper struct (`Expr { kind: ExprKind, span }`) to carry per-expression spans without rewriting every pattern match.
- Unused `name_span` / `key_span` fields on def / property AST nodes (left as `#[allow(dead_code)]` placeholders) were removed; they can come back if a future analyzer phase needs them.
- Diagnostic category is routed via `DiagKind` enum (`UnknownIdent` / `TypeMismatch` / `Dataflow` / `Other`) instead of message-string heuristics, so category rendering and `--ultra-strict` promotion share the same source of truth.

### Security / hardening

- Snippet renderer sanitises ASCII control bytes (0x00–0x1F minus `\t`, and 0x7F) to `?` before writing the source line to stderr. Prevents ANSI OSC/CSI injection through config contents displayed in a reviewer's terminal.
- `include "<glob>";` is now confined to the config's root directory. Absolute paths and `..` traversal outside that root are rejected with a clear error. Prevents an include line from silently pulling in arbitrary files (`/etc/passwd`, `~/.ssh/*` etc.) or from leaking the first bytes of such files via a pest parse error.

### Documentation fixes

- `limpidctl check` references in operations / pipelines / processing docs corrected to `limpid --check` (check lives in the daemon binary, not the CLI tool — this was decided during the v0.3.0 restructure, but the docs had drifted).

## [0.3.0] - 2026-04-24

DSL stabilization release. This is a broad pre-1.0 breaking change that settles the Event model, function namespaces, and core shape so that future work (analyzer polish, snippet library, transport expansion) can build on a final-form DSL without further surface-level churn.

### Breaking — Event model renamed

- `Event.raw` → `Event.ingress` (immutable bytes received on this hop)
- `Event.message` → `Event.egress` (bytes written on the wire by the output)
- `Event.fields` → `Event.workspace` (pipeline-local scratch namespace)
- `tap --json` / `inject --json` key names follow the rename; existing dumped replay files need `sed` (see `docs/src/operations/upgrade-0.3.md`)

### Breaking — Event core is now schema-agnostic

- `Event.facility` / `Event.severity` removed. These were syslog-specific metadata masquerading as pipeline-wide state; in a world where OTLP / OCSF / vendor JSON are first-class citizens, they do not belong in the Event core.
- DSL assignments `facility = N` / `severity = N` are now "unknown assignment target" errors. The PRI byte is constructed explicitly via the new `syslog.set_pri(egress, facility, severity)` function.
- `syslog.extract_pri(bytes)` returns the numeric PRI for reading.

### Breaking — Native process layer removed

- `modules/process/` is gone in its entirety. Pipeline statements like `process parse_syslog` no longer resolve to built-ins — schema-specific parsers are DSL functions (`syslog.parse(ingress)` etc.) invoked as statements inside an inline `process { ... }` block, and format primitives (`parse_json`, `parse_kv`, `regex_replace`) are flat DSL functions.
- `prepend_source` / `prepend_timestamp` have no direct replacement; the upgrade guide shows the `+` / `strftime` rewrite.

### Added — dot-namespaced function call syntax

- `<namespace>.<fn>(args)` grammar. Schema-specific functions declare their identity in the name. `parse_syslog(raw)` / `parse_cef(raw)` / `strip_pri(msg)` become `syslog.parse(ingress)` / `cef.parse(ingress)` / `syslog.strip_pri(egress)`. Flat primitives (JSON/KV/regex/hash/table) keep the bare-name form.
- New functions: `syslog.set_pri`, `syslog.extract_pri`, `regex_parse`, `hostname()`.

### Added — `regex_parse(target, pattern)`

- Named-capture extraction with dotted capture names producing nested objects: `(?P<date.month>\\w{3})` merges into `workspace.date.month`. Returns `Object` (bare-statement merges into `workspace`) or `null`.
- `regex_extract` remains as the single-value extractor.

### Added — `let` bindings

- `let x = <expr>` inside a `def process { ... }` body. Process-local scratch that keeps `workspace` clean of intermediate values. Bare-ident resolution is `LocalScope → Event metadata → error`.

### Added — pipeline fan-in

- `input a, b, c;` accepts multiple comma-separated inputs feeding the same pipeline body. Motivation: HA syslog (two redundant feeds running the same dedup / transform pipeline) no longer requires copy-pasting the pipeline twice.

### Added — `${expr}` template interpolation + string `+`

- `"prefix-${workspace.foo}-suffix"` interpolates any DSL expression. Old `%{name}` shorthand in `format()` has been removed; placeholders must be either reserved event names (`ingress`, `egress`, `source`, `timestamp`, `severity`, `facility`) or explicit `workspace.xxx` / `let`-bound names.
- `+` operator concatenates strings (falls back to arithmetic for numeric operands).

### Added — `strftime`, `hostname`

- `strftime(timestamp, format, tz?)` formats an RFC 3339 timestamp.
- `hostname()` returns the daemon's system hostname; portable configs can use `"${hostname()}"` in templates instead of hardcoding.

### Added — `output file` path templates via DSL evaluator

- `output file { path "/var/log/${source}/${strftime(timestamp, \"%Y-%m-%d\")}.log" }` evaluates the DSL expression per event instead of going through the legacy string template.

### Added — Design Principles page

- `docs/src/design-principles.md` publishes the five principles that govern limpid's scope (zero hidden behavior, I/O purity, domain knowledge as DSL snippets, only `egress` crosses hops, schema identity via namespaces).

### Added — developer / example docs

- `docs/src/processing/design-guide.md` — process design guide for contributors writing snippet library entries.
- `docs/src/pipelines/multi-host.md` — end-to-end worked example of a edge-host → relay → AMA multi-host pipeline, highlighting how the `tap` / `inject` primitives and the RFC 5424 hop contract turn a distributed pipeline into something you can reason about from one config.

### Changed — function code organization

- `crates/limpid/src/functions/` is now a tree of one-file-per-function modules: `primitives/` (flat), `syslog/` (dot namespace), `cef/` (dot namespace). The old `mod.rs` megafile is gone.
- Module trait introduced (`crates/limpid/src/modules/mod.rs`): `Module: Sized { fn schema() -> ModuleSchema; fn from_properties(...) }`. Replaces the former `FromProperties`. `schema()` is unused in-tree today but reserved for the upcoming analyzer (v0.4.0).

### Changed — hardening

- `limpid` and `limpidctl` restore `SIG_DFL` for SIGPIPE, so piped output (`limpidctl stats | head`) exits cleanly instead of panicking.
- `output http`: emits a `WARN` log when `verify false` disables TLS certificate validation, and the setting is documented as debugging-only.
- Control socket (`/var/run/limpid/control.sock`): max 8 concurrent connections, max 16 MiB per inject stream, max 4 KiB per command line.
- `syslog_tls` certificate and key loading moved off the async runtime via `spawn_blocking` to avoid stalling the reactor at startup.
- `fmt: cargo fmt --all` applied once across the tree so subsequent diffs are free of cosmetic noise.

### Internal refactors

- `<PRI>` header parsing consolidated into a single `parse_leading_pri` helper (was duplicated across `strip_pri`, `extract_pri`, `set_pri`).
- `values_equal` merged into `values_match` as the single equality routine for both `==`/`!=` and `switch` arms.
- TCP and Unix-socket outputs share a `PersistentConn` trait encoding the common "connect on first write, reconnect on broken pipe" pattern.
- `tls::build_client_config` (speculative dead code) removed; TLS client support will be reintroduced when an output needs it.

### Removed

- `modules/process/` (entire directory) and the `ModuleRegistry` process API (`register_process` / `call_process` / `process_names` / `ProcessFn`).
- `%{name}` shorthand in `format()` templates.
- `FromProperties` trait (absorbed into `Module`).

### Migration

See `docs/src/operations/upgrade-0.3.md` for end-to-end migration recipes including `sed` snippets for the Event model rename, the function rename table, and worked examples of replacing every removed native process with its DSL function equivalent.

## [0.2.2] - 2026-04-24

### Added

- `limpidctl inject --replay-timing[=<factor>]` — replays events at their original timing using each event's top-level `timestamp` field. Accepts `realtime` (= `1x`) or a factor like `10x` / `0.2x`. Defaults to `1x` when given without a value. Requires `--json`.

### Documentation

- `docs/src/operations/tap.md` — cadence-faithful replay section with examples (default / 10x / 0.2x / realtime), `--json` requirement, and the explicit failure cases (missing or unparseable timestamp, invalid factor, backwards timestamp, wall-clock catch-up) so there is no hidden behaviour.
- `docs/src/operations/cli.md` — `--replay-timing` entry in the CLI quick reference.

## [0.2.1] - 2026-04-18

### Fixed

- `--test-pipeline` now loads `table { ... }` global blocks from the configuration. Previously it constructed an empty `TableStore`, which caused pipelines using `table_lookup` / `table_upsert` / `table_delete` to emit "unknown table" warnings in test mode only.

## [0.2.0] - 2026-04-17

### Added

- `limpidctl inject <input|output> <name>` — pushes raw lines into a named input's event channel, or directly into an output's queue (bypassing pipelines entirely). Symmetric with `limpidctl tap`.
- `inject --json` — pushes full Event JSON (as emitted by `tap --json`), enabling `tap → inject` roundtrip for replay use cases.
- Control protocol: `inject <kind> <name> [json]`, EOF-terminated.
- Per-inject metrics: `events_injected` (for inputs and outputs) and `events_received` (for outputs).
- Prometheus exporter: three new counters (input injected, output injected, output received).

### Changed

- `limpidctl stats` output restructured to **Pipelines → Inputs → Outputs** ordering with updated counter set.

### Fixed

- `.gitignore` patterns to exclude common secrets layouts.
- `fold_by_precedence`: guard against empty operator lists.
- `tap.rs`: best-effort comment / error-path fixes surfaced by the v0.2.0 audit pass.

## [0.1.0] - 2026-04-17

Initial public release. Rust + tokio log pipeline daemon replacing rsyslog / syslog-ng / fluentd with a single readable DSL (`def input`, `def process`, `def output`, `def pipeline`). Includes syslog (UDP/TCP/ TLS) / tail / journal / unix socket inputs; file / HTTP / Kafka / TCP / UDP / unix socket / stdout outputs; in-DSL expression language with parsers (JSON / KV / CEF / syslog), regex, string templates, tables with TTL, GeoIP; control socket (`limpidctl tap`, `stats`, `health`); hot reload via `SIGHUP` with automatic rollback; per-output disk-backed queues.

[Unreleased]: https://github.com/naoto256/limpid/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/naoto256/limpid/compare/v0.7.15...v0.8.0
[0.7.15]: https://github.com/naoto256/limpid/compare/v0.7.14...v0.7.15
[0.7.14]: https://github.com/naoto256/limpid/compare/v0.7.13...v0.7.14
[0.7.13]: https://github.com/naoto256/limpid/compare/v0.7.12...v0.7.13
[0.7.12]: https://github.com/naoto256/limpid/compare/v0.7.11...v0.7.12
[0.7.11]: https://github.com/naoto256/limpid/compare/v0.7.10...v0.7.11
[0.7.10]: https://github.com/naoto256/limpid/compare/v0.7.9...v0.7.10
[0.7.9]: https://github.com/naoto256/limpid/compare/v0.7.8...v0.7.9
[0.7.8]: https://github.com/naoto256/limpid/compare/v0.7.7...v0.7.8
[0.7.7]: https://github.com/naoto256/limpid/compare/v0.7.6...v0.7.7
[0.7.6]: https://github.com/naoto256/limpid/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/naoto256/limpid/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/naoto256/limpid/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/naoto256/limpid/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/naoto256/limpid/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/naoto256/limpid/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/naoto256/limpid/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/naoto256/limpid/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/naoto256/limpid/compare/v0.5.8...v0.6.0
[0.5.8]: https://github.com/naoto256/limpid/compare/v0.5.7...v0.5.8
[0.5.7]: https://github.com/naoto256/limpid/compare/v0.5.6...v0.5.7
[0.5.6]: https://github.com/naoto256/limpid/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/naoto256/limpid/compare/v0.5.4...v0.5.5
[0.5.4]: https://github.com/naoto256/limpid/compare/v0.5.3...v0.5.4
[0.5.3]: https://github.com/naoto256/limpid/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/naoto256/limpid/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/naoto256/limpid/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/naoto256/limpid/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/naoto256/limpid/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/naoto256/limpid/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/naoto256/limpid/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/naoto256/limpid/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/naoto256/limpid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naoto256/limpid/releases/tag/v0.1.0

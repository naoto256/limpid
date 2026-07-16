# limpid Snippet Library

A read-only library of DSL snippets shipped with the `limpid` package
and installed under `/usr/share/limpid/snippets/`. User configurations
reference snippets by absolute path; the config loader's allow-list
(`SYSTEM_SNIPPET_DIR` in `config.rs`) explicitly permits this single
prefix.

## Layout

```
/usr/share/limpid/snippets/
├─ parsers/      per-vendor / per-format parsers, populating
│                workspace.lsis.parsed.* (the LSIS facts layer —
│                see LSIS below)
├─ composers/    target-shape rendering. Each composer reads
│                workspace.lsis.parsed.* (facts) and/or
│                workspace.lsis.shed.<consumer>.* (caller-supplied
│                hand-off slots) and writes its finished wire form
│                to workspace.lsis.composed.<slot>
├─ filters/      pre-parser noise filters (drop / pass-through by
│                content predicate)
└─ functions/    shared pure functions (RFC 3164 timestamp parsing,
                 protocol-name → IANA number, HTTP method →
                 activity_id)
```

The directory name is the snippet kind. `cargo xtask
lint-snippet-headers` dispatches header validation on it — the file
layout is the schema.

## LSIS — the Limpid Snippet Intermediate Schema

LSIS is the gentleman's agreement that lets independent snippets
compose: a reserved workspace namespace (`workspace.lsis.*`) whose
names carry meaning by convention. It binds the snippets in this
pack — and nothing else. Your own config may read and write whatever
it likes; the daemon neither knows nor enforces LSIS. Keep your own
state outside `workspace.lsis.*` and the two worlds never collide.
The only enforcement anywhere is this pack's own CI keeping the pack
internally consistent.

The namespace is stratified into three layers, and the layers differ
not in strictness but in the *kind* of contract each one makes.

### `workspace.lsis.parsed.*` — facts (a vocabulary contract)

What parsers established about the event: canonical OTel
`parsed.severity_number`, exact source spelling `parsed.severity`,
`parsed.time`, `parsed.device.hostname`, and friends. The contract is
a dictionary: *if* a field is present under this name, it means this
— nothing more. The vocabulary leans OCSF but is an open set
(syslog, CEF, OCSF-shaped, but not limited to), and every field is
optional; readers handle absence gracefully. Do not look for a schema
with required fields here. There isn't one, by design. Writers:
parsers. Readers: everyone.

Bundled semantic parsers write canonical `severity_number` and, when the source
provides one, exact source text in `severity`. `compose_ocsf` retains
`severity_id` only as a lower-priority compatibility input for legacy or
out-of-tree callers, and supplies OCSF's required Unknown (0) only at the output
boundary when neither a number nor source-specific severity text is available.

### `workspace.lsis.shed.*` — plumbing (a hand-off contract)

Tentative values written by glue blocks for the *next* stage: a
payload to wrap, target-specific attributes to attach. This layer has
no vocabulary of its own. Names borrow the consumer's vocabulary
(`shed.otlp.log_record.body` mirrors the OTLP proto), and each
consuming snippet's header defines the slots it eats. Meaning comes
from *who writes for whom*, not from the name globally. Values are
scoped to one hand-off; nothing under `shed.` is a fact about the
event.

### `workspace.lsis.composed.*` — products (a registry contract)

Finished wire forms, one slot per composer: `composed.ocsf`,
`composed.otlp`, `composed.rfc5424`. The slot name announces what got
produced, and the producing composer is its only writer. Egress
terminators read from here — `egress = workspace.lsis.composed.otlp`
is the whole story of shipping an event.

### Why three layers

A flat namespace forced facts, plumbing, and products to wear the
same face, and every reader had to guess which was which — that
guessing is where the old "OCSF-shaped" confusion came from. The
strata make the data's role part of its name. If you catch yourself
asking "where are the required fields?", you are in `parsed.*`
expecting a schema — it is a dictionary. If you are asking "what does
this `shed.` slot mean?", ask instead "which composer consumes it?"
— that composer's header is the contract.

### `parsed.*` vocabulary — OCSF alignment note

The `parsed.*` field names, numeric class IDs (`class_uid 3002`,
`4001`, …), and activity / status enumerations are borrowed from OCSF
1.3.0 so that `compose_ocsf` can render `parsed` to conformant OCSF
JSON without translation. This is not an OCSF conformance claim: fields
outside a class's OCSF definition are permitted (they land in
`unmapped` when rendered), and future LSIS revisions can diverge from
OCSF where wire realities demand it.

### Slot registry — composed layer

Each composer owns exactly one `composed.<slot>` output. This is the
single-writer invariant that keeps `<slot>_to_egress` terminators
unambiguous.

| Slot | Type | Writer | Purpose |
|---|---|---|---|
| `workspace.lsis.composed.ocsf` | String | `compose_ocsf` | OCSF 1.3.0 JSON, one object per event |
| `workspace.lsis.composed.rfc5424` | String | `compose_rfc5424` | single-line RFC 5424 syslog record |
| `workspace.lsis.composed.replayable` | String | `compose_replayable` | replay-shape JSONL (`{received_at, source, ingress}`) |
| `workspace.lsis.composed.otlp` | Bytes | `compose_otlp` | OTLP-1.0.0 `ResourceLogs` proto bytes |

Companion one-line processes `<slot>_to_egress` (defined in the same
file as each composer) move the slot to `egress` when the pipeline
emits that shape as its wire form.

### Shed slots — declared per consumer

The `shed.*` layer has no globally reserved sub-namespace. Each
consuming composer's header enumerates the slots it eats and the
default that applies when the caller omits the slot. Current
consumers:

| Consumer | Shed sub-tree | See header |
|---|---|---|
| `compose_otlp` | `workspace.lsis.shed.otlp.*` (resource attributes; scope name/version/attributes; log_record body/attributes/severity_text/observed-time overrides) + parsed time/severity graceful reads; observed time defaults to `received_at` | `composers/compose_otlp.limpid` |
| `compose_rfc5424` | `workspace.lsis.shed.rfc5424.*` (pri / timestamp / hostname / app_name / procid / msgid / sd / msg) | `composers/compose_rfc5424.limpid` |

`compose_ocsf` reads `parsed.*` directly. `compose_replayable` reads the ambient
event metadata and raw input (`received_at`, `source`, and `ingress`). Neither
uses shed slots. A composer adds a shed sub-tree when its target has structural
room the LSIS facts layer cannot express (OTLP target-specific attributes,
RFC 5424 fields that the pack cannot synthesise on the caller's behalf).

## What's included

The four tables below are regenerated from snippet header metadata
by `cargo xtask gen-snippet-inventory`; do not hand-edit the marked
regions.

### Parsers

<!-- BEGIN: inventory:parsers -->
<!-- Generated by `cargo xtask gen-snippet-inventory`. Do not
     edit by hand — edit the snippet headers and re-run the
     generator. See `Authoring conventions` below. -->

| File | Summary |
|---|---|
| **Transport** | |
| `parsers/parse_journald.limpid` | journalctl -o json lines → workspace.journald.* transport fields (transport-layer parser). |
| `parsers/parse_syslog.limpid` | RFC 3164 / RFC 5424 syslog wire → workspace.syslog.* transport fields (transport-layer parser; bridging the body into a vocabulary intake is the caller's job). |
| **Network firewall / IPS** | |
| `parsers/parse_asa.limpid` | Cisco ASA / FTD (ASA-syslog-compatibility) %ASA syslog messages → LSIS Authentication + Network Activity. |
| `parsers/parse_checkpoint_leef.limpid` | Check Point LEEF 2.0 (log_exporter syslog) traffic events → LSIS Network Activity. |
| `parsers/parse_checkpoint_syslog.limpid` | Check Point Syslog-Exporter records (RFC 5424 + Junos-style SD) → LSIS by action/product. |
| `parsers/parse_fortigate_cef.limpid` | Fortinet FortiGate CEF (FortiOS set format cef) → LSIS, dispatched by CEF cat subtype. |
| `parsers/parse_fortigate_syslog.limpid` | Fortinet FortiGate native key=value syslog (FortiOS "default" format) → same LSIS shape as parse_fortigate_cef. |
| `parsers/parse_juniper_srx_sd_syslog.limpid` | Juniper SRX sd-syslog (RFC 5424 + [junos@ SD block]) → LSIS by daemon / MSGID. |
| `parsers/parse_juniper_srx_syslog.limpid` | Juniper SRX unstructured syslog mode (RFC 3164 + MSGID prefix) → LSIS Detection Finding. |
| `parsers/parse_nsp.limpid` | Trellix (McAfee) Network Security Platform standard-template KV alerts → LSIS Detection Finding. |
| `parsers/parse_paloalto_cef.limpid` | Palo Alto Networks PAN-OS CEF → LSIS, dispatched by CEF name (TRAFFIC / THREAT / URL / …). |
| `parsers/parse_paloalto_syslog.limpid` | Palo Alto Networks PAN-OS native CSV syslog → LSIS, dispatched by the positional Type field. |
| **OSS NDR** | |
| `parsers/parse_suricata.limpid` | Suricata EVE JSON events → LSIS, dispatched by event_type. |
| `parsers/parse_zeek_default.limpid` | Zeek default-enabled JSON streams (conn / dns / http / ssl / files / x509 / weird / notice) → LSIS, dispatched by _path. |
| `parsers/parse_zeek_full.limpid` | Zeek full-scope extension — remaining protocol streams, drop arms for low-signal streams, and a catch-all that wraps unknown _paths into 4001/unmapped, on top of parse_zeek_soc. |
| `parsers/parse_zeek_soc.limpid` | Zeek SOC-scope extension — auth / protocol streams (ssh, smtp, ftp, dhcp, kerberos, ntlm, radius, smb_*, dce_rpc, snmp, rdp) on top of parse_zeek_default. |
| **Cloud audit (API control plane)** | |
| `parsers/parse_azure_activity.limpid` | Azure Activity Log events JSON → LSIS API Activity. |
| `parsers/parse_cloudtrail.limpid` | AWS CloudTrail JSON events → LSIS API Activity. |
| **Cloud network (data plane)** | |
| `parsers/parse_aws_vpc_flow.limpid` | AWS VPC Flow Logs text records (v2 default + v5 custom formats) → LSIS Network Activity. |
| **Cloud security findings** | |
| `parsers/parse_aws_guardduty.limpid` | AWS GuardDuty findings JSON → LSIS Detection Finding. |
| **Identity / IdP** | |
| `parsers/parse_okta_system.limpid` | Okta System Log events JSON (System Log API v1) → LSIS identity classes, dispatched by eventType. |
| **EDR** | |
| `parsers/parse_sysmon.limpid` | Microsoft Sysmon Windows-event JSON (forwarder-shipped) → LSIS, dispatched by EventID. |
| **Container / orchestration** | |
| `parsers/parse_k8s_audit.limpid` | Kubernetes audit Event JSON (audit.k8s.io/v1) → LSIS API Activity + Authentication. |
| **Web / proxy access** | |
| `parsers/parse_combined_log.limpid` | Apache / Nginx combined log format → LSIS HTTP Activity. |
| **Mail (MTA)** | |
| `parsers/parse_postfix.limpid` | Postfix mail-flow log bodies (postfix/<program>[pid]: tag included) → LSIS Email Activity. |
| **DNS server** | |
| `parsers/parse_bind.limpid` | ISC BIND 9 querylog lines → LSIS DNS Activity. |
| **Endpoint / host audit (Unix)** | |
| `parsers/parse_auditd.limpid` | Linux kernel audit records (auditd / audispd / auditbeat wire text) → LSIS, dispatched by record type. |
| `parsers/parse_openssh.limpid` | OpenSSH sshd application bodies (post-transport-unwrap) → LSIS Authentication. |
| `parsers/parse_sudo.limpid` | sudo / sudoedit application bodies (post-transport-unwrap) → LSIS Authorize Session. |
| **Endpoint / host audit (Windows)** | |
| `parsers/parse_winevent_json.limpid` | Windows Event Log JSON (NXLog field-naming shape; Security channel) → LSIS, dispatched by EventID. |
| **Vendor-neutral** | |
| `parsers/parse_ocsf.limpid` | Parses OCSF JSON into LSIS, normalizing root time to epoch nanoseconds and severity_id to OTel SeverityNumber. |
<!-- END: inventory:parsers -->

### Composers

<!-- BEGIN: inventory:composers -->
<!-- Generated by `cargo xtask gen-snippet-inventory`. Do not
     edit by hand — edit the snippet headers and re-run the
     generator. See `Authoring conventions` below. -->

| File | Summary | Writes |
|---|---|---|
| `composers/compose_ocsf.limpid` | Renders the LSIS intermediate to OCSF 1.3.0 JSON (27-class priority set), dispatched by class_uid. | `workspace.lsis.composed.ocsf` |
| `composers/compose_otlp.limpid` | Assembles one OTLP-1.0.0 ResourceLogs envelope from canonical LSIS scalars and source-adapter shed slots. Emits protobuf bytes for both otlp_http and otlp_grpc. | `workspace.lsis.composed.otlp` |
| `composers/compose_replayable.limpid` | Serialises the event into the minimal replay-record shape `{ received_at, source, ingress }` — the same three fields the error_log (DLQ) preserves and `inject --json` consumes. | `workspace.lsis.composed.replayable` |
| `composers/compose_rfc5424.limpid` | Assembles a single-line RFC 5424 syslog record from per-field shed slots (https://datatracker.ietf.org/doc/html/rfc5424). | `workspace.lsis.composed.rfc5424` |
<!-- END: inventory:composers -->

### Filters

<!-- BEGIN: inventory:filters -->
<!-- Generated by `cargo xtask gen-snippet-inventory`. Do not
     edit by hand — edit the snippet headers and re-run the
     generator. See `Authoring conventions` below. -->

| File | Summary |
|---|---|
| `filters/filter_openssh_journal.limpid` | Strip PAM-stack noise from a journal-sourced sshd stream ahead of parse_openssh. |
<!-- END: inventory:filters -->

### Functions

<!-- BEGIN: inventory:functions -->
<!-- Generated by `cargo xtask gen-snippet-inventory`. Do not
     edit by hand — edit the snippet headers and re-run the
     generator. See `Authoring conventions` below. -->

| File | Signature | Used by |
|---|---|---|
| `functions/http_method_activity_id.limpid` | `http_method_activity_id(method) → Int` | `parse_combined_log`, `parse_suricata`, `parse_zeek_default` |
| `functions/parse_datetime_rfc3164.limpid` | `parse_datetime_rfc3164(text) → Timestamp` | — |
| `functions/proto_num.limpid` | `proto_num(name) → Int \| null` | `parse_checkpoint_leef`, `parse_checkpoint_syslog`, `parse_juniper_srx_sd_syslog`, `parse_juniper_srx_syslog`, `parse_paloalto_cef`, `parse_paloalto_syslog`, `parse_suricata`, `parse_sysmon`, `parse_zeek_default`, `parse_zeek_full` |
| `functions/severity_converter.limpid` | `ocsf_severity_id_to_otel_severity_number(severity_id) → Int \| null` | `parse_ocsf` |
| `functions/severity_converter.limpid` | `otel_severity_number_to_ocsf_severity_id(severity_number) → Int \| null` | `compose_ocsf` |
| `functions/timestamp_converter.limpid` | `timestamp_ns_to_ms(value) → Int \| null` | `compose_ocsf` |
| `functions/timestamp_converter.limpid` | `timestamp_ms_to_ns(value) → Int \| null` | `parse_ocsf` |
<!-- END: inventory:functions -->

### Filebeat-flat JSON: `nest_dotted_keys` primitive

Some upstreams (Filebeat / Logstash JSON emitters used by zeek and
suricata modules, certain Splunk HEC sources, OpenSearch ingest
pipelines) flatten nested JSON for Elasticsearch indexing
conventions: `{"id": {"orig_h": "1.1.1.1"}}` becomes
`{"id.orig_h": "1.1.1.1"}`. limpid DSL does not expose
bracket-subscript access (`body["id.orig_h"]`) by design, so
dotted keys are unreachable from a parser without normalising.

The Rust primitive `nest_dotted_keys(obj)` recursively un-flattens
dotted keys back into nested Objects, loud-fail on collisions. The
Zeek `_flat` convenience entry points use it internally; for any
other Filebeat-flattened wire, wrap the parse step explicitly:

```limpid
process {
    workspace.foo = {
        body:     nest_dotted_keys(parse_json(ingress)),
        hostname: hostname(),
        time:     to_int(received_at)
    }
} | parse_foo | compose_ocsf | ocsf_to_egress
```

## Quick start

Drop a snippet into your `/etc/limpid/limpid.conf`:

```limpid
include "/usr/share/limpid/snippets/parsers/parse_fortigate_cef.limpid"
include "/usr/share/limpid/snippets/composers/compose_ocsf.limpid"

def input fw_syslog {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output ocsf_stdout {
    type stdout
}

def pipeline fw_to_ocsf {
    input fw_syslog
    process parse_fortigate_cef | compose_ocsf | ocsf_to_egress
    output ocsf_stdout
}
```

That's it. The parser writes facts to `workspace.lsis.parsed.*`;
`compose_ocsf` reads `workspace.lsis.parsed.*` and writes OCSF JSON
to `workspace.lsis.composed.ocsf`; the one-line `ocsf_to_egress`
step at the tail of the pipeline hands that slot off to `egress`.
Replace `ocsf_stdout` with an output that accepts OCSF JSON and you're shipping
to your SIEM or data lake. An OTLP output requires the envelope-wrapped pipeline
shown below; OCSF JSON cannot be connected to `otlp_http` or `otlp_grpc`
directly.

## Design principles

The library follows four contracts:

1. **LSIS strata (`parsed` / `shed` / `composed`).** Parsers populate
   `workspace.lsis.parsed.*` with facts about the event; composers
   read `parsed.*` (facts) and optionally `shed.*` (caller-supplied
   hand-off slots) and write finished wire forms to
   `workspace.lsis.composed.<slot>`. See the LSIS section above for
   the layer contracts and the shed / composed slot registries.
   Transport-namespace intermediates (`workspace.cef`,
   `workspace.syslog`, `workspace.journald`, …) live outside LSIS
   by design; pack composers do not read them directly.

2. **Loud-fail-fast on unsupported vocabulary.** Each parser's
   dispatcher routes events with shapes / subtypes / message IDs
   the snippet does not handle to `error_log` (DLQ) via the `error`
   keyword, with an operator-readable message. Silent zero-mapping
   is forbidden — if a vendor adds a field or a new subtype, the
   operator sees it in the DLQ on day one and decides whether to
   extend the snippet or update the upstream allow-list.

3. **egress single-writer invariant.** A composer never assigns
   `egress` directly. Composers write to `workspace.lsis.composed.
   <slot>`; the companion `<slot>_to_egress` one-line process is the
   sole writer of `egress` per pipeline. `grep 'egress = '
   packaging/snippets/` names the writer for every terminal.

4. **Bridges belong to the consumer.** When a composer reads
   something other than its plain shed vocabulary — a transport
   namespace, another composer's `composed` product, an ad-hoc
   caller value — the reader that decided to reach outside pays the
   cost. Named bridges (e.g. `journald_to_rfc5424`) live in the
   consuming composer's file, not in a shared bridges/ directory;
   ad-hoc glue blocks in the pipeline (`{ workspace.lsis.shed.otlp.
   log_record.body = workspace.lsis.composed.ocsf }`) do the same
   job when the mapping is one line.

## Pipeline shapes

Schema wire form:

```
process <vendor_parser> | compose_ocsf | ocsf_to_egress
```

Envelope-wrapped schema (OTLP wrapping the OCSF JSON as the log
body):

```
process <vendor_parser>
      | compose_ocsf
      | {
          workspace.lsis.shed.otlp.log_record.body =
              workspace.lsis.composed.ocsf
        }
      | compose_otlp
      | otlp_to_egress
```

Target-specific attributes (e.g. Azure Monitor Pipeline's
CommonSecurityLog columns) go in the same glue block via
`workspace.lsis.shed.otlp.log_record.attributes = [ ... ]`; see the
`compose_otlp` header for the full example.

For mixed-vendor / mixed-format inputs, dispatch upstream of the
parser with a `switch contains(ingress, "...")` block, calling the
appropriate parser per branch.

## Authoring conventions

Every snippet header carries a canonical key set determined by the
file's parent directory:

| Kind | Directory | Required keys (canonical order) |
|---|---|---|
| parser | `parsers/` | `Summary`, `Reads`, `Writes`, `Category`, `Test corpus` |
| composer | `composers/` | `Summary`, `Reads`, `Writes`, `Test corpus` |
| filter | `filters/` | `Summary`, `Reads`, `Effect`, `Test corpus` |
| function | `functions/` | `Summary`, `Signature`, `Test corpus` |

**Governing principle.** A header holds only knowledge the author
alone knows. Anything derivable from other keys, other files, or
the body is banned from the header and surfaces in the generated
inventory instead — that is why parsers carry `Category` but
composers / filters do not (their axis would duplicate
Reads/Writes/Effect), and why `Used by:` for a function is derived
by the inventory generator rather than authored.

### `Reads:` — universal stream-contract grammar

Every kind that flows events (parser / composer / filter) declares
its input contract on the `Reads:` value. The first token of the
first line names the stream source:

- **Raw wire.** First token `ingress` — the snippet reads bytes off
  the wire. Dot-line intake rows are forbidden.

  ```
  // Reads:       ingress (raw wire) — syslog-wrapped %ASA messages
  ```

- **Bridge / reader.** First token `workspace.<ns>.*` — the snippet
  reads a workspace namespace populated by an upstream process. At
  least one dot-line intake row is required per intake field:

  ```
  // Reads:       workspace.openssh.* (bridge — see Bridges below)
  //                .body       (required, String)  — sshd body
  //                .pid        (optional, String)  — process id
  //                .hostname   (optional, String)  — SSH server host
  //                .time       (optional, Int)     — epoch nanoseconds
  ```

  Each dot-line matches the regex
  `^\.<IDENT>\s+\((required|optional), <String|Int|Float|Bool|Object|Array|Timestamp>\)`.
  Trailing prose after the closing paren is permitted. Leading
  underscores in the identifier are permitted (journald's `_PID` /
  `__REALTIME_TIMESTAMP` are real intake fields).

Ambient event metadata (`received_at`, `source`, `ingress` when
passed through unchanged, `hostname()`) is not declared in `Reads:`
— every process can read those unconditionally.

### `Writes:` — LSIS-slot contract

`Writes:` names the LSIS slot(s) the snippet produces. Parsers
write facts to `workspace.lsis.parsed.*` and enumerate the OCSF
class(es) their dispatcher emits; composers write a single
`workspace.lsis.composed.<slot>` from the composed-layer registry
above.

### `Category:` — parser-only, closed vocabulary

Parsers pick a `Category:` value from the 17-entry whitelist in
`crates/xtask/src/inventory.rs::CATEGORIES`. The inventory table
groups rows by this key. To add a category, extend the slice and
document the addition here.

Composers and filters have no `Category:` axis — the LSIS slot the
composer writes and the drop / pass predicate the filter enforces
are already visible on their `Writes:` / `Effect:` keys.

### `Test corpus:` — provenance vocabulary

The value's first token is a fixed prefix that describes the
provenance of the corpus a snippet was verified against:

| Kind | Allowed prefixes |
|---|---|
| parser / composer / filter | `real` (author-captured), `public` (published dataset — FLAWS, OTRF/Mordor, elastic/integrations, …), `synthetic` (author-composed samples), `spec-only` (vendor spec / schema reference, no captured corpus) |
| function | `unit` (verified against the referenced authoritative source in parentheses) |

Environment labels (deployment names, host aliases, internal
codenames) are not permitted — corpus provenance describes what the
corpus IS, not where the author observed it.

### Free-form prose below the canonical keys

Every free-form block that carries authored knowledge — wire shape
details, per-leaf dispatch tables, sample inputs, bridge pipeline
examples, security notes, vendor-scoping caveats — is preserved
below the canonical keys as plain comment prose. To keep the
header tokenizer's key-matching honest (and the lint's unknown-key
warning meaningful), free-form labels use an em-dash separator
rather than a colon:

```
// Wire —
//   RFC 5424 syslog + Junos-style structured-data block
//
// Bridges —
//   process parse_syslog | { workspace.openssh = { body: ...
//                                                  hostname: ...
//                                                  time: ... } }
//         | parse_openssh
```

Colon-terminated labels ARE the reserved key set — a stray
`// Notes:` line will trip the lint's unknown-key warning, which is
the intended signal.

### Verifying a header

`cargo xtask lint-snippet-headers` runs the seven guardrails
(canonical key order, Category whitelist, Test corpus prefix,
Reads dot-line grammar, function Signature ↔ `def function`
cross-check, Summary presence, unknown-key warning). Steady state
is `0 errors, 0 warnings`.

`cargo xtask gen-snippet-inventory --check` verifies the four
inventory blocks above are in sync with current headers — CI uses
this mode to fail on drift.

### Body conventions

- Each `def process` body is single-responsibility (header parse,
  dispatch, per-leaf record build); the dispatcher handles
  unsupported vocabulary with `error "<operator-readable msg>"`.
- Helpers (`def function ...`) carry their per-vendor mapping tables
  (severity → LSIS `severity_number`, action → `activity_id`, etc.).
- Files are one per (vendor, format). FortiGate has two files
  (`parse_fortigate_cef` + `parse_fortigate_syslog`) because CEF and
  native KV are different wire shapes; OpenSSH is one file because
  sshd's wire is one shape across syslog and journald.
- Sample data in `//` prose is anonymised to RFC 5321 / 5737 forms
  (`example.com`, `192.0.2.x`, `198.51.100.x`).

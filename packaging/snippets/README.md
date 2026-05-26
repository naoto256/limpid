# limpid Snippet Library

The official snippet pack shipped with the `limpid` package, installed
read-only under `/usr/share/limpid/snippets/`. User configurations
reference snippets by absolute path; the config loader's allow-list
(`SYSTEM_SNIPPET_DIR` in `config.rs`) explicitly permits this prefix.

This README is the canonical document for the pack: it describes
*what* is included, *how* to use it, and — equally important — *the
design contracts the pack adopts*. The pack is not the only possible
snippet distribution: limpid itself is agnostic about who supplies
snippets, and an independent author can ship a different pack with
different conventions. If you do, the contracts below are the
reference point you can choose to follow or diverge from.

## Layout

```
/usr/share/limpid/snippets/
├─ parsers/      per-vendor / per-format parsers writing to
│                workspace.limpid.* (the parser ↔ composer
│                intermediate language; see Design principle 1)
├─ composers/    target-schema composers reading from
│                workspace.limpid.* (currently OCSF 1.3.0; also
│                the replay-shape composer for parser regression
│                capture and the RFC 5424 re-framer)
├─ filters/      noise filters (currently: drop-by-content
│                predicate; applied pre-parser or between
│                transport and body parsers)
└─ functions/    shared helpers used by ≥3 parsers (Rule of Three;
                 one-off mappings stay inline)
```

## What's included

### Parsers

<!-- BEGIN: inventory:parsers -->
<!-- Hand-maintained until the xtask inventory generator lands.
     When that lands, this block becomes generated output and must
     not be edited by hand — see `Authoring conventions` below. -->

For each parser's full output footprint (which `workspace.limpid`
fields are populated, which OCSF classes — if any — are targeted,
any non-OCSF extensions) see the `Output:` header in the file
itself. The inventory below is an overview of *what* sources are
covered, not *what they emit*.

| File | Source |
|---|---|
| **Transport** | |
| `parsers/parse_syslog.limpid` | RFC 3164 / 5424 syslog (transport, populates `workspace.syslog.*`) |
| `parsers/parse_journald.limpid` | systemd journald JSON (transport, populates `workspace.journald.*`) |
| **Security devices / cloud audit** | |
| `parsers/parse_fortigate_cef.limpid` | FortiGate (CEF wrap) |
| `parsers/parse_fortigate_syslog.limpid` | FortiGate (native KV syslog) |
| `parsers/parse_paloalto_cef.limpid` | PAN-OS (CEF wrap) |
| `parsers/parse_paloalto_syslog.limpid` | PAN-OS (native CSV syslog) |
| `parsers/parse_asa.limpid` | Cisco ASA / FTD-in-ASA-mode (syslog) |
| `parsers/parse_cloudtrail.limpid` | AWS CloudTrail (JSON) |
| `parsers/parse_juniper_srx_sd_syslog.limpid` | Juniper SRX RT_FLOW (RFC 5424 + Junos SD) |
| `parsers/parse_juniper_srx_syslog.limpid` | Juniper SRX RT_IDP / IDP_ATTACK_LOG_EVENT (RFC 3164 unstructured) |
| `parsers/parse_nsp.limpid` | Trellix / McAfee Network Security Platform IPS alerts |
| `parsers/parse_checkpoint_leef.limpid` | Check Point LEEF 2.0 (Accept / Drop / Reject / Block) |
| `parsers/parse_checkpoint_syslog.limpid` | Check Point Syslog Exporter (Junos-style SD; also R81+ `=` variant) |
| **OSS NDR** | |
| `parsers/parse_suricata.limpid` | OISF Suricata EVE JSON (alert / dns / http / flow / tls / fileinfo) |
| `parsers/parse_zeek_default.limpid` | Zeek default-enabled scripts + `_native` / `_flat` entry points |
| `parsers/parse_zeek_soc.limpid` | Transitively includes default + auth / SMB / DCE-RPC / SNMP / RDP / DHCP |
| `parsers/parse_zeek_full.limpid` | Transitively includes soc + remaining specialised scripts + catch-all for unknown `_path` |
| **Server / host vocabulary** | |
| `parsers/parse_openssh.limpid` | OpenSSH `sshd` body (transport-agnostic; bridge from `parse_syslog` or `parse_journald`) |
| `parsers/parse_sudo.limpid` | sudo (syslog / journald) |
| `parsers/parse_combined_log.limpid` | Apache / Nginx access log (combined format) |
| `parsers/parse_postfix.limpid` | Postfix MTA (syslog) |
| `parsers/parse_winevent_json.limpid` | Windows Security event log (NXLog / Vector / Winlogbeat JSON) |
| `parsers/parse_sysmon.limpid` | Microsoft Sysmon — EventID 1 / 3 / 11 |
| `parsers/parse_bind.limpid` | ISC BIND 9 querylog |
| `parsers/parse_auditd.limpid` | Linux auditd (~45 type codes across multiple OCSF classes) |
| **Vendor-neutral** | |
| `parsers/parse_ocsf.limpid` | OCSF JSON inbound (any vendor's prior `compose_ocsf` output; passthrough) |

<!-- END: inventory:parsers -->

### Composers

<!-- BEGIN: inventory:composers -->
- `composers/compose_ocsf.limpid` — dispatches by
  `workspace.limpid.class_uid` to per-class leaves, covering the
  OCSF 1.3.0 priority set (27 classes). Each leaf strips `null`
  keys via `null_omit` and writes OCSF JSON to `egress`.
- `composers/compose_rfc5424.limpid` — `workspace.journald.*` →
  RFC 5424 syslog wire. Used at edge boxes to re-frame journald
  entries for syslog relay (e.g. edge → relay → SIEM ingest).
- `composers/compose_replayable.limpid` — minimal `{received_at,
  source, ingress}` shape that round-trips through `inject --json`
  for parser regression / replay capture. Use it on a fan-out
  branch to record the raw wire while a parallel branch parses,
  so a parser bug discovered later can be fixed and the saved
  JSONL replayed offline.
<!-- END: inventory:composers -->

### Filters

<!-- BEGIN: inventory:filters -->
- `filters/filter_openssh_journal.limpid` — drops
  `pam_unix(sshd:session): session opened/closed` PAM noise from
  journald-sourced sshd streams (run between `parse_journald` and
  `parse_openssh`). sshd itself emits the authentication fact via
  `Accepted ...` / `Disconnected ...`; the PAM duplicate would
  double-count.
<!-- END: inventory:filters -->

### Functions

<!-- BEGIN: inventory:functions -->
- `functions/proto_num.limpid` — `proto_num(name) → Int | null`.
  IANA protocol number lookup (tcp / udp / icmp / icmpv6 / sctp /
  gre / esp / ah). Case-insensitive. Used by every parser
  emitting `connection_info.protocol_num` on an OCSF 4001 record.
- `functions/http_method_activity_id.limpid` —
  `http_method_activity_id(method) → Int`. HTTP request method →
  OCSF 4002 `activity_id` (spec-standard mapping). Used by
  `parse_suricata` / `parse_zeek_default` / `parse_combined_log`
  and any future HTTP-emitting parser.
- `functions/parse_datetime_rfc3164.limpid` —
  `parse_datetime_rfc3164(text) → Timestamp`. The LPL counterpart
  to the built-in `parse_datetime_rfc3339` primitive. Shipped as
  LPL because RFC 3164 wire carries neither year nor timezone, so
  the parser has to encode policy (current-year + future-clamp +
  UTC assumption) that operators should be able to edit. For
  RFC 5424 / OTLP / OCSF input use the built-in
  `parse_datetime_rfc3339(text)` primitive directly.
<!-- END: inventory:functions -->

### `nest_dotted_keys` primitive

Some upstreams (Filebeat / Logstash JSON emitters used by Zeek and
Suricata modules, certain Splunk HEC sources, OpenSearch ingest
pipelines) flatten nested JSON for Elasticsearch indexing:
`{"id": {"orig_h": "1.1.1.1"}}` becomes `{"id.orig_h": "1.1.1.1"}`.
limpid DSL does not expose bracket-subscript access
(`body["id.orig_h"]`) by design, so dotted keys are unreachable
from a parser without normalising.

The Rust primitive `nest_dotted_keys(obj)` recursively un-flattens
dotted keys back into nested Objects, loud-failing on collisions
and bounded against pathological inputs. The Zeek `_flat`
convenience entry points use it internally; for any other
Filebeat-flattened wire, wrap the parse step explicitly:

```limpid
process {
    workspace.foo = {
        body:     nest_dotted_keys(parse_json(ingress)),
        hostname: hostname(),
        time:     to_int(received_at)
    }
} | parse_foo | compose_ocsf
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

def output security_lake {
    type ...           // your destination
}

def pipeline fw_to_security_lake {
    input fw_syslog
    process parse_fortigate_cef | compose_ocsf
    output security_lake
}
```

The parser writes to `workspace.limpid.*` (the shared
intermediate language); the composer reads from
`workspace.limpid.*` and writes OCSF JSON to `egress`. Swap the
parser for any of the others; chain a filter ahead of the parser
to drop noise. To capture replay-shape, add `compose_replayable`
on a parallel fan-out branch — it reads `ingress`, not
`workspace.limpid`, so it cannot replace `compose_ocsf` as a
drop-in.

For mixed-vendor / mixed-format inputs, dispatch upstream of the
parser with a `switch contains(ingress, "...")` block, calling the
appropriate parser per branch.

## Pipeline shape

Parsers receive raw events on `ingress` (or a body extracted by an
upstream transport parser) and produce intermediate-language values on
`workspace.limpid.*`. The typical pipeline is two stages:

```
process <vendor_parser> | compose_ocsf
```

Transport-agnostic body parsers (`parse_openssh`, `parse_sudo`,
`parse_postfix`) require an intake bridge from the transport
parser before they can run — see *Design principle 4* below.

## Design principles

The pack adopts seven contracts. The first two define the parser
↔ composer separation; the remaining five govern how the pack
stays coherent as parsers are added. An independent snippet pack
can pick a different set; these are the conventions *this* pack
follows.

### 1. `workspace.limpid` is the shared parser ↔ composer intermediate language

Parsers and composers do not map M-to-N directly — that
combinatorial cost is what an intermediate language exists to
avoid. The pack adopts `workspace.limpid.*` as the single meeting
point: parsers write once, composers select.

Designing a bespoke intermediate schema from scratch is not worth
the cost, so the pack **borrows OCSF 1.3.0 as the reference
taxonomy** — `class_uid` values are OCSF UIDs, field names follow
OCSF naming where the source carries a concept OCSF models. **This
is a gentlemen's agreement, not a strict schema**:

- A parser aims to stay close to OCSF, but may add fields OCSF
  does not model (source-specific data the parser wants downstream
  to be able to surface) and may omit fields OCSF defines (the
  source does not carry them).
- A composer applies its own selection policy.
  `compose_ocsf` keeps OCSF-compatible fields, runs `null_omit`,
  and writes valid OCSF 1.3.0 JSON; non-OCSF fields are dropped.
  A different composer (third-party, or future first-party for
  OTLP / ECS / domain-specific schemas) can take a different
  policy and surface what `compose_ocsf` discards.
- The convention is documented (this README, per-parser `Output:`
  headers) but not mechanically enforced. In practice,
  parsers in this pack stay close to OCSF because `compose_ocsf`
  is the primary downstream; the room for divergence is preserved
  for future composers.

Vendor intermediates (`workspace.cef`, `workspace.syslog`,
`workspace.pf`, `workspace.ct`, `workspace.winevent`, etc.) remain
parser-private (Design principle 3).

**Scope and escape hatch.** This principle governs
**target-schema composers** — composers that read from
`workspace.limpid` to emit a structured downstream schema
(`compose_ocsf` today; conceptually any future OTLP / ECS /
domain-specific composer). It does not govern **utility
composers** that operate outside the intermediate language by design.
`compose_replayable` is the canonical example: it captures
`{received_at, source, ingress}` for replay / regression capture
and intentionally does not read `workspace.limpid` (the whole
point is to preserve the raw wire before parsing). Treat utility
composers as an escape hatch, not a counter-example to the
principle.

The contract is discussed in further depth in
[`docs/src/processing/user-defined.md`](../../docs/src/processing/user-defined.md).

### 2. Loud-fail-fast on unsupported vocabulary

Each parser's dispatcher routes events with shapes / subtypes /
message IDs the snippet does not handle to the configured
`error_log` (DLQ) via the `error` keyword, with an
operator-readable message. Silent zero-mapping is forbidden — if a
vendor adds a field or a new subtype, the operator sees it in the
DLQ on day one and decides whether to extend the snippet or
update the upstream allow-list.

Error message format: `parse_<vendor>: <reason>: <evidence>`. The
evidence portion quotes the unhandled token verbatim so the
operator can grep the corpus.

The DLQ entries are JSONL via `control { error_log "..." }`;
without that, errors fall back to a structured `tracing::error!`
line. Configure the error log path explicitly so
unsupported-vocabulary events don't silently scroll off journald.

### 3. Vendor namespaces: one per (vendor, format-family); mostly parser-private, sometimes a bridge

Parsers stage intermediate values on `workspace.<vendor>.*` (e.g.
`workspace.cef`, `workspace.syslog`, `workspace.openssh`,
`workspace.pf`, `workspace.ct`, `workspace.winevent`). The
convention is **one namespace per (vendor, format-family)** —
avoid colliding generic names like `workspace.body` or
`workspace.event`, which would break composability when multiple
parsers share a pipeline.

Two read patterns coexist:

- **Parser-private** (the common case). The namespace is scratch
  space for one parser only. `workspace.cef` is internal to the
  CEF-wire parsers (`parse_fortigate_cef`, `parse_paloalto_cef`,
  …); each writes and then reads back its own fields during
  dispatch. No other parser, and no composer, reads it.
- **Transport-bridge schema** (the body-parser case, sanctioned
  by Design principle 4). The namespace is the intake contract
  between an upstream transport parser and a body parser — the
  pipeline writer populates it, the body parser reads it.
  `workspace.openssh.{body, pid, hostname, time}` is the
  canonical example. These bridge namespaces are the **only**
  sanctioned cross-parser uses of vendor namespaces.

Cross-composer communication still goes exclusively through
`workspace.limpid.*` (Design principle 1) — bridge namespaces are
parser-to-parser, never parser-to-composer.

### 4. Transport-agnostic body parsers declare a fixed intake schema

Body parsers (`parse_openssh`, `parse_sudo`, `parse_postfix`) do
not parse `ingress` directly — they re-parse a body extracted by
an upstream transport parser. To stay portable across syslog /
journald / file ingest, each such parser declares a fixed
`workspace.<vendor>.{body, pid, hostname, time}` intake schema in
its header docstring, and the pipeline writer is responsible for
bridging the transport into that schema.

Standard intake keys:

| Key | Type | Required | Source guidance |
|---|---|---|---|
| `body` | String | yes | The application-level message text, post-transport-unwrap |
| `pid` | String | no | Prefer kernel-verified sources (journald `_PID` over `SYSLOG_PID`) |
| `hostname` | String | no | Trusted-source hostname; journald `_HOSTNAME` is kernel-verified |
| `time` | Int (epoch ns) | no | Convert at the bridge; journald `__REALTIME_TIMESTAMP` is µs (× 1000) |

See `parsers/parse_openssh.limpid` for a worked example of both
the syslog and journald bridges.

### 5. Shared helpers extracted on Rule of Three

Mapping logic used by **≥3 parsers** is extracted to `functions/`
as a top-level helper (current: `proto_num`,
`http_method_activity_id`). Logic used by 1 or 2 parsers stays
inline as `def function` inside the parser file, keeping the
parser self-contained for review.

Premature extraction is the more common mistake than late
extraction — 2 callsites are a duplicate, 3 are a pattern. Wait
for the third caller before lifting.

### 6. NOTE-flagged subtypes mark spec-derived but unverified mappings

When a parser handles a subtype based on vendor documentation but
has not yet been exercised against live data, the mapping is
flagged with a `// NOTE` comment immediately above the
case-branch. The header docstring repeats the list under a
`NOTE-flagged subtypes:` block so operators auditing the parser
before production rollout can see them in one place.

Verify NOTE-flagged subtypes against a representative corpus
before relying on them in production. Once verified, drop the
NOTE and update the header.

### 7. Test corpus discipline: real / public / synthetic / spec-only

Every parser declares its verification basis in its header under
`Test corpus:`. Four values, in descending order of confidence:

- **`real`** — production or lab traffic the contributor controls
  (e.g. `parse_paloalto_*`: live PA-460 in tap mode).
- **`public`** — a published dataset
  (e.g. `parse_cloudtrail`: FLAWS; `parse_winevent_json`:
  OTRF Mordor).
- **`synthetic`** — a generated corpus
  (e.g. `parse_asa`: miroslav-siklosi/Syslog-Generator).
- **`spec-only`** — vendor documentation only, no live
  verification. Such parsers are functional but every subtype is
  effectively NOTE-flagged.

The value includes corpus size, source, and per-shape parse-rate
where measured. Loud-fail-fast (principle 2) and Test corpus
together give operators a concrete answer to "how confident
should I be in this mapping?" — without this, the failure-mode
asymmetry of silent mis-mapping is invisible.

## Authoring conventions

If you contribute a snippet — to this pack or to a separate one
that follows these contracts — the following conventions apply.

### File naming and scope

One file per `(vendor, format)`. FortiGate has two files
(`parse_fortigate_cef` + `parse_fortigate_syslog`) because CEF and
native KV are different wire shapes that require different
parsers. OpenSSH is one file because sshd's wire is one shape
across syslog and journald.

### Header schema

Every parser file opens with a comment header that follows a fixed
schema. The xtask inventory generator (planned for the 0.7.4 line)
will parse these headers to produce the `What's included` tables
in this README and to lint for missing or malformed keys.

| Key | Required | Value format |
|---|---|---|
| `Vendor:` | yes | Canonical product name, optionally with umbrella in parens (`FortiGate (Fortinet)`, `ASA (Cisco)`) |
| `Wire:` | yes | Wire format + wrapper assumptions (`RFC 3164 syslog`, `syslog-wrapped CEF`, `JSON`, etc.) |
| `Upstream:` | yes | Either `ingress (raw wire)` for parsers reading `ingress` directly, or `parse_<transport> \| parse_<transport2>` listing transport parser(s) that produce the intake |
| `Intake:` | when `Upstream` is not `ingress (raw wire)` | Multi-line block declaring required and optional `workspace.<vendor>.<key>` entries with type and source guidance |
| `Output:` | yes | Prose description of what the parser writes to its primary output namespace. For IL-targeting parsers (the common case), name the `workspace.limpid.*` fields populated, the OCSF class UIDs targeted where applicable, and any non-OCSF extension fields (Design principle 1 allows divergence from OCSF for source-specific data). For transport parsers, name the vendor-private namespace written (e.g. `workspace.syslog.*` / `workspace.journald.*`) and the key shape produced. Presence-checked only; content is free-form. |
| `Test corpus:` | yes | One of `real` / `public` / `synthetic` / `spec-only`, followed by source, size, and per-shape parse-rate in parens |

Conventions:

- **Order**: keys appear in the order listed above. The generator
  will emit diagnostics on order violations (once the xtask
  lands) to keep diffs stable.
- **Continuation**: a value may span multiple lines; continuation
  lines start with `//` followed by ≥2 spaces of indentation.
- **Unknown keys**: tolerated with a generator warning. Use them
  for vendor-specific context (e.g. `FortiGate CEF dialect quirks:`)
  that does not fit the canonical keys.
- **Sample lines**: optional free-form `// Sample:` blocks may
  follow the schema block. Anonymise to RFC 5321 / 5737 forms
  (`example.com`, `192.0.2.x`, `198.51.100.x`).

A worked header in canonical form:

```
// OpenSSH sshd parser (vocabulary)
//
// Vendor:      OpenSSH
// Wire:        sshd application body, post-transport-unwrap
// Upstream:    parse_syslog | parse_journald (transport-agnostic body parser)
// Intake:      workspace.openssh.body       (required, String) — sshd body
//              workspace.openssh.pid        (optional, String) — process id as string
//              workspace.openssh.hostname   (optional, String) — SSH server hostname
//              workspace.openssh.time       (optional, Int)    — epoch nanoseconds
// Output:      OCSF 3002 (Authentication) on workspace.limpid:
//              activity_id (1=login attempt / 2=session teardown /
//              99=other (preauth / kex / banner errors)),
//              actor.user.name, src_endpoint.ip / port, auth_protocol="SSH",
//              status_id, status_detail. No non-OCSF extension fields.
// Test corpus: real (playground sshd + journald feed of internet-facing sshd;
//              additionally cross-checked against logpai/loghub OpenSSH_2k.log)
```

### Two-tier dispatch

The top-level `def process parse_<vendor>` is responsible for:

1. Stripping the wrapper (CEF header, syslog SD block, JSON
   envelope, etc.) and staging the parsed fields on the
   vendor-private workspace namespace.
2. Routing by a stable header field (e.g. `cat=<category>:<subtype>`
   for FortiGate CEF, `event_type` for Suricata EVE, `_path` for
   Zeek conn/dns/http logs) to a per-leaf `def process`.
3. Routing unrecognised values to `error` with the canonical
   message format.

Per-leaf `def process` units re-parse the body against the
subtype-specific shape and write the OCSF record to
`workspace.limpid.*`. Each leaf is single-responsibility — header
parse, dispatch, and per-leaf record build are three different
units.

### Helper placement

- **Inline** (`def function` within the parser file): vendor
  severity → OCSF `severity_id`, vendor action → OCSF `activity_id`,
  and any other mapping table that is parser-private. Keep the
  parser self-contained for review.
- **Shared** (`functions/` directory): logic used by ≥3 parsers
  (principle 5). Moving a helper from inline to shared is a
  refactor that lands in its own commit, after the third caller
  arrives.

### See also

- [`docs/src/snippets/README.md`](../../docs/src/snippets/README.md)
  — concept-level introduction to snippets in the limpid engine
  (linked here as the user-facing entry point).
- [`docs/src/processing/user-defined.md`](../../docs/src/processing/user-defined.md)
  — the DSL-side reference for `def process`, `def function`,
  workspace shapes, and the `error` keyword.

# Snippet Library

A maintained set of vendor parsers and target-schema composers,
shipped with the `limpid` package and installed under
`/usr/share/limpid/snippets/`. Operators get vendor logs into a
SIEM / data lake in OCSF form by including the parser, its declared
helper dependencies, and the target composer. No parser has to be
written from scratch, and changing DSL does not require recompiling
the daemon. Copy an installed snippet into an operator-owned path
before customising it; package upgrades replace files under
`/usr/share/limpid/snippets/`.

> **Status:** the snippet library was introduced in v0.7.0 and has
> expanded across the 0.7.x line. It currently ships 32 parser files
> (source / vocabulary parsers — FortiGate / ASA / Checkpoint / Palo Alto /
> Sysmon / CloudTrail / Zeek / Suricata / OCSF / Juniper SRX / ... — plus the
> transport / format parsers `parse_syslog`, `parse_journald`, and
> `parse_cef`), 31 sibling
> per-source OTLP adapters, the OCSF 1.3.0 27-class composer, the RFC 5424
> and replay-shape composers, one filter, and several reusable functions.
> The inbound `parse_ocsf` compatibility parser is the sole parser without
> an OTLP adapter. See the table below
> for the full current inventory; coverage continues to grow on the
> 0.7.x cadence.

## Snippet library

### Parsers

| Snippet | Source | LSIS class facts produced |
|---|---|---|
| **Transport** | | |
| `parsers/parse_syslog.limpid` | RFC 3164 / 5424 syslog wire (transport, populates `workspace.syslog.*`) | n/a |
| `parsers/parse_journald.limpid` | systemd journald JSON (transport, populates `workspace.journald.*`) | n/a |
| `parsers/parse_cef.limpid` | ArcSight CEF format from a syslog body (populates `workspace.cef.*`; chains as `parse_syslog \| parse_cef \| parse_<vendor>_cef`) | n/a |
| **Security devices / cloud audit** | | |
| `parsers/parse_fortigate_cef.limpid` | FortiGate (CEF wrap; chain `parse_syslog \| parse_cef \|` upstream) | 4001 / 2004 / 3002 / 6002 |
| `parsers/parse_fortigate_syslog.limpid` | FortiGate (native KV syslog) | (same as CEF) |
| `parsers/parse_paloalto_cef.limpid` | PAN-OS (CEF wrap; chain `parse_syslog \| parse_cef \|` upstream) | 4001 / 2004 / 6004 / 3002 |
| `parsers/parse_paloalto_syslog.limpid` | PAN-OS (native CSV syslog; chain `parse_syslog \|` upstream) | (same as CEF) |
| `parsers/parse_asa.limpid` | Cisco ASA / FTD-in-ASA-mode (syslog; chain `parse_syslog \|` upstream) | 3002 / 4001 |
| `parsers/parse_aws_guardduty.limpid` | AWS GuardDuty findings (JSON) | 2004 Detection Finding |
| `parsers/parse_aws_vpc_flow.limpid` | AWS VPC Flow Logs (text v2/v5) | 4001 Network Activity |
| `parsers/parse_azure_activity.limpid` | Azure Activity Log (JSON) | 6003 API Activity |
| `parsers/parse_cloudtrail.limpid` | AWS CloudTrail (JSON) | 6003 API Activity |
| `parsers/parse_k8s_audit.limpid` | Kubernetes Audit API events (JSON) | 6003 / 3002 |
| `parsers/parse_okta_system.limpid` | Okta System Log events (JSON) | 3001 / 3002 / 3005 / 3006 |
| `parsers/parse_juniper_srx_sd_syslog.limpid` | Juniper SRX RT_FLOW (RFC 5424 + Junos SD, `set security log format sd-syslog` mode) | 4001 Network Activity |
| `parsers/parse_juniper_srx_syslog.limpid` | Juniper SRX RT_IDP / IDP_ATTACK_LOG_EVENT (RFC 3164 unstructured, default `syslog` mode) | 2004 Detection Finding |
| `parsers/parse_nsp.limpid` | Trellix / McAfee Network Security Platform IPS alerts (standard syslog KV template, real-traffic verified) | 2004 Detection Finding |
| `parsers/parse_checkpoint_leef.limpid` | Check Point LEEF 2.0 (Accept / Drop / Reject / Block) — for QRadar bridges | 4001 Network Activity |
| `parsers/parse_checkpoint_syslog.limpid` | Check Point Syslog Exporter (Junos-style SD with `:` separator; handles R81+ `Log [Fields@<EID> ...]` `=` variant) | 4001 / 2004 / 3002 |
| **OSS NDR** | | |
| `parsers/parse_suricata.limpid` | OISF Suricata EVE JSON (event_type dispatch: alert / dns / http / flow / tls / fileinfo; stats dropped) | 2004 / 4001 / 4002 / 4003 |
| `parsers/parse_zeek_default.limpid` | Zeek default-enabled scripts (conn / dns / http / ssl / files / x509 / weird / notice) + `_native` / `_flat` convenience entry points | 2004 / 4001 / 4002 / 4003 |
| `parsers/parse_zeek_soc.limpid` | Transitively includes default + adds auth / SMB / DCE-RPC / SNMP / RDP / DHCP (ssh / smtp / ftp / kerberos / ntlm / radius / smb_* / dce_rpc / snmp / rdp) | + 3002 / 4004 / 4005 / 4006 / 4007 / 4008 / 4009 |
| `parsers/parse_zeek_full.limpid` | Transitively includes soc + adds remaining specialised scripts (signature / intel / traceroute / tunnel / mysql / irc / sip / dnp3 / modbus / socks / syslog / ntp / ocsp / pe / rfb / dpd) + drops low-value operational streams + catch-all for unknown `_path` (zero data loss) | + catch-all |
| **Server / host vocabulary** | | |
| `parsers/parse_openssh.limpid` | OpenSSH `sshd` body (transport-agnostic; bridge from `parse_syslog` or `parse_journald`) | 3002 Authentication |
| `parsers/parse_sudo.limpid` | sudo (syslog / journald) | 3003 Authorize Session |
| `parsers/parse_combined_log.limpid` | Apache / Nginx access log (combined format) | 4002 HTTP Activity |
| `parsers/parse_postfix.limpid` | Postfix MTA (syslog) | 4009 Email Activity |
| `parsers/parse_winevent_json.limpid` | Windows Security event log (NXLog / Vector / Winlogbeat JSON) | 3002 / 1007 / 3001 / 3006 |
| `parsers/parse_sysmon.limpid` | Microsoft Sysmon (NXLog / Vector / Winlogbeat JSON) — EventID 1 / 3 / 11 | 1007 / 4001 / 1001 |
| `parsers/parse_bind.limpid` | ISC BIND 9 querylog | 4003 DNS Activity |
| `parsers/parse_auditd.limpid` | Linux auditd, ~45 type codes across 7 OCSF classes (auth / account change / process / file / network / vulnerability / detection), real-corpus verified | 3002 / 3001 / 1007 / 1001 / 4001 / 2002 / 2004 |
| **Vendor-neutral** | | |
| `parsers/parse_ocsf.limpid` | OCSF JSON inbound (any vendor's prior compose_ocsf output); normalizes root `time` from OCSF ms to LSIS ns and `severity_id` to OTel `SeverityNumber` | any class |

### Composers

- `composers/compose_ocsf.limpid` — dispatches by
  `workspace.lsis.parsed.class_uid` to per-class leaves. Covers the
  OCSF 1.3.0 priority set (27 classes spanning System Activity /
  Findings / Identity & Access Management / Network Activity /
  Application Activity). Each leaf strips `null` keys via
  `null_omit` and writes OCSF JSON to
  `workspace.lsis.composed.ocsf`; the companion `ocsf_to_egress`
  process moves the slot to `egress`.
- `composers/compose_otlp.limpid` — assembles an OTLP-1.0.0
  `ResourceLogs` proto envelope. Each OTLP-capable raw-source parser file
  carries a sibling `<source>_to_otlp` adapter; it
  owns source-specific Resource, Scope, Body, and LogRecord attribute
  placement. The composer maps canonical parsed time and severity plus ten
  optional shed slots. The canonical shape is
  `parse_<source> | <source>_to_otlp | compose_otlp | otlp_to_egress`.
  Deployment-specific target adjustments may replace shed slots after the
  adapter; they do not replace the adapter itself. The inbound `parse_ocsf`
  compatibility parser is the sole parser without an OTLP adapter.
- `composers/compose_rfc5424.limpid` — generic RFC 5424 wire
  composer reading `workspace.lsis.shed.rfc5424.*` (pri / timestamp
  / hostname / app_name / procid / msgid / sd / msg). A named
  `journald_to_rfc5424` bridge lives in the same file for the
  common journald → syslog-relay path (edge → relay → AMA).
- `composers/compose_replayable.limpid` — minimal `{received_at,
  source, ingress}` JSON shape that round-trips through `inject
  --json` for parser regression / replay capture. Use it on a
  fan-out branch to record the raw wire while a parallel branch
  parses, so a parser bug discovered later can be fixed and the
  saved JSONL replayed offline.

See the [pack
README](https://github.com/naoto256/limpid/blob/release/0.8.1/packaging/snippets/README.md#slot-registry--composed-layer)
for the full composed / shed slot registries and the LSIS layer
contracts. The same README defines the authoring schema: each file declares a
`Facade`, and every listed process or function has an adjacent contract block.
Private dispatch leaves remain unlisted and need no public header.

### Filters

- `filters/filter_openssh_journal.limpid` — drops PAM-side noise
  (`pam_unix(sshd:session): session opened/closed`) from journald-
  sourced sshd streams. Run between `parse_journald` and the
  bridge into `parse_openssh`. sshd itself emits the authentication
  fact via `Accepted ...` / `Disconnected ...`; the PAM duplicate
  would double-count.

### Functions

- `functions/severity_converter.limpid` — explicit OCSF 1.3.0
  `severity_id` ↔ OTel `SeverityNumber` boundary conversion. The functions
  are partial and return `null` outside their standard domains;
  `parse_ocsf` and `compose_ocsf` turn invalid non-null protocol values into
  explicit errors. OCSF Other (99) remains represented by its sibling
  `severity` text rather than an invented OTel number.

- `functions/parse_datetime_rfc3164.limpid` —
  `parse_datetime_rfc3164(String, String) → Timestamp | Null`. LPL
  counterpart to the built-in `parse_datetime_rfc3339` primitive.
  RFC 3164 wire carries neither year nor timezone, so the helper
  applies the current-year + future-clamp policy after the calling
  parser resolves its vendor-specific timezone default or explicit
  override. A vendor-defined fixed zone wins; documented device-local
  formats and formats with no authoritative timezone contract both
  default to `local` (the limpid host's system timezone — the device
  most likely shares the host's zone).
  IANA names and fixed offsets are also accepted as explicit overrides.
  For RFC 5424 / OTLP / OCSF input use the built-in
  `parse_datetime_rfc3339` primitive directly.
- `functions/timestamp_converter.limpid` — exact integer boundary helpers
  `timestamp_ns_to_ms(Int | Timestamp | Null) → Int | Null` and
  `timestamp_ms_to_ns(Int | Null) → Int | Null`.
  `compose_ocsf` uses the first;
  `parse_ocsf` uses the second. Helpers shared by multiple source schemas
  live under `functions/`; source-local helpers stay beside their parser.
- `functions/http_method_activity_id.limpid` —
  `http_method_activity_id(String) → Int`. HTTP request method
  (`GET` / `POST` / `PUT` / `DELETE` / `HEAD` / `OPTIONS` /
  `CONNECT` / `TRACE`) → OCSF 4002 HTTP Activity `activity_id`,
  with `99` (Other) for anything outside the OCSF 1.3.0 enumeration.
- `functions/proto_num.limpid` — `proto_num(String) → Int | Null`.
  Transport-layer protocol name (case-insensitive `tcp` / `udp` /
  `icmp` / …) → IANA protocol number for OCSF
  `connection_info.protocol_num`. Returns `null` rather than a
  wrong guess for unknown names.

## Quick start

The basic pattern is a few `include` lines + a staged pipeline
(transport → format → vendor vocabulary → composer):

```limpid
include "/usr/share/limpid/snippets/parsers/parse_syslog.limpid"
include "/usr/share/limpid/snippets/parsers/parse_cef.limpid"
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
    process parse_syslog | parse_cef | parse_fortigate_cef | compose_ocsf | ocsf_to_egress
    output security_lake
}
```

`parse_syslog` unwraps the transport, `parse_cef` decodes the CEF
format, and `parse_fortigate_cef` writes facts to `workspace.lsis.parsed.*`;
`compose_ocsf` reads from there and writes the OCSF JSON record to
`workspace.lsis.composed.ocsf`; the one-line `ocsf_to_egress`
companion at the tail of the pipeline moves the slot to `egress`.
Swap the parser for any of the others; swap
`compose_ocsf | ocsf_to_egress` for
`compose_replayable | replayable_to_egress` to capture replay-shape;
chain a filter ahead of the parser to drop noise.

For mixed-vendor inputs, dispatch upstream of the parser:

```limpid
def pipeline mixed_in {
    input multi_vendor_syslog
    if contains(ingress, "CEF:0|Palo Alto Networks") {
        process parse_syslog | parse_cef | parse_paloalto_cef | compose_ocsf | ocsf_to_egress
    } else if contains(ingress, "CEF:0|Fortinet") {
        process parse_syslog | parse_cef | parse_fortigate_cef | compose_ocsf | ocsf_to_egress
    } else {
        process parse_syslog | parse_paloalto_syslog | compose_ocsf | ocsf_to_egress
    }
    output security_lake
}
```

## Design contracts

The LSIS namespace convention (three layers: parsed / shed /
composed) that ties parsers and composers together is documented in
the [pack
README](https://github.com/naoto256/limpid/blob/release/0.8.1/packaging/snippets/README.md#lsis--the-limpid-snippet-intermediate-schema).
Snippets and pipelines below follow the same three-layer contract.

The other contract worth calling out here is the loud-fail-fast
policy on unsupported vocabulary.

### Loud-fail-fast on unsupported vocabulary

Each parser's dispatcher routes events with shapes / subtypes /
message IDs the snippet does not handle to the configured `error_log`
(DLQ) via the `error` keyword, with an operator-readable message
(e.g. `parse_asa: unsupported message ID 400039: IPS:6101 RPC Port
Unregistration ...`). Silent zero-mapping is forbidden — if a
vendor adds a field or a new subtype, the operator sees it in the
DLQ on day one and decides whether to extend the snippet or update
the upstream allow-list.

The DLQ entries are JSONL via `control { error_log "..." }`; without
that, errors fall back to a payload-free `tracing::error!` summary
line (the [`error_log_fallback`
ladder](../operations/error-log.md#tracing-fallback-ladder-error_log_fallback)'s
default). Set `error_log` explicitly so unsupported-vocabulary events
land in a replayable file rather than a summary-only signal on
journald.

## Per-parser status

Every parser's docstring records:

- the wire format and any wrapper assumptions (RFC 3164 syslog,
  JSON framing, etc.);
- per-message-ID / per-subtype OCSF mappings;
- the test corpus the parser was verified against (real / public /
  synthetic) and per-shape parse-rate;
- `NOTE`-flagged subtypes that are documented from the vendor's
  spec but not yet exercised against live data — verify before
  relying on them in production.

Highlights (security devices / cloud audit first, server / host
systems below):

- **PAN-OS** parsers were verified against a live PA-460 in Tap
  mode, with four wire-format quirks fixed vs. the legacy CEF docs
  (severity is 1-5 not 0-10, `cs1=Rule` not Threat Category,
  `signature_id` carries the threat name not `cs2`,
  `SourceLocation` is GeoIP not hostname).
- **ASA** verified against the miroslav-siklosi/Syslog-Generator
  synthetic corpus (5000 lines, 96 distinct message IDs); auth
  event IDs (605004 / 605005 / 611101 / 611103 / 109001 / 109005 /
  109017) parsed cleanly, the long tail of system / IPS / VPN
  message IDs routes to error_log per the loud-fail-fast policy.
- **CloudTrail** verified against the public FLAWS dataset (1M
  events): activity_id verb prefix mapping (Get/Describe/List →
  Read, Create/Put/Add → Create, Update/Modify/Set → Update,
  Delete/Remove/Detach → Delete, etc.) covers ~99% of the corpus.
- **OpenSSH** verified against a playground sshd capture plus a
  journald feed of internet-facing sshd traffic; covers
  `Accepted` / `Failed` / `Invalid user` / `Disconnected` /
  `Connection closed` / `banner exchange` / `Did not receive
  identification`.
- **sudo** verified across three hosts (4565 lines) covering both
  modern pam_unix wire form and older variants;
  command-continuation lines (sudo's COMMAND= overflow handling)
  drop silently.
- **Postfix** verified against a real production mail.log slice;
  smtp delivery / qmgr accept / smtpd connect / NOQUEUE reject /
  bounce shapes covered.
- **Windows Event Log JSON** verified against the OTRF / Mordor
  attack-scenario dataset (Empire mimikatz logonpasswords trace,
  702 Security-channel events).

The remaining classes for which `compose_ocsf` has a leaf but no bundled
parser currently produces facts are Registry Key Activity (1008), Registry
Value Activity (1009), Compliance Finding (2003), Incident Finding (2005),
Network File Activity (4010), Datastore Activity (6005), and Scan Activity
(6007). These are candidates for future snippets.

## Authoring your own snippets

If you write a vendor parser for a source not in the library, the
conventions are:

- **One file per (vendor, format).** FortiGate has two files
  (`parse_fortigate_cef` + `parse_fortigate_syslog`) because CEF and
  native KV are different wire shapes; OpenSSH is one file because
  sshd's wire is one shape across syslog and journald.
- **Canonical file header** follows the per-kind schema documented in the
  [pack README](https://github.com/naoto256/limpid/blob/release/0.8.1/packaging/snippets/README.md#authoring-conventions):
  parser files declare `Summary`, `Reads`, `Writes`, `Category`, and
  `Test corpus`; composers and shared functions use their corresponding
  schemas. The header is the source for generated inventory. Keep sample
  wires in the header and anonymise them with documentation domains and
  RFC 5737 addresses.
- **Two-tier dispatch**: the top-level `def process parse_<vendor>`
  strips the wrapper and routes by header field (`switch
  workspace.<vendor>.<key>`); per-leaf `def process` re-parses the
  body against its subtype-specific shape and writes the parsed
  fact record to `workspace.lsis.parsed.*`.
- **Loud-fail-fast** on unsupported vocabulary via `default { error
  "<operator-readable msg>" }`.
- **Helpers** (`def function ...`) carry per-vendor mapping tables
  (severity → OTel SeverityNumber, action → activity_id, etc.) — keep
  them in the same file as the parser so the parser is
  self-contained.

The library files themselves are good worked examples — the OpenSSH
parser is the smallest and shows the basic shape; the PAN-OS CEF
parser shows multi-class dispatch from a single header field; the
Postfix parser shows nested two-tier dispatch (program → subtype).

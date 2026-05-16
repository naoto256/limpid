# limpid Snippet Library

A read-only library of DSL snippets shipped with the `limpid` package
and installed under `/usr/share/limpid/snippets/`. User configurations
reference snippets by absolute path; the config loader's allow-list
(`SYSTEM_SNIPPET_DIR` in `config.rs`) explicitly permits this single
prefix.

## Layout

```
/usr/share/limpid/snippets/
├─ parsers/      per-vendor / per-format parsers writing to
│                workspace.limpid.* (the parser ↔ composer
│                canonical intermediate)
├─ composers/    target-schema composers reading from
│                workspace.limpid.* (currently OCSF 1.3.0;
│                also the replay-shape composer for parser
│                regression capture)
└─ filters/      pre-parser noise filters (drop / pass-through
                 by content predicate)
```

## What's included (v0.7.0)

### Parsers (18)

| File | Source | OCSF class(es) emitted |
|---|---|---|
| **Transport** | | |
| `parsers/parse_syslog.limpid` | RFC 3164 / 5424 syslog (transport, populates `workspace.syslog.*`) | n/a (transport layer) |
| `parsers/parse_journald.limpid` | systemd journald JSON (transport, populates `workspace.journald.*`) | n/a (transport layer) |
| **Security devices / cloud audit** | | |
| `parsers/parse_fortigate_cef.limpid` | FortiGate (CEF wrap) | 4001 / 2004 / 3002 / 6002 |
| `parsers/parse_fortigate_syslog.limpid` | FortiGate (native KV syslog) | (same as CEF) |
| `parsers/parse_paloalto_cef.limpid` | PAN-OS (CEF wrap) | 4001 / 2004 / 6004 / 3002 |
| `parsers/parse_paloalto_syslog.limpid` | PAN-OS (native CSV syslog) | (same as CEF) |
| `parsers/parse_asa.limpid` | Cisco ASA / FTD-in-ASA-mode (syslog) | 3002 / 4001 |
| `parsers/parse_cloudtrail.limpid` | AWS CloudTrail (JSON) | 6003 API Activity |
| `parsers/parse_juniper_srx_sd_syslog.limpid` | Juniper SRX RT_FLOW (RFC 5424 + Junos SD, `set security log format sd-syslog` mode) | 4001 Network Activity |
| `parsers/parse_juniper_srx_syslog.limpid` | Juniper SRX RT_IDP / IDP_ATTACK_LOG_EVENT (RFC 3164 unstructured, default `syslog` mode) | 2004 Detection Finding |
| `parsers/parse_nsp.limpid` | Trellix / McAfee Network Security Platform IPS alerts (standard syslog KV template, real-traffic verified against a real NSP Manager) | 2004 Detection Finding |
| `parsers/parse_checkpoint_leef.limpid` | Check Point LEEF 2.0 (Accept / Drop / Reject / Block) — for QRadar bridges | 4001 Network Activity |
| `parsers/parse_checkpoint_syslog.limpid` | Check Point Syslog Exporter (`[key:"value"; ...]` SD; also handles R81+ `Log [Fields@<EID> ...]` `=` variant) | 4001 / 2004 / 3002 |
| **OSS NDR** | | |
| `parsers/parse_suricata.limpid` | OISF Suricata EVE JSON (event_type dispatch: alert / dns / http / flow / tls / fileinfo; stats dropped) | 2004 / 4001 / 4002 / 4003 |
| `parsers/parse_zeek_default.limpid` | Zeek default-enabled scripts (conn / dns / http / ssl / files / x509 / weird / notice) + `_native` / `_flat` convenience variants | 2004 / 4001 / 4002 / 4003 |
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
| `parsers/parse_ocsf.limpid` | OCSF JSON inbound (any vendor's prior compose_ocsf output) | passthrough (any class) |

Each parser's docstring records:
- the wire format and any wrapper assumptions (RFC 3164 syslog, JSON
  framing, etc.);
- per-message-ID / per-subtype OCSF mappings;
- the test corpus the parser was verified against (real / public /
  synthetic) and per-shape parse-rate;
- `NOTE`-flagged subtypes that are documented from the vendor's spec
  but not yet exercised against live data — verify before relying on
  them in production.

### Composers (3)

- `composers/compose_ocsf.limpid` — dispatches by
  `workspace.limpid.class_uid` to per-class leaves, covering the
  OCSF 1.3.0 priority set (27 classes). Each leaf strips `null`
  keys via `null_omit` and writes OCSF JSON to `egress`.
- `composers/compose_rfc5424.limpid` — `workspace.journald.*` →
  RFC 5424 syslog wire. Used at edge boxes to re-frame journald
  entries for syslog relay (e.g. edge → relay → AMA).
- `composers/compose_replayable.limpid` — minimal `{received_at,
  source, ingress}` shape that round-trips through `inject --json`
  for parser regression / replay capture.

### Filters (1)

- `filters/filter_openssh_journal.limpid` — drops `pam_unix(sshd:session):
  session opened/closed` PAM noise from journald-sourced sshd
  streams (run between `parse_journald` and `parse_openssh`). sshd
  itself emits the authentication fact via `Accepted ...` /
  `Disconnected ...`; the PAM duplicate would double-count.

### Functions (1)

- `functions/parse_datetime_rfc3164.limpid` —
  `parse_datetime_rfc3164(text) → Timestamp`, the LPL counterpart to
  the built-in `parse_datetime_rfc3339` primitive. Shipped as LPL
  rather than Rust because RFC 3164 wire carries neither year nor
  timezone, so the parser has to encode policy (current-year +
  future-clamp + UTC assumption) that operators should be able to
  edit. For RFC 5424 / OTLP / OCSF input use the built-in
  `parse_datetime_rfc3339(text)` primitive directly.

### Filebeat-flat JSON: `nest_dotted_keys` primitive

Some upstreams (Filebeat / Logstash JSON emitters used by zeek
and suricata modules, certain Splunk HEC sources, OpenSearch
ingest pipelines) flatten nested JSON for Elasticsearch indexing
conventions: `{"id": {"orig_h": "1.1.1.1"}}` becomes
`{"id.orig_h": "1.1.1.1"}`. limpid DSL does not expose
bracket-subscript access (`body["id.orig_h"]`) by design, so
dotted keys are unreachable from a parser without normalising.

The Rust primitive `nest_dotted_keys(obj)` recursively
un-flattens dotted keys back into nested Objects, loud-fail on
collisions. The Zeek `_flat` convenience entry points use it
internally; for any other Filebeat-flattened wire, wrap the
parse step explicitly:

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

def output ama {
    type tcp
    address "127.0.0.1:28330"
}

def pipeline fw_to_ocsf {
    input fw_syslog
    process parse_fortigate_cef | compose_ocsf
    output ama
}
```

That's it. The parser writes to `workspace.limpid.*` (canonical
OCSF-shape intermediate); the composer reads from `workspace.limpid.*`
and writes OCSF JSON to `egress`. Add `output` to your SIEM /
data-lake destination (Sentinel, Splunk, Security Lake, OTLP, …)
and you're shipping OCSF.

## Design principles

The library follows two contracts, documented at length in
`docs/src/processing/user-defined.md`:

1. **`workspace.limpid` is the parser ↔ composer canonical
   intermediate.** Parsers populate `workspace.limpid.*` only with
   OCSF-canonical fields. Vendor intermediates (`workspace.cef`,
   `workspace.syslog`, `workspace.pf`, `workspace.ct`, etc.) are
   parser-private and the composer never reads them. This keeps the
   composer schema-aware (it knows OCSF) without it being
   vendor-aware (it never sees CEF / FortiGate quirks).
2. **Loud-fail-fast on unsupported vocabulary.** Each parser's
   dispatcher routes events with shapes / subtypes / message IDs
   the snippet does not handle to `error_log` (DLQ) via the `error`
   keyword, with an operator-readable message. Silent zero-mapping
   is forbidden — if a vendor adds a field or a new subtype, the
   operator sees it in the DLQ on day one and decides whether to
   extend the snippet or update the upstream allow-list.

## Pipeline shape

Parsers expect to receive raw events on `ingress` and produce
canonical OCSF-shape on `workspace.limpid.*`. The typical pipeline
is two stages:

```
process <vendor_parser> | compose_ocsf
```

For mixed-vendor / mixed-format inputs, dispatch upstream of the
parser with a `switch contains(ingress, "...")` block, calling the
appropriate parser per branch. (See the test scaffolding under
`_check_*.limpid` in the repo root for working examples.)

## Authoring conventions

If you contribute a new snippet, see the per-file headers for
the canonical shape:

- File header: `// Vendor: ...` / `// Wire: ...` / `// Output: ...`
  block at the top, followed by per-shape sample lines (anonymised
  to RFC 5321 / 5737 forms — `example.com`, `192.0.2.x`,
  `198.51.100.x`).
- Each `def process` body is single-responsibility (header parse,
  dispatch, per-leaf record build); the dispatcher handles
  unsupported vocabulary with `error "<operator-readable msg>"`.
- Helpers (`def function ...`) carry their per-vendor mapping
  tables (severity → OCSF severity_id, action → activity_id, etc.).
- Files are one per (vendor, format). FortiGate has two files
  (`parse_fortigate_cef` + `parse_fortigate_syslog`) because CEF
  and native KV are different wire shapes; OpenSSH is one file
  because sshd's wire is one shape across syslog and journald.

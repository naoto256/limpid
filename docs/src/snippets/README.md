# Snippet Library

A maintained set of vendor parsers and target-schema composers,
shipped with the `limpid` package and installed under
`/usr/share/limpid/snippets/`. Operators get vendor logs into a
SIEM / data lake in OCSF form by adding a single `include` line to
their config — no parser to write from scratch, no recompile when
a vendor adds a field (the snippet is plain DSL: edit the file and
SIGHUP).

> **Status — v0.7.0:** the library debuts in this release with 11
> parsers, the OCSF 1.3.0 27-class composer, the replay-shape
> composer, and one filter. Coverage will grow over the 0.7.x
> point-release line.

## What ships in v0.7.0

### Parsers

| Snippet | Source | OCSF class(es) emitted |
|---|---|---|
| **Security devices / cloud audit** | | |
| `parsers/parse_fortigate_cef.limpid` | FortiGate (CEF wrap) | 4001 / 2004 / 3002 / 6002 |
| `parsers/parse_fortigate_syslog.limpid` | FortiGate (native KV syslog) | (same as CEF) |
| `parsers/parse_paloalto_cef.limpid` | PAN-OS (CEF wrap) | 4001 / 2004 / 6004 / 3002 |
| `parsers/parse_paloalto_syslog.limpid` | PAN-OS (native CSV syslog) | (same as CEF) |
| `parsers/parse_asa.limpid` | Cisco ASA / FTD-in-ASA-mode (syslog) | 3002 / 4001 |
| `parsers/parse_cloudtrail.limpid` | AWS CloudTrail (JSON) | 6003 API Activity |
| **Server / host systems** | | |
| `parsers/parse_openssh.limpid` | OpenSSH `sshd` (syslog / journald) | 3002 Authentication |
| `parsers/parse_sudo.limpid` | sudo (syslog / journald) | 3003 Authorize Session |
| `parsers/parse_combined_log.limpid` | Apache / Nginx access log (combined format) | 4002 HTTP Activity |
| `parsers/parse_postfix.limpid` | Postfix MTA (syslog) | 4009 Email Activity |
| `parsers/parse_winevent_json.limpid` | Windows Security event log (NXLog / Vector / Winlogbeat JSON) | 3002 / 1007 / 3001 / 3006 |
| **Vendor-neutral** | | |
| `parsers/parse_ocsf.limpid` | OCSF JSON inbound (any vendor's prior compose_ocsf output) | passthrough (any class) |

### Composers

- `composers/compose_ocsf.limpid` — dispatches by
  `workspace.limpid.class_uid` to per-class leaves. Covers the OCSF
  1.3.0 priority set (27 classes spanning System Activity / Findings
  / Identity & Access Management / Network Activity / Application
  Activity). Each leaf strips `null` keys via `null_omit` and writes
  OCSF JSON to `egress`.
- `composers/compose_replayable.limpid` — minimal `{received_at,
  source, ingress}` JSON shape that round-trips through `inject
  --json` for parser regression / replay capture. Use it on a
  fan-out branch to record the raw wire while a parallel branch
  parses, so a parser bug discovered later can be fixed and the
  saved JSONL replayed offline.

### Filters

- `filters/filter_openssh_journal.limpid` — drops PAM-side noise
  (`pam_unix(sshd:session): session opened/closed`) from journald-
  sourced sshd streams before `parse_openssh` sees them. sshd
  itself emits the authentication fact via `Accepted ...` /
  `Disconnected ...`; the duplicate would double-count.

## Quick start

The basic pattern is two `include` lines + a two-stage pipeline:

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

`parse_fortigate_cef` writes the parsed event to `workspace.limpid.*`
in canonical OCSF shape; `compose_ocsf` reads from there and writes
the OCSF JSON record to `egress`. Swap the parser for any of the
others; swap the composer for `compose_replayable` to capture
replay-shape; chain a filter ahead of the parser to drop noise.

For mixed-vendor inputs, dispatch upstream of the parser:

```limpid
def pipeline mixed_in {
    input multi_vendor_syslog
    if contains(ingress, "CEF:0|Palo Alto Networks") {
        process parse_paloalto_cef | compose_ocsf
    } else if contains(ingress, "CEF:0|Fortinet") {
        process parse_fortigate_cef | compose_ocsf
    } else {
        process parse_paloalto_syslog | compose_ocsf
    }
    output security_lake
}
```

## Design contracts

Two contracts run through the library — the parser ↔ composer
canonical intermediate, and the loud-fail-fast policy on
unsupported vocabulary.

### `workspace.limpid` is the canonical intermediate

Parsers populate `workspace.limpid.*` only with OCSF-canonical
fields. Vendor intermediates (`workspace.cef`, `workspace.syslog`,
`workspace.pf`, `workspace.ct`, `workspace.winevent`, etc.) are
parser-private scratch — the composer never reads them. This keeps
the composer schema-aware (it knows OCSF) without making it
vendor-aware (it never sees CEF quirks, FortiGate dialect, or PAN-OS
positional CSV columns).

The contract is documented in [Process Design Guide → Use
`workspace.limpid` as the canonical
intermediate](../processing/user-defined.md#assignments). New
parsers follow it; out-of-tree vendor parsers should follow it too
so they compose cleanly with `compose_ocsf`.

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
that, errors fall back to a structured `tracing::error!` line.
Configure the error log path explicitly so unsupported-vocabulary
events don't silently scroll off journald.

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
  log-forwarder feed of internet-facing sshd traffic; covers
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

The remaining classes for which `compose_ocsf` has a leaf but no
parser yet emits to (most of category 2 Findings; some of category
4 sub-protocols like DNS / DHCP / RDP / SMB / SSH / FTP /
NetworkFile; some of category 6 like Datastore / Scan) are
candidates for upcoming snippets in 0.7.x point releases.

## Authoring your own snippets

If you write a vendor parser for a source not in the library, the
conventions are:

- **One file per (vendor, format).** FortiGate has two files
  (`parse_fortigate_cef` + `parse_fortigate_syslog`) because CEF and
  native KV are different wire shapes; OpenSSH is one file because
  sshd's wire is one shape across syslog and journald.
- **File header** carries `// Vendor:` / `// Wire:` / `// Output:`
  lines describing the source, plus per-shape sample lines anonymised
  to RFC 5321 / 5737 forms (`example.com`, `192.0.2.x`,
  `198.51.100.x`).
- **Two-tier dispatch**: the top-level `def process parse_<vendor>`
  strips the wrapper and routes by header field (`switch
  workspace.<vendor>.<key>`); per-leaf `def process` re-parses the
  body against its subtype-specific shape and writes the OCSF
  record to `workspace.limpid.*`.
- **Loud-fail-fast** on unsupported vocabulary via `default { error
  "<operator-readable msg>" }`.
- **Helpers** (`def function ...`) carry per-vendor mapping tables
  (severity → OCSF severity_id, action → activity_id, etc.) — keep
  them in the same file as the parser so the parser is
  self-contained.

The library files themselves are good worked examples — the OpenSSH
parser is the smallest and shows the basic shape; the PAN-OS CEF
parser shows multi-class dispatch from a single header field; the
Postfix parser shows nested two-tier dispatch (program → subtype).

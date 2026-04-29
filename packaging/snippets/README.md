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

### Parsers (11)

| File | Source | OCSF class(es) emitted |
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

Each parser's docstring records:
- the wire format and any wrapper assumptions (RFC 3164 syslog, JSON
  framing, etc.);
- per-message-ID / per-subtype OCSF mappings;
- the test corpus the parser was verified against (real / public /
  synthetic) and per-shape parse-rate;
- `NOTE`-flagged subtypes that are documented from the vendor's spec
  but not yet exercised against live data — verify before relying on
  them in production.

### Composers (2)

- `composers/compose_ocsf.limpid` — dispatches by
  `workspace.limpid.class_uid` to per-class leaves, covering the
  OCSF 1.3.0 priority set (27 classes). Each leaf strips `null`
  keys via `null_omit` and writes OCSF JSON to `egress`.
- `composers/compose_replayable.limpid` — minimal `{received_at,
  source, ingress}` shape that round-trips through `inject --json`
  for parser regression / replay capture.

### Filters (1)

- `filters/filter_openssh_journal.limpid` — drops `pam_unix(sshd:session):
  session opened/closed` PAM noise from journald-sourced sshd
  streams. sshd already emits its own `Accepted ...` /
  `Disconnected ...` lines that cover the same authentication fact;
  the PAM duplicate would double-count.

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

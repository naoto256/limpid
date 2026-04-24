# Snippet Library

The `limpid` package ships a read-only library of DSL snippets under
`/usr/share/limpid/snippets/`. Snippets are pure-DSL building blocks
that reduce a working pipeline for common SIEM sources to a handful of
`include` directives plus a pipeline definition.

The library follows Design Principle 3 strictly: schema knowledge
(vendor formats, OCSF class shapes, severity / protocol mappings) lives
in DSL snippets, not in the limpid runtime.

## What's in v0.5.0

| Tree | Files | Purpose |
|------|-------|---------|
| [`_common/`](#shared-helpers) | 5 | Format detection, CEF header parser, vendor / format dispatchers, severity / protocol normalisers |
| [`parsers/`](#parsers) | 5 | FortiGate, Check Point, Palo Alto, Microsoft Defender for Endpoint, Azure Application Gateway WAF |
| [`composers/`](#composers) | 2 | OCSF Network Activity (4001), Detection Finding (2004) |

Additional parsers and composers ship in v0.5.x point releases as they
mature.

## Quick start

```limpid
// /etc/limpid/limpid.conf
include "/usr/share/limpid/snippets/parsers/fortigate.limpid"
include "/usr/share/limpid/snippets/composers/ocsf_network_activity.limpid"

def input fw_syslog {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output ama {
    type tcp
    addr "127.0.0.1:28330"
}

def pipeline fw_to_ocsf {
    input fw_syslog
    process parse_fortigate | compose_ocsf_network_activity
    output ama
}
```

Two `include`s, one pipeline. The parser snippet declares its own
dependencies on shared helpers (`_common/cef.limpid`,
`_common/proto.limpid`, etc.); the loader resolves them recursively.

> **Why absolute paths?**
> The config loader's allow-list explicitly permits absolute includes
> under `/usr/share/limpid/snippets/` (and only there). System-package
> upgrades propagate fixes without per-config copies. Any other
> absolute path is still rejected — the config root remains the single
> writable include space.

## Pipeline granularity

Every parser exposes three entry points; pick whichever matches your
event stream:

| Granularity | Example | When to use |
|-------------|---------|-------------|
| **Leaf** (zero dispatch cost) | `parse_fortigate_cef_traffic` | You know the format and subtype |
| **Vendor** (subtype auto-detect) | `parse_fortigate` | You know the vendor; format / subtype vary per event |
| **All-CEF** (vendor auto-detect) | `parse_cef` | Mixed-vendor CEF stream from a single collector |

All three produce identical OCSF output for a given input. Composers
work the same way: `compose_ocsf` dispatches on
`workspace.class_uid`, or you can call `compose_ocsf_<class>` directly.

## Shared helpers

Files under `_common/` are internal building blocks; their names start
with `_` to mark them as such. You usually do not include them
directly — parsers do.

| File | Provides | Notes |
|------|----------|-------|
| `_common/format.limpid` | `_detect_format` | Decides between `cef`, `json`, `syslog_kv` based on payload prefix |
| `_common/cef.limpid` | `_parse_cef_header` | Single source of CEF header parsing for every CEF parser |
| `_common/cef_vendor_dispatch.limpid` | `parse_cef` | Routes a CEF event to its vendor parser via `cef_device_vendor` |
| `_common/severity.limpid` | `_normalize_severity` | `low/medium/high/critical/...` → OCSF `severity_id` (1–6); lowercases first so `"High"` and `"high"` both work |
| `_common/proto.limpid` | `_normalize_proto` | IANA numeric (`6` / `17` / `1`) **or** lowercase name (`"tcp"`) → canonical OCSF `protocol_name` |

## Parsers

Each parser snippet contains:

1. **A leaf** that maps a specific `(vendor, format, subtype)` triple
   to canonical workspace fields (`workspace.src_endpoint.ip`, etc.).
2. **A subtype dispatcher** keyed on the relevant discriminator
   (`cef_name` for FortiGate; `cef_device_product` for Check Point;
   PAN log_type for Palo Alto; …).
3. **A vendor dispatcher** that runs format detection and routes to
   the right format-specific subtype dispatcher.

The user-facing names are stable; internal helpers are documented in
each file's header.

| Snippet | Vendor / Source | OCSF class output (v0.5.0) |
|---------|-----------------|----------------------------|
| `parsers/fortigate.limpid` | Fortinet FortiGate (CEF) | 4001 Network Activity |
| `parsers/checkpoint.limpid` | Check Point Log Exporter (CEF) | 4001 Network Activity |
| `parsers/paloalto.limpid` | Palo Alto Networks PAN-OS syslog (CSV THREAT) | 2004 Detection Finding |
| `parsers/mde.limpid` | Microsoft Defender for Endpoint Security Graph alerts (JSON) | 2004 Detection Finding |
| `parsers/azure_waf.limpid` | Azure Application Gateway WAF (JSON, AppGwFirewallLog) | 2004 Detection Finding |

## Composers

| Snippet | OCSF class | Notes |
|---------|-----------|-------|
| `composers/ocsf_network_activity.limpid` | 4001 Network Activity | Plus `compose_ocsf` schema-level dispatcher (extended as new composers land) |
| `composers/ocsf_detection_finding.limpid` | 2004 Detection Finding | Tolerates `null` `src_endpoint` / `dst_endpoint` (process-centric findings have no network endpoint) |

Composers are deliberately mechanical: each is a hash literal of
canonical workspace fields plus a few class-specific constants
(`class_uid`, `category_uid`), serialised to `egress` via `to_json`.
See [Authoring Conventions](./conventions.md) for the full template.

## Why the library exists

Without it, every limpid user reinvents the same things:

- A CEF header parser that strips the optional syslog wrapper.
- A 5-stage severity string → integer mapping.
- An IANA protocol number → name mapping.
- An OCSF JSON shape per class.

These are domain knowledge, not user policy. The library gives every
operator a known-good starting point and a maintainable channel for
fixes (security advisories, OCSF spec bumps, vendor format changes
all flow through `apt upgrade limpid`).

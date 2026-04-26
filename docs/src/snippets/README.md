# Snippet Library

> **Status — v0.5.0:** the shipped snippet library is **not yet included** in this release. The runtime support for snippets (the `/usr/share/limpid/snippets/` allow-list, nested `include`, the canonical intermediate `workspace.limpid.*`) is in place; the library files themselves land in **v0.6.0**.

## What snippets will be

A snippet is a `.limpid` file the limpid package installs under `/usr/share/limpid/snippets/`. User configurations include them as ordinary DSL fragments. The intent is that operators get a maintained set of vendor parsers and target-schema composers without having to write them from scratch.

The planned layout:

```
/usr/share/limpid/snippets/
├─ _common/      shared helpers (format detection, CEF header, severity / protocol normalisers)
├─ parsers/      per-(vendor, format) parsers writing to workspace.limpid.*
└─ composers/    per-target-class composers reading from workspace.limpid.*
```

The principles parsers and composers will follow — the canonical intermediate (`workspace.limpid`), the responsibility split between format primitives, vendor parsers, and composers, the parser/composer contract — are documented in [Process Design Guide → Use `workspace.limpid` as the canonical intermediate](../processing/design-guide.md#use-workspacelimpid-as-the-canonical-intermediate). Until the library lands you can write your own snippets following those conventions; once it lands the same conventions apply.

## What's included today

Nothing yet. The runtime path validates includes from `/usr/share/limpid/snippets/`, but the directory is empty in the v0.5.0 package.

## What lands in v0.6.0

The first batch is expected to cover:

- `_common/` helpers (format dispatcher, CEF header parser, severity / protocol normalisers).
- `parsers/` for FortiGate (CEF), Check Point (CEF), Palo Alto (CSV THREAT), Microsoft Defender for Endpoint (JSON), Azure Application Gateway WAF (JSON).
- `composers/` for OCSF Network Activity (4001) and OCSF Detection Finding (2004), plus a `compose_ocsf` schema-level dispatcher.

Concrete contents and naming will be documented here when the v0.6.0 release lands.

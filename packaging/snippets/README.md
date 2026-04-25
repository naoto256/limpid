# limpid Snippet Library

A read-only library of DSL snippets shipped with the `limpid` package
and installed under `/usr/share/limpid/snippets/`. User configurations
reference snippets by absolute path; the config loader's allow-list
(`SYSTEM_SNIPPET_DIR` in `config.rs`) explicitly permits this single
prefix.

## Layout

```
/usr/share/limpid/snippets/
├─ _common/          shared helpers (format detection, CEF header,
│                    proto / severity normalisers, vendor dispatcher)
├─ parsers/          per-vendor parsers (FortiGate, Check Point,
│                    Palo Alto, MDE, Azure WAF in v0.5.0)
└─ composers/        OCSF class-specific composers
                     (Network Activity 4001, Detection Finding 2004
                     in v0.5.0; more to follow)
```

## Quick start

Drop a snippet into your `/etc/limpid/limpid.conf`:

```limpid
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

That's it. The parser snippet declares its own dependencies on shared
helpers (`_common/cef.limpid`, `_common/proto.limpid`); the loader
resolves them recursively, so the user only includes the **entry
points** they actually want.

## Design principles

The library follows three conventions, documented at length in
`docs/src/processing/user-defined.md` and `_PLAN_V050_SNIPPET_LIBRARY.md`:

1. **Re-entrant self-healing** — every parser / composer can be called
   at any layer (leaf, vendor dispatcher, format dispatcher) and
   produces the same egress. Each layer null-checks its prerequisites
   and pulls them in via `process` calls if absent. Helpers run
   exactly once thanks to the workspace-state guard.
2. **Positionless collections** — arrays in the DSL have no `arr[n]`
   syntax. Element identity (via `find_by` / `foreach`) is the only
   addressing model; mutation is `append` / `prepend` only. The
   library never relies on element position.
3. **Schema knowledge in DSL, not Rust** — vendor-specific quirks
   (FortiGate vs Check Point CEF protocol numbering, severity
   string variants) live in `_common/*.limpid` helpers, not in
   the limpid runtime. New vendors are pure-DSL additions.

## Pipeline granularity

Every parser exposes three entry points; pick whichever matches your
event stream:

| Granularity | Example | When to use |
|-------------|---------|-------------|
| **Leaf** (zero dispatch cost) | `parse_fortigate_cef_traffic` | You know the format and subtype |
| **Vendor** (subtype auto-detect) | `parse_fortigate` | You know the vendor; format / subtype vary per event |
| **All-CEF** (vendor auto-detect) | `parse_cef` | Mixed-vendor CEF stream from a single collector |

All three produce identical OCSF output for a given input. Composers
work the same way — `compose_ocsf` dispatches on `workspace.class_uid`,
or you can call `compose_ocsf_<class>` directly.

## Authoring conventions

If you contribute a new snippet, see the internal style guide in
`_PLAN_V050_SNIPPET_LIBRARY.md`. Highlights:

- One file per vendor (parsers) / per OCSF class (composers).
- File header: vendor / source description, link to spec, sample event.
- Every `def process` carries `@requires` / `@produces` doc comments.
- Helpers go in `_common/`, prefixed with `_` to signal "internal".
- Use `include` only at the top of the file; the loader resolves the
  graph recursively (no manual ordering required).

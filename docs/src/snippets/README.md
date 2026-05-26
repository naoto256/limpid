# Snippets

limpid pipelines compose from `def input` / `def process` /
`def output` / `def function` / `def pipeline` declarations. A **snippet** is a
DSL file that bundles a related set of such declarations and is
brought into a config with a single `include` line:

```limpid
include "/path/to/snippet.limpid"
```

There is no separate snippet runtime: once included, a snippet's
processes and functions are indistinguishable from any others in
the config. The snippet boundary exists for *distribution* —
versioned files that operators drop into a config, edit on the
fly (DSL source, no recompile), and SIGHUP to reload.

limpid is agnostic about who supplies snippets. The config
loader's `SYSTEM_SNIPPET_DIR` allow-list controls which
filesystem prefixes a config may `include` from; the default is
`/usr/share/limpid/snippets/` (the official pack, below), but
operators are free to extend the allow-list to other prefixes and
ship their own snippet packs.

## The official snippet pack

A maintained set of vendor parsers, target-schema composers,
shared helpers, and a noise filter is shipped with the `limpid`
package and installed under `/usr/share/limpid/snippets/`.
Operators get vendor logs into a SIEM / data lake in OCSF form by
adding two `include` lines to their config — no parser to write
from scratch, no recompile when a vendor adds a field.

**The pack's design contracts, authoring conventions, and
per-parser status are documented in the pack's own README.** It
ships with the package (open
`/usr/share/limpid/snippets/README.md` after install) and is
also the canonical source viewable in the repo at
[`packaging/snippets/README.md`](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md).

The pack is one possible distribution, not a mandatory one. A
third party can ship a different pack adopting different
conventions; the contracts in the official pack's README are a
reference point, not a requirement of the engine.

## What's in the official pack

The inventory of parsers / composers / filters / shared helpers
shipped today, with per-source verification status, OCSF mapping
detail, and sample wire, is maintained in the pack's own README
(linked above). That is the canonical source — duplicating it
here invites drift. A few highlights worth surfacing for readers
who stop at the docs page:

- **Parsers** cover security devices and cloud audit (FortiGate,
  PAN-OS, Cisco ASA, AWS CloudTrail, Juniper SRX, Check Point,
  Trellix NSP), OSS NDR (Suricata, Zeek across default / SOC /
  full coverage tiers), and server / host vocabulary (OpenSSH,
  sudo, Apache/Nginx access logs, Postfix, Windows Security event
  log, Sysmon, BIND, Linux auditd).
- **Composers** include `compose_ocsf` (OCSF 1.3.0, 27-class
  priority set), `compose_rfc5424` (journald → RFC 5424 edge
  re-framer), and `compose_replayable` (raw-wire capture for
  parser regression / replay).
- **Shared helpers** in `functions/` cover IANA protocol number
  lookup, HTTP method → OCSF activity_id, and an LPL RFC 3164
  timestamp parser.

### Filebeat-flattened JSON

Some upstreams (Filebeat / Logstash JSON emitters used by Zeek
and Suricata modules, certain Splunk HEC sources, OpenSearch
ingest pipelines) emit nested JSON with dotted top-level keys —
`{"id.orig_h": "1.1.1.1"}` instead of `{"id": {"orig_h":
"1.1.1.1"}}`. limpid DSL does not expose bracket-subscript access
by design, so dotted keys are unreachable from a parser without
normalising. The Rust primitive `nest_dotted_keys(obj)`
un-flattens them; the Zeek `_flat` entry points use it
internally. For any other Filebeat-shaped wire, the packaging
README's *`nest_dotted_keys` primitive* section documents the
wrapping pattern.

For the design contracts behind these snippets — the
`workspace.limpid` intermediate language, the loud-fail-fast
policy on unsupported vocabulary, the transport-agnostic intake
schema, and authoring guidance for new parsers — read the pack's
own
[`packaging/snippets/README.md`](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md).

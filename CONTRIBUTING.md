# Contributing

This is a personal project. Bug reports and feature requests via Issues are welcome.

Pull requests are not accepted at this time.

## Public documentation and distributed files

Describe provenance by what the data is: its format, source product, public
corpus name, or synthetic nature. Do not describe where it was observed:
deployment names, host aliases, device models, machine counts, or whether the
environment was production. Example configuration values are distinct
from claims about an author's environment.

Keep development and release procedures out of user-facing documentation and
distributed READMEs. Put those procedures here; user pages should explain
configuration, observable behavior, limits, and recovery.

## Authoring conventions

Every snippet has one file-level facade header followed by a contract block
immediately above each public member. The file's parent directory still
determines its inventory kind.

| Scope | Required keys (canonical order) |
|---|---|
| parser file | `Facade`, `Category`, `Test corpus` |
| composer / filter / function file | `Facade`, `Test corpus` |
| facade process | `Process`, `Summary`, `Reads`, `Writes` |
| facade function | `Function`, `Summary`, `Signature` |

`Facade:` is a comma-separated list of `process <name>` and `function <name>`
entries. A leading-space continuation line extends the list. Every listed
member must have exactly one adjacent member block whose heading and following
definition agree. A top-level definition omitted from `Facade:` is private to
the file and has no member block; an unlisted member block is an error.

```limpid
// Facade: process parse_example, process example_to_otlp,
//         function example_kind
// Category: Vendor-neutral
// Test corpus: synthetic (header examples)
//
// File-wide design prose follows here.

// Process: parse_example
// Summary: Normalize one example event into canonical facts.
// Reads: workspace.example.*
//        .body (required, Object)
//        workspace.example_config.*
//        .mode (optional, String)
// Writes: workspace.lsis.parsed.*
//         .class_uid (required, Int)
def process parse_example { ... }
```

**Governing principle.** A header holds only knowledge the author
alone knows. Anything derivable from other keys, other files, or
the body is banned from the header and surfaces in the generated
inventory instead — that is why parsers carry `Category` but
composers / filters do not (their axis would duplicate
their process contracts), and why `Used by:` for a function is derived
by the inventory generator rather than authored.

### `Reads:` / `Writes:` — per-process contract grammar

Every facade process declares its input and output contracts. A contract is
either a reserved event field or one or more workspace roots:

- **Raw wire.** First token `ingress` — the snippet reads bytes off
  the wire. Dot-line intake rows are forbidden.

  ```
  // Reads:       ingress (raw wire) — syslog-wrapped %ASA messages
  ```

- **Workspace roots.** A root is `workspace.<ns>.*`. Each root owns the
  following dot-lines until the next root and requires at least one dot-line.
  Multiple roots are permitted:

  ```
  // Reads: workspace.openssh.*
  //        .body       (required, String)  — sshd body
  //        .pid        (optional, String)  — process id
  //        workspace.pipeline_options.*
  //        .hostname   (optional, String)  — source-backed override
  ```

  Each dot-line matches the regex
  `^\.<IDENT>\s+\((required|optional), <String|Int|Float|Bool|Object|Array|Timestamp|Bytes>\)`.
  Trailing prose after the closing paren is permitted. Leading
  underscores in the identifier are permitted (journald's `_PID` /
  `__REALTIME_TIMESTAMP` are real intake fields).

Ambient event metadata (`received_at`, `source`, `ingress` when
passed through unchanged, `hostname()`) is not declared in `Reads:`
— every process can read those unconditionally.

`Writes:` uses the same multi-root grammar. It additionally accepts the
reserved `egress` and `ingress` forms. Parsers normally write canonical facts
under `workspace.lsis.parsed.*`; transport and format parsers instead name
their transport namespace. Composers write a composed-layer slot, while their
terminal facade process writes `egress`.

### `Category:` — parser-only, closed vocabulary

Parsers pick a `Category:` value from the 17-entry whitelist in
`crates/xtask/src/inventory.rs::CATEGORIES`. The inventory table
groups rows by this key. To add a category, extend the slice and
document the addition here.

Composers and filters have no `Category:` axis — their process contracts expose
the relevant composition slot or pass/drop boundary.

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
inventory blocks in `packaging/snippets/README.md` are in sync with current headers — CI uses
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

### Inventory maintenance

The installed snippet root is the `SYSTEM_SNIPPET_DIR` allow-list in
`crates/limpid/src/config.rs`. The directory name selects the snippet kind
for `cargo xtask lint-snippet-headers`. The daemon does not enforce LSIS;
pack consistency is maintained by the repository checks.

`cargo xtask gen-snippet-inventory` regenerates the four marked tables in
`packaging/snippets/README.md`. Retain the `BEGIN: inventory:*` and
`END: inventory:*` markers. Check the result with
`cargo xtask gen-snippet-inventory --check` before committing.

To inspect the terminal egress writers, run
`rg 'egress = ' packaging/snippets/`. Only the companion
`<slot>_to_egress` process should assign the completed output bytes.

## Windows package development and release

For each unqualified architecture, test installation, first start, startup failure, PARAMCHANGE,
STOP with queued events, upgrade, and uninstall on an elevated test machine.
Verify the service stays stopped after initial installation; its account can
read config and write state/logs; an unrelated standard user cannot connect to
the control pipe or read LTP keys; and upgrade/uninstall preserve existing
configuration and bookmarks. Native foreground tests do not establish SCM
behavior. Debug candidate archives are for validation, not release assets.

The tag-driven release workflow builds x64 and ARM64 MSVC ZIPs with Kafka
support alongside the Debian packages. Archives are named
`limpid-<version>-<target>.zip`; the release-level `SHA256SUMS` covers each ZIP
and Debian package, while the ZIP's own manifest covers its three binaries.
The release is created only after all build jobs succeed. Native build checks
do not replace the acceptance matrix above.

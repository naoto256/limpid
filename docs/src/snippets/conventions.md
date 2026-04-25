# Snippet Authoring Conventions

These are the conventions the v0.5.0 snippet library follows. They
exist to make snippets composable: any user can drop a parser into
their pipeline and trust that it will play nicely with composers,
shared helpers, and other parsers.

If you contribute a snippet upstream, follow these rules. If you write
your own private snippets, the conventions are still recommended —
they are the trade-offs the library settled on after 5 vendor PoCs.

## 1. Re-entrant self-healing

Every parser and composer must produce the same output regardless of
which entry point the user invokes:

```limpid
process parse_fortigate_cef_traffic | compose_ocsf_network_activity   # leaf
process parse_fortigate_cef         | compose_ocsf_network_activity   # subtype
process parse_fortigate             | compose_ocsf                    # vendor
```

All three are valid entry points. They produce bit-identical egress
for a given input.

The mechanic: each layer null-checks its prerequisites and pulls them
in via `process` calls if absent. The first call wins; subsequent
guards see the value already set and skip.

```limpid
def process parse_fortigate_cef_traffic {
    if workspace.cef_name == null {
        process _parse_cef_header
    }
    // ... fill canonical fields ...
}

def process parse_fortigate_cef {
    if workspace.cef_name == null {
        process _parse_cef_header
    }
    switch workspace.cef_name { ... }
}
```

When `parse_fortigate_cef` runs, `_parse_cef_header` runs once — its
guard prevents re-parsing inside the leaf.

## 2. Positionless collections

Arrays in limpid have no `arr[n]` syntax. The library never relies on
element position. To pick an element, address it by intrinsic
identity:

```limpid
// WRONG: position is an accident of construction order
workspace.process = workspace.evidence[0]

// RIGHT: identity survives upstream insertion / deletion
workspace.process = find_by(workspace.evidence, "entityType", "Process")
```

To extend a collection, use `append` or `prepend` — both identify
"where" by insertion-order semantics rather than a numeric index that
would shift under later mutations.

```limpid
workspace.observables = append(workspace.observables, new_obs)
```

See [Arrays](../processing/user-defined.md#arrays) for the full
rationale.

## 3. Schema knowledge in DSL, not Rust

Vendor-specific quirks (CEF protocol numbering, severity string
casing, OCSF class shapes) live in `_common/*.limpid` snippets, never
in the limpid runtime. New vendors and OCSF classes are pure-DSL
additions.

This rules out a Rust primitive for "given a CEF event, produce OCSF
Network Activity" — that would couple the runtime to specific schemas.
The runtime ships only schema-agnostic primitives (`cef.parse`,
`to_int`, `find_by`, …); the schema mapping is the snippet's job.

## File layout

```
packaging/snippets/
├─ _common/         shared helpers (file name = subject area)
├─ parsers/         one file per vendor (= per CEF/syslog/JSON source)
└─ composers/       one file per OCSF class
```

The file granularity rules are:

- **Parsers**: one `.limpid` file per vendor. Multiple subtypes
  (`*_cef_traffic`, `*_cef_utm_ips`, `*_syslog_traffic`) co-locate in
  the same file because they share the vendor-level dispatcher and
  helpers.
- **Composers**: one file per OCSF class UID, named
  `ocsf_<class_lower>.limpid`.
- **Helpers** (`_common/`): one file per logical concern (severity
  mapping, protocol mapping, …). Files starting with `_` mark
  internal-use helpers; user-facing entry points have no leading `_`.

## Naming

| Layer | Pattern | Examples |
|-------|---------|----------|
| Leaf parser | `parse_<vendor>_<format>_<subtype>` | `parse_fortigate_cef_traffic`, `parse_paloalto_syslog_threat` |
| Subtype dispatcher | `parse_<vendor>_<format>` | `parse_fortigate_cef` |
| Vendor dispatcher | `parse_<vendor>` | `parse_fortigate` |
| Format dispatcher | `parse_<format>` | `parse_cef` |
| OCSF composer leaf | `compose_ocsf_<class_lower>` | `compose_ocsf_network_activity` |
| OCSF composer dispatcher | `compose_ocsf` | (single, switches on `workspace.class_uid`) |
| Internal helper | `_<verb>_<subject>` | `_parse_cef_header`, `_normalize_severity` |

## File header template

Every snippet file begins with:

```limpid
// <Title>: <one-line summary>.
//
// <Vendor / source description and link to spec.>
//
// Sample event (truncated for readability):
//   <a few lines of representative input>
//
// Format dimensions tested: <CEF, JSON, CSV, etc.>
```

Every `def process` carries `@requires` / `@produces` doc comments
describing the workspace contract:

```limpid
// @requires (workspace): cef_*, syslog_msg
// @produces (workspace): src_endpoint, dst_endpoint, connection_info,
//                        activity_id, class_uid, severity_id, metadata
def process parse_fortigate_cef_traffic {
    ...
}
```

The analyzer doesn't yet enforce these — the comment is for human
maintainers. Future versions of `limpid --check` may parse them to
flag contract drift.

## Composer template

Composers are mechanical:

1. Build `workspace.ocsf` as a hash literal of canonical workspace
   fields plus class-specific constants.
2. `egress = to_json(workspace.ocsf)`.

```limpid
def process compose_ocsf_network_activity {
    workspace.ocsf = {
        // --- Base event (shared across all OCSF classes) ---
        class_uid: 4001,
        category_uid: 4,
        class_name: "Network Activity",
        category_name: "Network Activity",
        type_uid: 400100 + workspace.activity_id,
        activity_id: workspace.activity_id,
        severity_id: workspace.severity_id,
        time: received_at,
        metadata: workspace.metadata,

        // --- Class-specific fields (pluck from canonical workspace) ---
        src_endpoint: workspace.src_endpoint,
        dst_endpoint: workspace.dst_endpoint,
        connection_info: workspace.connection_info,

        // --- Standard optional enrichment slots ---
        observables: workspace.observables,
        enrichments: workspace.enrichments
    }
    egress = to_json(workspace.ocsf)
}
```

`null` workspace values pass through unchanged; OCSF validators tolerate
optional-field absence.

## Tests

Every snippet ships with at least one sample event in its file header.
A future test harness will inject those samples through the snippet
and validate the egress against the OCSF JSON Schema. Until that lands,
authors should run their snippet through `limpid --test-pipeline` with
the sample event and confirm:

- `--check` passes
- The egress matches the OCSF spec for the target class
- No workspace fields outside the documented `@produces` set leak
  into `egress` (the composer's pluck list is the contract)

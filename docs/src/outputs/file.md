# file

Appends event `egress` bytes to a local file. Supports dynamic path templates (full DSL expressions) and file permission control.

## Configuration

```
def output archive {
    type file
    path "/var/log/limpid/archive.log"
    mode "0640"
    owner "syslog"
    group "adm"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `path` | yes | — | File path (literal, or a template with `${...}`) |
| `mode` | no | system default | Octal file permissions (e.g., `"0640"`) |
| `owner` | no | process user | File owner (requires `CAP_CHOWN`) |
| `group` | no | process group | File group |

Permissions are applied only when the file is first created.

## Dynamic path templates

`path` can contain `${...}` interpolations that are evaluated per event. Only **event-intrinsic** fields are addressable from output config: `source` (the input layer's peer address), `received_at` (ingress timestamp), and `ingress` (raw bytes). Pipeline-mutable state — `workspace`, `egress`, `error` — is **rejected at parse time** by the analyzer and the daemon both, so a template that references them never reaches runtime. (Routing decisions that depend on pipeline-internal state belong in the pipeline body — split traffic into multiple `def output` blocks, each with its own static or event-intrinsic destination, and use `if`/`switch` in the pipeline to pick which output an event goes to.)

See [DSL Syntax Basics → String interpolation](../dsl-syntax.md#string-interpolation) for the full interpolation syntax. The short version:

```
def output per_source {
    type file
    path "/var/log/limpid/${source.ip}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def output rolled_daily {
    type file
    path "/var/log/limpid/all/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}
```

Any DSL expression over event-intrinsic fields is allowed inside `${...}` — identifiers (`source.ip`, `received_at`), function calls (`strftime`, `lower`, `regex_extract`) over those identifiers, string concatenation with `+`, and so on. There are no hardcoded placeholders; for calendar components, call `strftime(received_at, ...)` explicitly.

For per-tenant or per-content routing — anything that depends on what an earlier process parsed out — split into multiple outputs from the pipeline:

```
def output siem_apac { type file path "/var/log/limpid/apac.log" }
def output siem_emea { type file path "/var/log/limpid/emea.log" }

def pipeline route {
    input syslog_udp
    process parse_fortigate | classify_region   // sets workspace.region
    switch workspace.region {
        "apac" { output siem_apac }
        "emea" { output siem_emea }
    }
}
```

The output configs stay static; the pipeline body decides which output each event reaches.

### Sanitisation

Path interpolation goes through three safety passes that together make directory escape impossible.

**Pass 1 — per-interpolation slash normalise + empty-result reject.** Every `${...}` interpolation in the path template — `${source.ip}`, `${strftime(received_at, "%Y-%m-%d", "local")}`, `${lower(source.ip)}`, all of them — has `/` and `\` in the resulting string replaced with `_`. An interpolation that evaluates to the empty string is rejected with an error (it would silently produce surprise paths like `/foo//bar` or `/foo/.log`).

> The invariant is "**one interpolation = one non-empty path component**". Directory structure must be expressed in the literal parts of the template:
>
> ```
> path "/var/log/${source.ip}/${strftime(received_at, "%Y-%m-%d", "local")}.log"   // OK — hierarchy is literal
> ```
>
> If an event-intrinsic value happens to contain a slash (rare on `source.ip`, but possible on `strftime` format strings that include path separators), it becomes `_` rather than spawning subdirectories. To split into directories, place each piece in its own interpolation slot.
>
> An empty interpolation result almost always reflects a misconfigured `strftime` format or a degenerate input (e.g. a Unix socket without a remote peer, where `source` falls back to a sentinel), so it's rejected up front rather than silently producing `/foo//bar` or `/foo/.log`. If a slot is genuinely optional in your wiring, build the literal hierarchy without it.
>
> Dots are NOT stripped — interpolations contributing to FQDN-style filenames work as expected (`${source.ip}.log` for `10.0.0.1` produces `10.0.0.1.log`).

**Pass 2 — `..` traversal reject on the fully-rendered path.** After all interpolations resolve and the literal+interpolation parts are joined into a single path string, the result is split on `/` and any exact `..` component fails the write with a loud error. Silently rewriting `..` would quietly redirect writes to a different file, so per Principle 1 (zero hidden behaviour) it is refused rather than stripped. Since Pass 1 already normalises slashes inside each interpolation, no single value can introduce a `..` segment of its own; this pass is the cross-literal catch:

```
path "/var/log/../x.log"   // → Pass 2 rejects: `..` traversal component
```

Unusual but harmless dirnames like `...` or `..foo` pass through — only the exact `..` token in any path position is rejected.

**Pass 3 — trailing-slash and empty-result reject.** A rendered path that ends in `/` (directory reference, not a file) or collapses to `""` fails with an explicit error rather than deferring to the OS. Both shapes almost always reflect a misconfigured template — leaving them to surface as `EISDIR` at write time buries the real cause under a low-level syscall error.

The three passes together guarantee that the final write path stays within the directory tree the operator declared in the template, has a non-empty filename component, and never terminates in a directory reference — regardless of what arrives on the wire.

Parent directories are created automatically.

## Notes

- Each line is one event's `egress` bytes followed by a newline.
- For log rotation, use `logrotate` with `copytruncate` or `create` + SIGHUP.
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).

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

`path` can contain `${...}` interpolations that are evaluated per event against the full DSL. See [DSL Syntax Basics → String interpolation](../dsl-syntax.md#string-interpolation) for the full syntax; the short version:

```
def output per_source {
    type file
    path "/var/log/limpid/${source}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def output per_host {
    type file
    path "/var/log/limpid/${workspace.hostname}.log"
}
```

Any DSL expression is allowed inside `${...}` — identifiers (`source`, `workspace.xxx`), function calls (`strftime`, `lower`, `regex_extract`), string concatenation with `+`, and so on. There are no hardcoded placeholders; for calendar components, call `strftime(received_at, ...)` explicitly.

### Sanitisation

Path interpolation goes through two safety passes that together make directory escape impossible.

**Pass 1 — per-interpolation slash strip + empty-result reject.** Every `${...}` interpolation in the path template — `${workspace.hostname}`, `${lower(workspace.host)}`, `${source}`, `${a + "-" + b}`, all of them — has `/` and `\` in the resulting string replaced with `_`. An interpolation that evaluates to the empty string is rejected with an error (it would silently produce surprise paths like `/foo//bar` or `/foo/.log`).

> The invariant is "**one interpolation = one non-empty path component**". Directory structure must be expressed in the literal parts of the template:
>
> ```
> path "/var/log/${workspace.region}/${workspace.host}.log"   // OK — hierarchy is literal
> ```
>
> If a workspace value happens to contain a slash (e.g. `workspace.path = "asia/tokyo"`), it becomes `_` rather than spawning subdirectories. To split into directories, parse the value into pieces explicitly and place each piece in its own interpolation slot.
>
> An empty interpolation result almost always reflects a config or data bug — a null workspace value, or an unintended Pass-2 collapse — so it's rejected up front rather than silently producing `/foo//bar` or `/foo/.log`. If a value is genuinely optional in your pipeline, build the final path string in a `process` first (with whatever default / fallback your case wants) and reference the resulting workspace key from the path template.
>
> Dots are NOT stripped — interpolations contributing to FQDN-style filenames work as expected (`${workspace.host}.log` → `web01.example.com.log`).

**Pass 2 — `..` traversal strip on the fully-rendered path.** After all interpolations resolve and the literal+interpolation parts are joined into a single path string, every `../` sequence is removed (iterated to a fixpoint), a trailing `/..` is stripped, and a result of exactly `..` is emptied. This catches traversal that arises from concatenation across literals and interpolations even when no single piece contains a slash:

```
path "/var/log/${workspace.parent}/x.log"   // parent="..", evaluated path="/var/log/../x.log"
                                            // → Pass 2 strips "../" → "/var/log/x.log"
```

**Pass 3 — empty-path reject.** If Pass 2 collapsed the entire rendered path to `""` (e.g. the template was a single `${".."}`-shaped interpolation), error explicitly. Trailing-slash and directory-target cases are left to the OS — `EISDIR` / `ENOTDIR` give the same diagnostic surface either way.

The three passes together guarantee that the final write path stays within the directory tree the operator declared in the template, with a non-empty filename component, regardless of what arrives in workspace.

Parent directories are created automatically.

## Notes

- Each line is one event's `egress` bytes followed by a newline.
- For log rotation, use `logrotate` with `copytruncate` or `create` + SIGHUP.

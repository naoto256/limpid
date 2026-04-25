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

`path` can contain `${...}` interpolations that are evaluated per event against the full DSL. See [String Templates](../processing/templates.md) for the full syntax; the short version:

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

Interpolations that read `workspace.*` directly (e.g. `${workspace.hostname}`) have `/`, `\`, and `..` replaced with `_` before substitution, so a hostile or malformed workspace value cannot escape the configured directory. Interpolations that compute a value (including `${lower(workspace.host)}`) are **not** auto-sanitised; if you need the guardrail on a computed value, add it explicitly with `regex_replace`.

Event metadata like `${source}` and results of functions like `${strftime(received_at, ...)}` are substituted verbatim.

Parent directories are created automatically.

## Notes

- Each line is one event's `egress` bytes followed by a newline.
- For log rotation, use `logrotate` with `copytruncate` or `create` + SIGHUP.

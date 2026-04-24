# User-defined Processes

You can define custom processes using the DSL. They compose built-in processes, expression functions, and control flow.

## Defining a process

```
def process enrich {
    process parse_kv

    if workspace.srcip != null {
        workspace.geo = geoip(workspace.srcip)
    }

    egress = format("%{devname} %{srcip} -> %{dstip} %{action}")
}
```

## Assignments

| Target | Effect |
|--------|--------|
| `egress = expr` | Replace the bytes the output will write |
| `facility = expr` | Set facility (0-23) and rewrite `<PRI>` in `egress` |
| `severity = expr` | Set severity (0-7) and rewrite `<PRI>` in `egress` |
| `workspace.xxx = expr` | Set a workspace value (nested: `workspace.geo.country = "JP"`) |

### Important: what is and isn't reflected in output

**`egress`** is the byte buffer that output modules write to the wire. If you want to change what gets sent, you must change `egress`:

```
// This changes the output:
egress = format("%{hostname}: %{syslog_msg}")

// This does NOT change the output:
workspace.hostname = "new-host"
// ↑ sets a workspace value, but `egress` is unchanged
```

`workspace` is a pipeline-local scratch area for intermediate values — parsed data, enrichment results, routing decisions. Workspace values are **not** automatically serialised into `egress`. To include them in the output, explicitly rebuild `egress`:

```
process parse_kv                             // parse into workspace
egress = to_json()                           // serialise all workspace values as JSON
// or
egress = format("%{srcip} -> %{dstip}")      // build a custom format
```

### PRI rewriting

`facility` and `severity` are special: assigning to them also rewrites the `<PRI>` header in `egress` (if one exists). This is the only case where a metadata assignment automatically modifies `egress`.

```
def process ama_rewrite {
    if contains(ingress, "CEF:") {
        facility = 16
    } else {
        facility = 17
    }
    severity = 6
}
```

## Control flow

### if / else if / else

```
if severity <= 3 {
    workspace.priority = "high"
} else if severity <= 5 {
    workspace.priority = "medium"
} else {
    workspace.priority = "low"
}
```

### switch

```
switch workspace.device_vendor {
    "Fortinet" {
        process parse_kv
    }
    "CheckPoint" {
        process parse_cef
    }
    default {
        drop
    }
}
```

### try / catch

Catches errors from process execution. The error message is available as `error` inside the catch block.

```
try {
    process parse_json
} catch {
    workspace.parse_error = error
}
```

### foreach

Iterates over an array value in `workspace`. The current item is available as `workspace._item`.

```
foreach workspace.items {
    workspace.count = workspace.count + 1
}
```

### process call

Calls another named process or a built-in process:

```
process parse_cef
process my_custom_enrichment
process regex_replace("\\d{3}-\\d{4}", "XXX-XXXX")
```

### drop

Terminates the event immediately. The event is counted as `events_dropped`:

```
if contains(ingress, "healthcheck") {
    drop
}
```

## Using in pipelines

Reference a user-defined process by name:

```
def pipeline main {
    input syslog
    process enrich             // calls the process defined above
    output archive
}
```

Or chain with built-in processes:

```
process strip_pri | enrich | {
    egress = to_json()
}
```

# User-defined Processes

You define a process with `def process <name> { ... }`. Inside the body you call functions, assign to event slots and workspace, branch with `if` / `switch` / `try`, iterate with `foreach`, and call other processes by name.

## Defining a process

```
def process enrich_fortigate {
    parse_kv(egress)                                  // merge KV pairs into workspace

    if workspace.srcip != null {
        workspace.geo = geoip(workspace.srcip)
    }

    egress = format("%{workspace.devname} %{workspace.srcip} -> %{workspace.dstip} %{workspace.action}")
}
```

`parse_kv(egress)` here is a bare function-call statement. Because it returns an object, the object's keys are merged into `workspace`. See [Expression Functions](./functions.md#bare-statements-vs-assignments) for the full rule.

## Assignments

| Target | Effect |
|--------|--------|
| `egress = expr` | Replace the bytes the output will write |
| `workspace.xxx = expr` | Set a workspace value (nested: `workspace.geo.country = "JP"`) |
| `let name = expr` | Bind a process-local scratch value (visible only inside this process body) |

Anything else on the left of `=` is rejected as an unknown assignment target.

> **What about `facility = ...` / `severity = ...`?** Those metadata fields were removed from the Event core in 0.3. To set or rewrite the syslog `<PRI>` byte, use the explicit `syslog.set_pri(egress, facility, severity)` function. To read it back, use `syslog.extract_pri(egress)`. See [Upgrading to 0.3](../operations/upgrade-0.3.md#event-core-facility--severity-removed).

### Important: what is and isn't reflected in output

**`egress`** is the byte buffer that output modules write to the wire. If you want to change what gets sent, you must change `egress`:

```
// This changes the output:
egress = format("%{workspace.syslog_hostname}: %{workspace.syslog_msg}")

// This does NOT change the output:
workspace.syslog_hostname = "new-host"
// ↑ sets a workspace value, but `egress` is unchanged
```

`workspace` is a pipeline-local scratch area for intermediate values — parsed data, enrichment results, routing decisions. Workspace values are **not** automatically serialised into `egress`. To include them in the output, explicitly rebuild `egress`:

```
parse_kv(egress)                              // parse into workspace
egress = to_json()                            // serialise the whole event as JSON
// or
egress = format("%{workspace.srcip} -> %{workspace.dstip}")
```

### Rewriting the syslog PRI

```
def process ama_rewrite {
    if contains(ingress, "CEF:") {
        egress = syslog.set_pri(egress, 16, 6)   // local0.info for CEF
    } else {
        egress = syslog.set_pri(egress, 17, 6)   // local1.info for everything else
    }
}
```

## Control flow

### if / else if / else

```
let pri = syslog.extract_pri(ingress)
let severity = pri % 8

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
switch workspace.cef_device_vendor {
    "Fortinet" {
        parse_kv(egress)
    }
    "CheckPoint" {
        cef.parse(ingress)
    }
    default {
        drop
    }
}
```

### try / catch

Catches errors raised inside the body. The error message is available as `error` inside the catch block.

```
try {
    parse_json(egress)
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

Calls another named process:

```
process enrich_fortigate
process my_custom_enrichment
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
    process enrich_fortigate
    output archive
}
```

Or chain with other processes (named or inline):

```
process strip_headers | enrich_fortigate | {
    egress = to_json()
}
```

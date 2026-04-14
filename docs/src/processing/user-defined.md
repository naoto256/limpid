# User-defined Processes

You can define custom processes using the DSL. They compose built-in processes, expression functions, and control flow.

## Defining a process

```
def process enrich {
    process parse_kv

    if fields.srcip != null {
        fields.geo = geoip(fields.srcip)
    }

    message = format("%{devname} %{srcip} -> %{dstip} %{action}")
}
```

## Assignments

| Target | Effect |
|--------|--------|
| `message = expr` | Replace message content |
| `facility = expr` | Set facility (0-23) and rewrite `<PRI>` in message |
| `severity = expr` | Set severity (0-7) and rewrite `<PRI>` in message |
| `fields.xxx = expr` | Set a field (nested: `fields.geo.country = "JP"`) |

### Important: what is and isn't reflected in output

**`message`** is what gets written by output modules. If you want to change the output, you must change `message`:

```
// This changes the output:
message = format("%{hostname}: %{syslog_msg}")

// This does NOT change the output:
fields.hostname = "new-host"
// ↑ sets a field value, but the message content is unchanged
```

`fields` is a working space for intermediate values — parsed data, enrichment results, routing decisions. Fields are **not** automatically serialized into the output message. To include field values in the output, explicitly rebuild the message:

```
process parse_kv                              // parse into fields
message = to_json()                           // serialize all fields as JSON
// or
message = format("%{srcip} -> %{dstip}")     // build a custom format
```

### PRI rewriting

`facility` and `severity` are special: assigning to them also rewrites the `<PRI>` header in the message (if one exists). This is the only case where a metadata assignment automatically modifies the message content.

```
def process ama_rewrite {
    if contains(raw, "CEF:") {
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
    fields.priority = "high"
} else if severity <= 5 {
    fields.priority = "medium"
} else {
    fields.priority = "low"
}
```

### switch

```
switch fields.device_vendor {
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
    fields.parse_error = error
}
```

### foreach

Iterates over an array field. The current item is available as `fields._item`.

```
foreach fields.items {
    fields.count = fields.count + 1
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
if contains(raw, "healthcheck") {
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
    message = to_json()
}
```

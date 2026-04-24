# Routing

Pipelines support conditional routing with `if/else` and `switch` statements.

## if / else if / else

```
def pipeline main {
    input syslog

    if severity <= 3 {
        output alert
    }
    output siem
}
```

## switch

Route events based on a value:

```
def pipeline archive {
    input syslog_udp
    process strip_pri

    switch source {
        "192.0.2.1" {
            output fw01
        }
        "192.0.2.2" {
            output fw02
        }
        "192.0.2.3" {
            if contains(ingress, "type=\"traffic\"") {
                drop
            }
            process prepend_source | prepend_timestamp
            output fw03
        }
        default {
            drop
        }
    }
}
```

## Multi-output

Since `output` is non-terminal, you can send to multiple destinations:

```
def pipeline main {
    input syslog

    // Archive raw log first
    output archive

    // Parse and enrich
    process parse_cef | {
        workspace.geo = geoip(workspace.src)
        egress = to_json()
    }

    // Send enriched version to SIEM
    output siem
}
```

The archive receives the raw bytes, the SIEM receives the enriched JSON — from the same pipeline.

## Combining filtering and routing

```
def pipeline ama_forward {
    input ama_tcp

    // Filter noise first
    process filter_chargen | filter_fortinet_traffic

    // Rewrite and forward
    process ama_rewrite
    output ama
}
```

Filters at the top of the pipeline drop unwanted events before they reach any output.

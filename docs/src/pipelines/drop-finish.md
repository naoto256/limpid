# drop and finish

limpid has two ways to terminate a pipeline for an event:

## drop

Discards the event. Counted as `events_dropped` in metrics. Use for filtering.

```
def process filter_noise {
    if contains(ingress, "healthcheck") {
        drop
    }
    if contains(ingress, "CHARGEN") {
        drop
    }
}
```

## finish

Explicitly terminates the pipeline. Counted as `events_finished` (or `events_discarded` if no output was reached).

```
def pipeline main {
    input syslog
    output archive
    finish               // explicit, but optional here
}
```

## Implicit finish

Reaching the end of a pipeline without `drop` or `finish` is an **implicit finish**. These are equivalent:

```
// Explicit
def pipeline main {
    input syslog
    output archive
    finish
}

// Implicit (recommended)
def pipeline main {
    input syslog
    output archive
}
```

## When to use each

| Scenario | Use |
|----------|-----|
| Filtering out unwanted events | `drop` |
| Normal pipeline completion | Implicit finish (just let it end) |
| Early exit from a branch | `finish` |
| Unknown/unexpected source | `drop` in `default` branch |

## Metrics impact

| Termination | Metric |
|-------------|--------|
| `drop` | `events_dropped` |
| `finish` or end of pipeline (with output) | `events_finished` |
| `finish` or end of pipeline (no output) | `events_discarded` |

`events_discarded` indicates a possible misconfiguration — the event went through the pipeline but was never sent anywhere.

## Example: filtering + routing

```
def pipeline archive {
    input syslog_udp
    process { egress = syslog.strip_pri(egress) } | filter_noise

    switch source {
        "192.0.2.1" {
            output fw01                    // implicit finish
        }
        "192.0.2.3" {
            if contains(ingress, "type=\"traffic\"") {
                drop                       // filter: events_dropped
            }
            output fw03                    // implicit finish
        }
        default {
            drop                           // unknown source: events_dropped
        }
    }
}
```

# drop, finish, and error

limpid has three ways to explicitly terminate a pipeline for an event, plus an implicit finish at the end of the body:

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

## error

Routes the event to the [error log](../operations/error-log.md) (DLQ) with an operator-readable reason. Counted as `events_errored` — same metric as runtime process failures, because both signal "this event could not be processed and the operator should know."

```
def process parse_fortigate_cef {
    workspace.cef = cef.parse(workspace.syslog.msg)
    switch workspace.cef.name {
        "traffic" { process parse_fortigate_cef_traffic }
        "utm"     { process parse_fortigate_cef_utm }
        default   { error "unsupported FortiGate CEF subtype: ${workspace.cef.name}" }
    }
}
```

`error` takes an optional message expression. The expression is rendered (string interpolation, function calls, anything an `${...}` template can do) and stored in the error_log entry's `reason` field. Use it to capture **why** the event was rejected so the operator doesn't have to reverse-engineer it from the raw bytes:

```
def process expect_field {
    if workspace.user.id == null {
        error "missing required field user.id"
    }
}
```

A bare `error` (no message) emits a generic default. Prefer a message — by the time the event lands in the DLQ, the operator has lost the local context that made the error obvious.

`error` is distinct from `drop`:

- `drop` says "this event is uninteresting" — it's filtering, the silence is intentional.
- `error` says "this event was supposed to be processable but wasn't" — operator action needed.

It's also distinct from a runtime process failure (regex compile error, type mismatch, etc.): both end up in the DLQ counted as `events_errored`, but `error` is the *intentional* signal from your own DSL, with a message you control. Useful in snippet libraries (parser dispatchers, schema-bound validators) where the parser knows it can't continue and the host pipeline shouldn't be guessing.

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
| Unknown/unexpected source (intended discard) | `drop` in `default` branch |
| Schema dispatcher hits unsupported subtype (operator should know) | `error "..."` in `default` branch |
| Required field missing on an event we *should* be able to handle | `error "..."` |

## Metrics impact

| Termination | Metric |
|-------------|--------|
| `drop` | `events_dropped` |
| `finish` or end of pipeline (with output) | `events_finished` |
| `finish` or end of pipeline (no output) | `events_discarded` |
| `error "..."` (explicit) or process raised a runtime error | `events_errored` (+ DLQ write) |

`events_discarded` indicates a possible misconfiguration — the event went through the pipeline but was never sent anywhere.

`events_errored` indicates either an explicit `error` statement or a pipeline-runtime failure (regex compile error, unknown-identifier panic at runtime, type mismatch, …). The event is *not* forwarded downstream — at the failure point the runtime has no way to produce a correct egress, and the pre-0.5 behaviour of forwarding the original `ingress` silently turned wrap / enrichment bugs into data-shape regressions at the receiving SIEM. Instead, the event is routed to the [error log](../operations/error-log.md) so operators can inspect, fix the offending config, and replay.

`events_errored_unwritable` is the subset where the DLQ write itself failed (disk full, permissions, rotation race). The runtime falls back to a structured `tracing::error!` line, but operators should alarm on this counter — a non-zero value means the replay path may be incomplete.

## Example: filtering + routing

```
def pipeline archive {
    input syslog_udp
    process { egress = syslog.strip_pri(egress) } | filter_noise

    switch source.ip {
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

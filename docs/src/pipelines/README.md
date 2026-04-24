# Pipelines

Pipelines wire inputs, processes, and outputs together. They define the flow of events from ingestion to destination.

## Basic structure

```
def pipeline main {
    input syslog                       // where events come from
    process { cef.parse(ingress) }     // transform
    output archive                     // where events go
}
```

## Key rules

- **`output`** is **non-terminal** — it deep-copies the event to the output queue and continues
- **`drop`** discards the event (counted as `events_dropped`)
- **`finish`** terminates the pipeline successfully (counted as `events_finished`)
- Reaching the **end** of a pipeline is an implicit `finish`
- If a pipeline finishes without hitting any `output`, the event is counted as `events_discarded`

## Pipeline statements

| Statement | Effect |
|-----------|--------|
| `input <name>` | Declares the input source |
| `input <a>, <b>, ...` | Declares multiple input sources (fan-in) |
| `process <name>` | Runs a process (or chain with `\|`) |
| `process { ... }` | Inline (anonymous) process |
| `output <name>` | Copies event to output queue (non-terminal) |
| `drop` | Discards event (terminal) |
| `finish` | Ends pipeline successfully (terminal) |
| `if ... { } else { }` | Conditional branching |
| `switch ... { }` | Multi-way routing |

## Fan-out

Multiple pipelines can share the same input. Each pipeline gets an independent copy of every event:

```
def pipeline archive {
    input syslog
    output file_archive
}

def pipeline forward {
    input syslog          // same input, different pipeline
    process enrich
    output siem
}
```

## Fan-in

Symmetric to fan-out: a single pipeline can subscribe to multiple inputs. List them comma-separated on the `input` line. Events from every listed input are merged at the pipeline entrance and processed by the same body.

```
def pipeline ha_syslog {
    input syslog_a, syslog_b        // both feed this pipeline
    process normalize
    output siem
}
```

Typical use: HA syslog where two relays send the same events to `syslog_a` and `syslog_b`, and the pipeline deduplicates downstream (e.g. via a shared dedup table) — no need to duplicate the whole pipeline body per input.

### Semantics

- **Entrance only.** The `input a, b;` declaration must appear once, at the top of the pipeline. Merging events mid-pipeline is not supported — if you need that, use two pipelines writing to a shared output.
- **No ordering guarantee.** Events from different inputs are delivered in arrival order on the merged stream. Ordering across inputs is not preserved.
- **No per-input attribution inside the pipeline.** From the body's perspective, every event looks the same regardless of which input delivered it. If you need to tell inputs apart (for metrics, drops, token-bucket behaviour), do it at the input layer — pipeline metrics aggregate across all subscribed inputs.
- **`inject` / `tap` stay input-scoped.** `inject input syslog_a` and `tap input syslog_a` still operate on a single input by name; fan-in does not change that contract.

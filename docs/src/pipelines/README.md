# Pipelines

Pipelines wire inputs, processes, and outputs together. They define the flow of events from ingestion to destination.

## Basic structure

```
def pipeline main {
    input syslog           // where events come from
    process parse_cef      // transform
    output archive          // where events go
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

# Archive every log and forward selected events

Keep a complete local archive while sending only useful events to another syslog server. Give urgent events a separate JSON output without changing the copies already sent elsewhere.

## Branch after the archive

Each `output` takes a copy of the event at that point in the pipeline. It does not end processing. Write the archive first, then select and transform the downstream copies.

```limpid
include "/usr/share/limpid/snippets/parsers/parse_syslog.limpid"

def input syslog_local {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/all.log"
}

def output downstream {
    type syslog_tcp
    peer { host "192.0.2.20" port 514 }
}

def output urgent {
    type file
    path "/var/log/limpid/urgent.jsonl"
}

def process urgent_document {
    egress = to_json({
        message: ingress,
        hostname: workspace.syslog.hostname,
        severity: workspace.syslog.severity
    })
}

def pipeline archive_and_forward {
    input syslog_local
    output archive
    process parse_syslog

    if workspace.syslog.severity != null {
        if workspace.syslog.severity <= 4 {
            output downstream
        }
        if workspace.syslog.severity <= 3 {
            process urgent_document
            output urgent
        }
    }
}
```

Create the file directories and grant the service write access. Replace the documentation address with the downstream server, reserve the listener port, and restrict incoming traffic to intended senders. This example uses plain TCP on a trusted network; configure TLS when the transport requires it.

## Follow each copy

Syslog severity uses smaller numbers for more urgent events. This pipeline forwards warning and more urgent messages (0–4); error and more urgent messages (0–3) also get a JSON record.

| Event             | Local archive | Downstream syslog | Urgent JSON       |
| ----------------- | ------------- | ----------------- | ----------------- |
| Informational (6) | Original line | —                 | —                 |
| Warning (4)       | Original line | Original line     | —                 |
| Error (3)         | Original line | Original line     | Structured record |

The last branch rewrites `egress`, not `ingress`. The archive and syslog copies have already been taken, so both retain the original line. If parsing fails, the earlier archive output has already been queued; the error stops later processing. If parsing succeeds without a severity, neither conditional branch runs.

These are independent output queues, not an atomic transaction across destinations. Queuing an archive copy does not mean its disk write has finished, or that the remote server has received another copy. Size queues and configure durable error logging for your delivery requirements.

No `drop` is needed here: an event can simply reach the end after its archive output. A `drop` or `finish` placed before another branch would stop that branch too. Use separate `if` statements when an event should reach more than one destination; use an exclusive `switch` when only one destination should be selected.

See the [routing contract](./routing.md) for output-copy and termination semantics.

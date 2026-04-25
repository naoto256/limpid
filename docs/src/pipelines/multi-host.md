# Multi-host Pipeline Example

A small case study of a two-tier pipeline built entirely in limpid: two edge hosts (`edge01`, `edge02`, Ubuntu 24.04) ship journald logs to a central `relay` host, which rewrites and relays them to the Azure Monitor Agent (AMA) for Log Analytics Workspace (LAW) ingestion.

The example is deliberately small — one service on the edge, one forwarding pipeline in the middle, one SIEM destination. The point is not the size; it is showing **how the design principles work across a hop boundary in practice.**

## Topology

```
+---------------+          +---------------+          +---------------+
|    edge01     |          |    edge02     |          |     ...       |
|               |          |               |          |               |
| journal input |          | journal input |          |               |
|      |        |          |      |        |          |               |
| wrap_app      |          | wrap_app      |          |               |
|      |        |          |      |        |          |               |
| tcp output    |          | tcp output    |          |               |
|  (RFC 5424)   |          |  (RFC 5424)   |          |               |
+-------|-------+          +-------|-------+          +---------------+
        |                          |
        |    RFC 5424 framed       |
        |    over TCP:514          |
        +-----------+--------------+
                    |
                    v
           +------------------+
           |     relay        |
           |   10.0.0.10      |
           |                  |
           |  ama_tcp input   |
           |       |          |
           |  filter / rewrite|
           |       |          |
           |  ama output      |
           |  (tcp 127.0.0.1: |
           |       28330)     |
           +--------|---------+
                    |
                    v
           +------------------+
           |  Azure Monitor   |
           |  Agent (mdsd)    |
           |       |          |
           |       v          |
           |  Log Analytics   |
           |  Workspace       |
           |  (Syslog /       |
           |  CommonSecurity- |
           |  Log tables)     |
           +------------------+
```

Three things to notice before looking at any DSL:

1. Only RFC 5424 framed syslog crosses the wire between edge hosts and the relay. That is the hop contract.
2. The relay never sees journal fields, never sees hostnames as separate metadata, never sees anything the edge hosts did not explicitly put into the bytes. Principle 3: only `egress` crosses hop boundaries.
3. Each hop runs limpid with the same DSL. There are no vendor-specific config dialects in play (no rsyslog `omfwd`, no mdsd XML beside the AMA DCR itself).

## Edge host: edge01 / edge02

On the edge, the job is: read journald entries from `app.service`, wrap each one in a valid RFC 5424 frame, and send it on.

```limpid
// /etc/limpid/edge.limpid

def input app_journal {
    type journal
    match "_SYSTEMD_UNIT=app.service"
    state_file "/var/lib/limpid/journal/app.cursor"
}

def process wrap_app {
    // @requires: ingress  (journald line: "app[PID]: {JSON}")
    // @produces: egress   (RFC 5424 frame with PRI=<14>, MSG = original payload)
    //
    // The journal input emits lines shaped like "IDENTIFIER[PID]: MESSAGE"
    // (see docs/src/inputs/journal.md). We wrap the whole line as the MSG
    // of a minimal RFC 5424 frame, using facility=user/severity=info so the
    // edge host does not take a position on routing — the relay
    // decides the final PRI.

    egress = format(
        "<14>1 %{strftime(received_at, \"%Y-%m-%dT%H:%M:%S%.3fZ\", \"utc\")} %{hostname()} app - - - %{ingress}"
    )
}

def output to_relay {
    type tcp
    address "10.0.0.10:514"
    framing non_transparent
    queue {
        type disk
        path "/var/lib/limpid/queues/relay"
        max_size "500MB"
    }
}

def pipeline app_to_relay {
    input app_journal
    process wrap_app
    output to_relay
}
```

Three design points worth calling out:

- **PRI is deliberately neutral (`<14>` = user.info).** The edge host does not decide where the event ends up; it only guarantees a valid frame. The relay picks the final facility. This split — "upstream produces a shape, downstream picks the routing" — is only clean because Principle 3 says the contract is just bytes.
- **No parsing on the edge.** The edge host does not run `parse_json` against the payload. If parsing is needed, the relay does it. Anything the edge parses becomes state that dies at the hop boundary anyway, so spending cycles on it would be wasted work and Principle 2 violation surface.
- **Disk queue on the edge.** Network to the relay can blip; we do not want `wrap_app` blocking the journal cursor. The queue lets the output layer absorb the blip without pushing backpressure into the pipeline.

## Central host: relay

On the relay, the job is: accept RFC 5424 frames, drop noise, rewrite PRI so AMA's DCRs route the event to the right LAW table, and forward to AMA.

```limpid
// /etc/limpid/relay.limpid  (extract — other pipelines omitted)

def input ama_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
    queue {
        type disk
        path "/var/lib/limpid/queues/ama"
        max_size "1GB"
    }
}

def process app_drop_debug {
    // @requires: source       (set by syslog_tcp input from peer address)
    // @requires: ingress      (RFC 5424 frame from an edge host)
    // @produces: workspace.*  (parsed fields, if the event came from an edge host)
    //
    // Only parse events from the known edge-host IPs. We do not parse traffic
    // from other peers — other pipelines in this file handle them.

    if source == "10.0.0.21" or source == "10.0.0.22" {
        syslog.parse(ingress)
        try {
            parse_json(workspace.syslog_msg)
        } catch {
            // Not every line is JSON (startup banners, etc). Keep the event.
            workspace.parse_error = error
        }
        if workspace.level == "DEBUG" {
            drop
        }
    }
}

def process ama_rewrite {
    // @requires: ingress   (any syslog frame)
    // @produces: egress    (same bytes with PRI rewritten for DCR routing)
    //
    // AMA's DCRs route by facility:
    //   facility=16 (local0) -> CommonSecurityLog table (for CEF events)
    //   facility=17 (local1) -> Syslog table (everything else)
    // This process is the single place that makes that decision.

    if contains(ingress, "CEF:") {
        egress = syslog.set_pri(egress, 16, 6)
    } else {
        egress = syslog.set_pri(egress, 17, 6)
    }
}

def pipeline ama_forward {
    input ama_tcp
    process app_drop_debug | ama_rewrite
    output ama
}
```

Note what the relay does *not* do:

- It does not know or care that `edge01` and `edge02` exist by name. It knows two IPs.
- It does not have a separate pipeline per edge host. The pipeline is keyed on the wire contract, not on the sender identity.
- It does not reserialize the JSON payload. The edge host's MSG body passes through untouched to AMA, which in turn passes it to LAW as the `SyslogMessage` column. KQL at the SIEM side runs `extend j = parse_json(SyslogMessage)` to structure it at query time.

## Why RFC 5424 is the hop contract

It could have been anything — raw journald lines, JSON, CEF, a custom framing. RFC 5424 was chosen for three concrete reasons, and those reasons generalize to any multi-hop limpid topology:

1. **The receiver already speaks it.** `syslog_tcp` input plus `syslog.parse` and `syslog.set_pri` are primitives the daemon ships. Picking a wire format with built-in primitives costs less DSL than inventing one.
2. **PRI carries one bit of routing state cheaply.** `ama_rewrite` needs to set a facility so AMA's DCR routes correctly. Doing that with a PRI byte is a one-line operation; doing it with a JSON envelope would require a parse + rewrite + serialize cycle at every hop.
3. **The contract is greppable.** If something goes wrong, `tcpdump` on port 514 or `limpidctl tap input ama_tcp` shows the exact bytes the receiver sees. There is no "metadata layer" to miss.

This is Principle 3 (`Only egress crosses hop boundaries`) playing out. The edge host does not send the relay a sidecar map of journal fields, a "real" timestamp, or a source tag. It sends bytes. Everything the relay knows about the event, it reconstructs from those bytes with `syslog.parse`.

## Verifying the pipeline

With four tap points you can see exactly where an event is at every hop:

```bash
# On edge01 — verify journald events enter the pipeline
sudo limpidctl tap input app_journal

# On edge01 — verify the RFC 5424 frame before it hits the wire
sudo limpidctl tap output to_relay

# On the relay — verify bytes arrived correctly
sudo limpidctl tap input ama_tcp

# On the relay — verify PRI rewrite and parse results
sudo limpidctl tap output ama --json | jq '.egress, .workspace'
```

A common bug shape: `app_drop_debug` was supposed to drop DEBUG events but wasn't — the `level` field was nested inside the parsed JSON and the snippet referenced `workspace.level` instead of the correct path. The four-point tap finds this in under a minute: the event is present at `input ama_tcp`, still present at `output ama`, and `workspace.level` is undefined. No guessing, no restart, no log-digging. This is Principle 5 (safety and operational transparency) paying rent.

## End-to-end testing without real traffic

Because the edge pipeline is just a DSL program, you can drive it with `inject` and watch it with `tap` — no app.service traffic needed:

```bash
# On edge01 — simulate a journald line entering the pipeline
echo 'app[12345]: {"level":"INFO","msg":"user login","user":"alice"}' \
  | sudo limpidctl inject input app_journal

# Stream the result at the edge output
sudo limpidctl tap output to_relay
# <14>1 2026-04-20T06:15:23.104Z edge01 app - - - app[12345]: {"level":"INFO","msg":"user login","user":"alice"}
```

Replaying captured traffic into a staging relay is the same pattern: `tap --json` on one daemon, `inject --json` on the other. See [Debug Tap](../operations/tap.md#replay-with-tap--inject) for the full workflow, including `--replay-timing` for cadence-sensitive tests.

This matters because pipeline correctness is a two-axis problem: *does the config express the right intent*, and *does the expressed intent work against real traffic shapes*. The first axis is covered by `limpid --check` and code review. The second is covered by `inject` + `tap` on recorded traffic, in CI, against the same DSL that runs in production.

## One DSL across hosts

The edge hosts and the relay run the same binary with different configs. The configs are written in the same DSL — the same `def input / def process / def output / def pipeline` grammar, the same function library, the same `limpidctl` for observability. A contributor who has read one config can read the other.

Compare the equivalent assembly of tools a team would otherwise stitch together for this topology:

- rsyslog on edge hosts (imjournal + omfwd + a Ruby-like template language for the frame).
- rsyslog or mdsd's native syslog receiver on the relay.
- AMA's DCR-defined XML for the routing.
- Three completely different configuration dialects, three different debuggers, three different ways to ask "where did the event go?".

Replacing those with one DSL per hop is what the *Domain knowledge in DSL* operating rule buys at deployment time.

## AMA-specific notes

Two pieces of this are specific to Azure Monitor Agent and will look different for Splunk, Elasticsearch, or OpenSearch destinations:

- **The `28330/tcp` mdsd socket and the facility routing rule (16 → CommonSecurityLog, 17 → Syslog) are AMA's contract.** limpid just writes bytes to it. If you are targeting Splunk HEC, the output becomes `type http`, the `ama_rewrite` process goes away, and whatever shaping Splunk wants takes its place.
- **JSON payloads land in LAW as strings in the `SyslogMessage` column.** Structured access is a KQL-side `parse_json`. Getting structured columns on ingest requires a custom LAW table plus a DCR with a file-based data source, which is out of scope for a syslog relay and is not something limpid changes. This is a property of AMA / DCR, not of limpid.

The point of calling these out is the same as the point of the whole example: **routing policy lives in one small, readable process (`ama_rewrite`), not scattered across the daemon.** If Azure changes the facility convention tomorrow, the fix is a two-line edit in one file.

## Related

- [Design Principles](../design-principles.md) — the principles this example exercises, especially Principle 3 (hop boundaries), Principle 4 (atomic events), and Principle 5 (operational transparency).
- [Process Design Guide](../processing/design-guide.md) — how `wrap_app`, `app_drop_debug`, and `ama_rewrite` fit the granularity and contract conventions.
- [Debug Tap](../operations/tap.md) — the `tap` / `inject` workflow used throughout.
- [Pipeline Examples](./examples.md) — smaller, single-host pipeline examples.

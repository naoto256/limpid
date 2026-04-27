# Multi-host Pipeline Example

A small case study of a two-tier pipeline built entirely in limpid: two edge hosts (`edge01`, `edge02`, Ubuntu 24.04) ship journald logs as RFC 5424 syslog to a central `relay` host. The relay also receives CEF events from a FortiGate firewall (`fortigate01`) on the same listener. It then rewrites the PRI and relays everything to the Azure Monitor Agent (AMA), which routes plain syslog to LAW's `Syslog` table and CEF to `CommonSecurityLog`.

The example is deliberately small — one service on the edges, one CEF source, one forwarding pipeline in the middle, one SIEM destination. The point is not the size; it is showing **how the design principles work across a hop boundary in practice.**

## Topology

```
+---------------+   +---------------+   +-----------------+
|    edge01     |   |    edge02     |   |   fortigate01   |
|  10.0.0.21    |   |  10.0.0.22    |   |   10.0.0.30     |
|               |   |               |   |                 |
| journal input |   | journal input |   |  (CEF over      |
|      |        |   |      |        |   |   syslog/TCP    |
| wrap_app      |   | wrap_app      |   |   on 514)       |
|      |        |   |      |        |   |                 |
| tcp output    |   | tcp output    |   |                 |
|  (RFC 5424)   |   |  (RFC 5424)   |   |                 |
+-------|-------+   +-------|-------+   +--------|--------+
        |                   |                    |
        |    RFC 5424       |     CEF over       |
        |    over TCP:514   |     syslog/TCP:514 |
        +---------+---------+--------------------+
                  |
                  v
         +------------------+
         |     relay        |
         |   10.0.0.10      |
         |                  |
         |  tcp514 input     |
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

1. Only syslog-framed bytes cross the wire to the relay. RFC 5424 from the edges, CEF-over-syslog from FortiGate — both fit the `syslog_tcp` listener's framing rules. That is the hop contract.
2. The relay never sees journal fields, never sees CEF parsed structure on the wire, never sees anything the senders did not explicitly put into the bytes. Principle 3: only `egress` crosses hop boundaries.
3. Each hop that runs limpid runs the same binary with the same DSL. (FortiGate is a fixed appliance, not limpid, so it speaks CEF over its own syslog client — the relay's syslog_tcp input accepts that without special-casing.)

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

    egress = "<14>1 ${strftime(received_at, "%Y-%m-%dT%H:%M:%S%.3fZ", "utc")} ${hostname()} app - - - ${ingress}"
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

def input tcp514 {
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
    // @requires: source              (set by syslog_tcp input from peer)
    // @requires: workspace.syslog.*  (set by the inline syslog parser at the pipeline head)
    // @produces: drops the event     (when the parsed JSON body says DEBUG)
    //
    // Only inspect events from the known edge-host IPs. FortiGate
    // events flow past untouched.

    if source.ip == "10.0.0.21" or source.ip == "10.0.0.22" {
        try {
            workspace.json = parse_json(workspace.syslog.msg)
            if workspace.json.level == "DEBUG" {
                drop
            }
        } catch {
            // Not every line is JSON (startup banners, etc). Keep the event.
            workspace.parse_error = error
        }
    }
}

def process ama_rewrite {
    // @requires: workspace.syslog.msg  (set by the inline parser at the pipeline head)
    // @produces: egress                (same bytes with PRI rewritten for DCR routing)
    //
    // AMA's DCRs route by facility:
    //   facility=16 (local0) -> CommonSecurityLog table (for CEF events)
    //   facility=17 (local1) -> Syslog table (everything else)
    // This process is the single place that makes that decision.

    if starts_with(workspace.syslog.msg, "CEF:") {
        egress = syslog.set_pri(egress, 16, 6)
    } else {
        egress = syslog.set_pri(egress, 17, 6)
    }
}

def pipeline ama_forward {
    input tcp514

    // Parse once at the head so downstream processes can read
    // workspace.syslog.* without re-parsing.
    process { workspace.syslog = syslog.parse(ingress) }

    process app_drop_debug | ama_rewrite
    output ama
}
```

Note what the relay does *not* do:

- It does not know senders by name — only by IP. Its CEF / non-CEF distinction is made by inspecting `ingress`, not by routing on a sender attribute set elsewhere.
- It does not have a separate pipeline per sender. One pipeline handles edge-host journald-RFC5424 traffic and FortiGate CEF traffic — the body's branches are keyed on the wire contract (`contains(ingress, "CEF:")`, source IP), not on a per-sender topology.
- It does not reserialize the journald JSON payload. The edge host's MSG body passes through untouched to AMA, which in turn passes it to LAW as the `SyslogMessage` column. KQL at the SIEM side runs `extend j = parse_json(SyslogMessage)` to structure it at query time. CEF events take the parallel path — `ama_rewrite` sets PRI to local0.info so AMA writes them to the `CommonSecurityLog` table, where SIEM rules consume the parsed CEF directly.

## Verifying the pipeline

With four tap points you can see exactly where an event is at every hop:

```bash
# On edge01 — verify journald events enter the pipeline
sudo limpidctl tap input app_journal

# On edge01 — verify the RFC 5424 frame before it hits the wire
sudo limpidctl tap output to_relay

# On the relay — verify bytes arrived correctly
sudo limpidctl tap input tcp514

# On the relay — verify PRI rewrite and parse results
sudo limpidctl tap output ama --json | jq '.egress, .workspace'
```

A common bug shape: `app_drop_debug` was supposed to drop DEBUG events but wasn't — the `level` field was nested inside the parsed JSON and the snippet referenced `workspace.level` instead of the correct path. The four-point tap finds this in under a minute: the event is present at `input tcp514`, still present at `output ama`, and `workspace.level` is undefined. No guessing, no restart, no log-digging. This is Principle 5 (safety and operational transparency) paying rent.

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

- rsyslog on edge hosts (imjournal input + omfwd output + RainerScript templates for the frame).
- rsyslog or mdsd's native syslog receiver on the relay.
- AMA's DCR for the table routing (XML / JSON depending on the deployment shape).

Three configuration languages, each with its own templating, conditionals, and observability tooling. None are bad in isolation; the operational cost is paid every time someone has to edit two of them in one change. Collapsing the topology onto one DSL per hop is what the *Domain knowledge in DSL* operating rule buys at deployment time.

## Related

- [Design Principles](../design-principles.md) — the principles this example exercises, especially Principle 3 (hop boundaries), Principle 4 (atomic events), and Principle 5 (operational transparency).
- [Process Design Guide](../processing/design-guide.md) — how `wrap_app`, `app_drop_debug`, and `ama_rewrite` fit the granularity and contract conventions.
- [Debug Tap](../operations/tap.md) — the `tap` / `inject` workflow used throughout.
- [Pipeline Examples](./examples.md) — smaller, single-host pipeline examples.

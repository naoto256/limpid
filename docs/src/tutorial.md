# Tutorial

A walk through building up a pipeline one operational requirement at a time, the same way you would in production. Each step adds a few lines, runs a CLI command, and shows what changes.

The scenario: you operate a few firewalls that send syslog to a central host. You want to forward to Azure Monitor Agent (AMA), keep an archive on disk, drop the noise, and be able to look inside the daemon at any time.

Prerequisite: limpid is installed (see [Installation](./getting-started.md)). All paths below assume the conventional layout under `/etc/limpid/`.

## Step 1 — Pass-through

The minimum useful pipeline: receive syslog over TCP and write it to a file. No parsing, no rewriting. Everything in one file:

```limpid
// /etc/limpid/limpid.conf
def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/archive.log"
}

def pipeline main {
    input fw_tcp
    output archive
}
```

The pipeline is `input → output`; what arrives on the wire is appended verbatim to `archive.log`. No transformation happens because none was asked for — Principle 2 (I/O is dumb transport).

## Step 2 — Forward to AMA as well

The on-disk archive is in place. Now you also want to forward the same stream to Azure Monitor Agent (AMA), which listens on a local TCP port. Add a second output and a second `output` line — that's it.

```limpid
// /etc/limpid/limpid.conf
def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/archive.log"
}

def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
}

def pipeline main {
    input fw_tcp
    output archive
    output ama
}
```

`output` is **non-terminal**: an event passes through `output archive` and continues to `output ama`. There is one more piece of magic in this — but it only becomes visible in Step 3, so we'll come back to it then.

After editing the config, reload the daemon to pick up the change — either `sudo systemctl reload limpid` or `sudo kill -HUP $(pidof limpid)`. The same applies after every step from here on; we won't repeat the instruction. If the new config fails to parse or start, limpid keeps the previous configuration running and reports the diagnostic — there is no half-loaded state to recover from.

## Step 3 — Parse, then drop the noise

FortiGate `traffic` events are too noisy to forward to AMA, but you still want them in the on-disk archive for after-the-fact investigation. So: archive everything, then drop FortiGate traffic, then forward what's left.

To recognise a FortiGate traffic event you need to parse the FortiGate event into a shape you can branch on. The `include` directive can pull in DSL parts from the shipped snippet library at `/usr/share/limpid/snippets/`. Suppose a FortiGate parser is provided as `parsers/fortigate.limpid` and exposes a `parse_fortigate` process that fills `workspace.limpid.*` (limpid's canonical intermediate — OCSF-shaped, see [Process Design Guide](./processing/design-guide.md#use-workspacelimpid-as-the-canonical-intermediate)) from a FortiGate event. The activity kind lands at `workspace.limpid.activity_name`. (The set of shipped snippets will grow over time — see [Snippet Library](./snippets/README.md) for what is available today.)

```limpid
// /etc/limpid/limpid.conf  — add at the top
include "/usr/share/limpid/snippets/parsers/fortigate.limpid"

def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/archive.log"
}

def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
}

def pipeline main {
    input fw_tcp
    output archive                                          // everything goes to disk
    process parse_fortigate                                 // populate workspace.limpid.*
    if workspace.limpid.activity_name == "traffic" { drop } // drop the noise
    output ama                                              // only the survivors reach AMA
}
```

Two things to read carefully here.

The first is the magic from Step 2. Even though the `process` and `if drop` after `output archive` mutate or remove the event, the bytes already handed to `output archive` are unaffected. Each output's view of the event is **deep-copied** at the point of branching, so downstream processing cannot reach back and change what an earlier output saw. A single pipeline can fan an event out to as many destinations as needed, in the order you want them written, without a `copy` or `tee` construct, and without the post-branch processing being able to corrupt earlier branches.

The second is what the pipeline body actually contains: an `input`, two `output`s, a `process`, and an `if/drop`. There is no separate "filter" or "router" abstraction — routing decisions live in the pipeline as ordinary statements, in the order they execute. That mirrors how you'd describe the pipeline in words: "archive everything, parse it as FortiGate, drop traffic events, forward the rest to AMA."

Worth one more note: `parse_fortigate` populates `workspace.limpid.activity_name`, which the `if` then reads. The daemon itself knows nothing about FortiGate — that knowledge lives entirely in the snippet, which maps the FortiGate event into limpid's canonical intermediate.

## Step 4 — Confirm the drop is actually happening

The pipeline is wired. Before trusting it in production, let's confirm it does what we expect: `archive` receives everything, `ama` receives everything *except* FortiGate traffic events.

To talk to the running daemon you need `limpidctl`, which connects over a Unix socket. Declare the socket path in `limpid.conf` (anywhere at the top level — order doesn't matter):

```limpid
// /etc/limpid/limpid.conf  — add at the top
control {
    socket "/var/run/limpid/control.sock"
}
```

Reload the daemon (`systemctl reload limpid`) and the socket appears.

Now start with the counters:

```bash
$ sudo limpidctl stats
Pipelines:
  main                         15234 received     14102 finished      1132 dropped         0 discarded
Inputs:
  fw_tcp                       15234 received         0 invalid         0 injected
Outputs:
  archive                      15234 received     15234 written         0 failed
  ama                          14102 received     14102 written         0 failed
```

15,234 events came in. `archive` got all of them. `ama` got 14,102 — short by exactly the 1,132 events the pipeline dropped. The shape matches: the drop is firing, and nothing is silently leaking to AMA.

Counters tell you *how many*; they don't tell you *which*. To check that the events being dropped really are the FortiGate traffic ones (and not, say, something legitimate that you accidentally matched), attach a `tap` to the AMA output and look at what's flowing through:

```bash
$ sudo limpidctl tap output ama
<134>CEF:0|Fortinet|FortiGate|7.0|attack|src=203.0.113.5 ...
<134>CEF:0|Fortinet|FortiGate|7.0|utm|src=203.0.113.18 ...
  ⋮

$ sudo limpidctl tap output archive
<134>CEF:0|Fortinet|FortiGate|7.0|attack|src=203.0.113.5 ...
<134>CEF:0|Fortinet|FortiGate|7.0|traffic|src=203.0.113.7 ...
<134>CEF:0|Fortinet|FortiGate|7.0|utm|src=203.0.113.18 ...
<134>CEF:0|Fortinet|FortiGate|7.0|traffic|src=203.0.113.9 ...
  ⋮
```

`ama` shows only non-`traffic` events; `archive` still shows the `traffic` ones interleaved. The drop is firing exactly between the two outputs, which is what the pipeline asked for.

`tap` attaches to a named hop in a running pipeline; nothing is paused, no traffic is duplicated to other consumers, and the overhead while no one is listening is negligible. So you can leave the daemon alone and just ask it questions whenever you want.

## Step 5 — Split the config across files

The config is growing. The conventional layout: one file per module under `inputs/`, `outputs/`, and `pipelines/`, with `limpid.conf` reduced to includes and global blocks.

```limpid
// /etc/limpid/limpid.conf
include "/usr/share/limpid/snippets/parsers/fortigate.limpid"

include "inputs/*.limpid"
include "outputs/*.limpid"
include "pipelines/*.limpid"

control {
    socket "/var/run/limpid/control.sock"
}
```

```limpid
// /etc/limpid/inputs/fw_tcp.limpid
def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}
```

```limpid
// /etc/limpid/outputs/archive.limpid
def output archive {
    type file
    path "/var/log/limpid/archive.log"
}
```

```limpid
// /etc/limpid/outputs/ama.limpid
def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
}
```

```limpid
// /etc/limpid/pipelines/main.limpid
def pipeline main {
    input fw_tcp
    output archive
    process parse_fortigate
    if workspace.limpid.activity_name == "traffic" { drop }
    output ama
}
```

The pipeline body itself is unchanged from Step 3 — only the surrounding files moved.

(There is no `processes/` directory yet — we don't have any user-defined processes to put there. That changes in Step 6.)

Glob includes resolve relative to the main config file's directory. The directory layout is a convention; you could put everything in one file (and we did, until now). See [Main Configuration](./configuration.md) for the global blocks and include rules.

## Step 6 — Send a summary of traffic to AMA instead of dropping it

The requirement changes: AMA *does* want visibility into FortiGate traffic events, but not the firehose. A summary is enough — one event per `(src, dst)` flow per five minutes is plenty to reconstruct what was talking to what.

Dropping is no longer the right operation. Instead, dedup: forward the first event for each `(src, dst)` pair, suppress the rest until the entry expires.

`parse_fortigate` already populated `workspace.limpid.src_endpoint.ip` and `workspace.limpid.dst_endpoint.ip` (the snippet maps the FortiGate CEF event into limpid's canonical intermediate, which is OCSF-shaped — see [Process Design Guide → canonical intermediate](./processing/design-guide.md#use-workspacelimpid-as-the-canonical-intermediate)). We need somewhere to remember which pairs we have seen recently — that's what limpid's in-memory tables are for. Declare a table in `limpid.conf` (the `table` block must live in the main config), then write the process that consults and updates it.

```limpid
// /etc/limpid/limpid.conf  — add the table block
table {
    traffic_seen {
        max 100000
        ttl 300
    }
}
```

```limpid
// /etc/limpid/processes/dedup_fortigate_traffic.limpid
def process dedup_fortigate_traffic {
    if workspace.limpid.activity_name == "traffic" {
        let key = workspace.limpid.src_endpoint.ip + "|" + workspace.limpid.dst_endpoint.ip
        if table_lookup("traffic_seen", key) != null {
            drop
        }
        table_upsert("traffic_seen", key, "1")
    }
}
```

```limpid
// /etc/limpid/pipelines/main.limpid
def pipeline main {
    input fw_tcp
    output archive
    process parse_fortigate | dedup_fortigate_trafic
    output ama
}
```

A few things to read:

- `process A | B` chains processes left to right — the event flows through `parse_fortigate` first, then `dedup_fortigate_traffic`. Equivalent to writing them on two `process` lines; the `|` form just reads more naturally when several processes line up.
- `workspace.limpid.src_endpoint.ip` and `workspace.limpid.dst_endpoint.ip` exist because `parse_fortigate` ran first. Pipeline order is real order — what comes earlier has populated the canonical intermediate by the time later steps run.
- `let key = ...` is a process-local scratch variable — scalar only, scoped to this process invocation, gone when the event leaves.
- `table_upsert` resets the TTL on every call, so a flow that keeps appearing keeps being suppressed; the dedup window only opens up once a flow has been quiet for five minutes (`ttl 300` on the table).
- The table is in-memory and lost on daemon restart. That's usually fine for dedup — at worst, the first batch after restart is forwarded normally instead of being suppressed. See [table functions](./functions/expression-functions.md#table-functions) for the full surface (`table_lookup` / `table_upsert` / `table_delete`).
- `archive` still sees every event — it sits before the dedup process, and Step 3's deep-copy guarantee keeps it that way.

This is a bigger change than anything we have done so far — a new global block, a new process, a new pipeline reference. Hot-reloading and hoping for the best is not how you want to find out about a typo. Run `--check` first.

## Step 7 — Validate before deploying

`limpid --check` parses the whole configuration, resolves every reference between modules and pipelines, and reports the first thing that doesn't add up — without binding any sockets or touching the running daemon.

```bash
$ limpid --check --config /etc/limpid/limpid.conf
error: unknown process 'dedup_fortigate_trafic' referenced in pipeline 'main'
  --> /etc/limpid/pipelines/main.limpid:5:30
   |
 5 |     process parse_fortigate | dedup_fortigate_trafic
   |                              ^^^^^^^^^^^^^^^^^^^^^^^ no such process is defined
   |
   = help: did you mean `dedup_fortigate_traffic`?
```

Caught it. The pipeline reference is missing an `f` in `traffic`. Fix the typo in `pipelines/main.limpid`, run `--check` again:

```bash
$ limpid --check --config /etc/limpid/limpid.conf
Configuration OK
  1 input(s), 2 output(s), 2 process(es), 1 pipeline(s)
```

Now reload — `sudo systemctl reload limpid`. The new config takes effect atomically; if it had failed to parse or start at this point, limpid would have rolled back to the previous configuration automatically. There is no half-loaded state to recover from.

Run `--check` in CI on every config change. Type errors, unknown identifiers, unreachable pipelines, and similar mistakes are all surfaced here with line and column before they ever reach the daemon.

## Step 8 — Replay yesterday's traffic against today's config

You added the dedup in Step 6. You want to confirm what it actually does to a real day of traffic, without bothering the production source.

Capture once, replay repeatedly:

```bash
# On production: capture what arrives at the input, full Event JSON
$ sudo limpidctl tap input fw_tcp --json > /tmp/fw_tcp_capture.jsonl

# On staging (or the same host with a different config): replay
$ sudo limpidctl inject input fw_tcp --json < /tmp/fw_tcp_capture.jsonl
$ sudo limpidctl stats
Pipelines:
  main                        278941 received     10117 finished    268824 dropped         0 discarded
```

`tap --json` and `inject --json` are symmetric — every Event captured by one is consumable by the other, with `received_at` and `source` preserved. Add `--replay-timing` to play back at the original cadence, or `--replay-timing=10x` to compress an hour of traffic into six minutes. This makes "what would this config change have done last Tuesday?" a routine question.

## Where to next

You now have a pipeline that:

- receives syslog over TCP,
- archives every event verbatim to disk,
- parses FortiGate CEF events using a shipped snippet,
- forwards to AMA — every non-`traffic` event in full, plus one `traffic` event per `(src_endpoint.ip, dst_endpoint.ip)` flow per five minutes as a summary.

And along the way you've picked up the operator-side tools that come with limpid:

- `limpidctl tap` and `limpidctl stats` for seeing what is flowing where, live, with no redeploy,
- `limpid --check` for catching configuration mistakes before they reach the daemon, with automatic rollback if a reload fails anyway,
- `limpidctl tap --json` + `limpidctl inject --json --replay-timing` for capturing real traffic and replaying it through any configuration after the fact.

From here:

- [Inputs](./inputs/README.md) and [Outputs](./outputs/README.md) — every wire-speaking module the daemon ships
- [Snippet Library](./snippets/README.md) — what parsers and composers are available today
- [Process Design Guide](./processing/design-guide.md) — patterns for writing your own processes
- [Pipelines](./pipelines/README.md) — `if`/`switch` routing, multi-output, `drop` vs `finish`
- [Operations → CLI](./operations/cli.md) — the full surface of `limpidctl`

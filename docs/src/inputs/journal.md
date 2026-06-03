# journal

Reads entries from the systemd journal. Linux only.

## Build requirement

```bash
sudo apt install libsystemd-dev
cargo build --release -p limpid --features journal
```

## Configuration

```
def input system {
    type journal
    match "SYSLOG_FACILITY=10"
    state_file "/var/lib/limpid/journal/cursor"
    poll_interval "1s"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `match` | no | none | Journal match filter (e.g., `SYSLOG_FACILITY=10`) |
| `state_file` | no | none | Path to persist journal cursor (survives restarts) |
| `poll_interval` | no | `1s` | How often to poll for new entries |

## Wire format

`ingress` is **byte-identical to one line of `journalctl -o json`** — limpid
invents no format here (LOTL: Living Off The Land). One journal entry → one
`Event`, with `ingress` carrying a single-line UTF-8 JSON object.

```
{"__CURSOR":"s=abc...","__REALTIME_TIMESTAMP":"1714400000000000",
 "PRIORITY":"6","SYSLOG_FACILITY":"4","SYSLOG_IDENTIFIER":"sshd",
 "_PID":"12345","_HOSTNAME":"edge01","_SYSTEMD_UNIT":"ssh.service",
 "MESSAGE":"Accepted publickey for alice from 192.0.2.10 port 51234 ssh2"}
```

Conventions matching `journalctl`:

- field names preserved as journald exposes them (`PRIORITY`, `_PID`,
  `__REALTIME_TIMESTAMP`, `SYSLOG_IDENTIFIER`, `MESSAGE`, …)
- field order: insertion order from libsystemd
- UTF-8-clean values: JSON strings
- non-UTF-8 byte values (rare; e.g. `COREDUMP`): JSON array of integers
  `[104, 101, 108, 108, 111]`
- numeric-looking fields like `PRIORITY` remain JSON **strings** (`"6"`);
  convert with `to_int(...)` if you need arithmetic
- absent fields: omitted (no nulls)

`workspace` stays empty on input — all parsing happens in the process layer.

## Use in a pipeline

The standard pattern is two snippets — one to pull the structured fields off
the JSON, one to compose the wire form you want to ship:

```
include "/usr/share/limpid/snippets/parsers/parse_journald.limpid"
include "/usr/share/limpid/snippets/composers/compose_rfc5424.limpid"

def input ssh_journal {
    type journal
    match "_SYSTEMD_UNIT=ssh.service"
    state_file "/var/lib/limpid/journal/ssh.cursor"
}

def output relay {
    type syslog_tcp
    peer { host "relay.example" port 514 }
}

def pipeline ssh_to_relay {
    input ssh_journal
    process parse_journald | compose_rfc5424
    output relay
}
```

`parse_journald` populates `workspace.journald.*` with everything in the
ingress JSON. `compose_rfc5424` reads those fields and writes a single-line
RFC 5424 record to `egress`. Swap `compose_rfc5424` for `parse_openssh |
compose_ocsf` to ship OCSF Authentication events instead.

## Notes

- On first start without a `state_file`, reading begins at the end of the journal.
- The cursor is saved atomically (write-to-temp + rename).
- The source address for journal events is `127.0.0.1:0` (placeholder; the
  meaningful host metadata lives inside the JSON as `_HOSTNAME`).

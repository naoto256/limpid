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
| `match` | no | none | Journal match filter (`FIELD=value`, e.g. `SYSLOG_FACILITY=10`). Repeatable — see [match combining rules](#match-combining-rules) for AND / OR semantics. |
| `state_file` | no | none | Path to persist journal cursor (survives restarts) |
| `poll_interval` | no | `1s` | How often to poll for new entries |

### `match` combining rules

`match` is repeatable. libsystemd's `sd_journal_add_match` combines
consecutive filters by the field name:

- **Same field name → OR.** `match "SYSLOG_IDENTIFIER=app1" match "SYSLOG_IDENTIFIER=app2"` matches entries whose `SYSLOG_IDENTIFIER` is either `app1` or `app2`.
- **Different field names → AND.** `match "SYSLOG_IDENTIFIER=app" match "_UID=1000"` matches only entries whose `SYSLOG_IDENTIFIER` is `app` **and** whose `_UID` is `1000`.

Field names are the ones journald uses internally (`SYSLOG_IDENTIFIER`, `_UID`, `_SYSTEMD_UNIT`, `PRIORITY`, `MESSAGE`, …) — see `systemd.journal-fields(7)`. `journalctl -o json` shows the full field set for any entry you want to filter on.

Format is validated at daemon startup / `--check` time — every match string must contain `=`, and each token before / after the separator is treated as an opaque byte sequence handed to libsystemd. libsystemd rejects field names it doesn't accept (lowercase, empty, non-printable, NUL-containing, etc.) at the runtime `sd_journal_add_match` boundary; the journal input **logs the rejected filter and terminates the reader** rather than continuing without the filter (a filter that libsystemd cannot install matches nothing, so zero events is the semantically correct output — fix the filter and restart the daemon).

## Wire format

`ingress` is shaped to be **journalctl-`-o json`-compatible for the
fields libsystemd exposes** — limpid invents no format here (LOTL: Living
Off The Land). One journal entry → one `Event`, with `ingress` carrying a
single-line UTF-8 JSON object. The shape matches `journalctl -o json` on
field set and values, with two known divergences: `__SEQNUM` /
`__SEQNUM_ID` (newer `journalctl`) are **not surfaced** because the
in-crate `journal_sys` FFI doesn't bind `sd_journal_get_seqnum`, and
JSON object key order is libsystemd's insertion order — not guaranteed
to byte-match `journalctl -o json`.

```
{"__CURSOR":"s=abc...","__REALTIME_TIMESTAMP":"1714400000000000",
 "PRIORITY":"6","SYSLOG_FACILITY":"4","SYSLOG_IDENTIFIER":"sshd",
 "_PID":"12345","_HOSTNAME":"edge01","_SYSTEMD_UNIT":"ssh.service",
 "MESSAGE":"Accepted publickey for alice from 192.0.2.10 port 51234 ssh2"}
```

Conventions matching `journalctl`:

- field names preserved as journald exposes them (`PRIORITY`, `_PID`,
  `__REALTIME_TIMESTAMP`, `SYSLOG_IDENTIFIER`, `MESSAGE`, …)
- field order: insertion order from libsystemd is preserved through the read path. JSON object key order on the wire is a serialisation detail and is not guaranteed to byte-match `journalctl -o json`.
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
    process parse_journald | compose_rfc5424 | rfc5424_to_egress
    output relay
}
```

`parse_journald` populates `workspace.journald.*` with everything in the
ingress JSON. `compose_rfc5424` reads those fields and writes a single-line
RFC 5424 record to `workspace.lsis.rfc5424`; the companion `rfc5424_to_egress`
step at the end of the pipeline hands that slot to `egress`. Swap
`compose_rfc5424 | rfc5424_to_egress` for `parse_openssh | compose_ocsf |
ocsf_to_egress` to ship OCSF Authentication events instead.

## Notes

- On first start without a `state_file`, reading begins at the end of the journal.
- The cursor is saved atomically (write-to-temp + rename).
- The source address for journal events is `127.0.0.1:0` (placeholder; the
  meaningful host metadata lives inside the JSON as `_HOSTNAME`).
- Since 0.7.8, the persisted cursor advances only after the pipeline worker finishes processing the corresponding entry. In-flight entries at the moment of a crash are re-read on the next start — the at-least-once recovery contract documented under [Recovery readiness](../operations/error-log.md#recovery-readiness-check---check).

# windows_event_log

Subscribes to a Windows Event Log channel through the native Windows Event Log API. Each event enters the pipeline as UTF-8 JSON in `ingress`.

This input is available on Windows only.

## Configuration

```limpid
def input system_events {
    type windows_event_log
    channel "System"
    query "*[System[(Level=1 or Level=2 or Level=3)]]"
    state_file "C:/ProgramData/limpid/state/system.bookmark"
    poll_interval "1s"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `channel` | yes | — | Event Log channel name, for example `System` or `Microsoft-Windows-Sysmon/Operational` |
| `query` | no | `"*"` | XPath query passed to the Windows Event Log subscription |
| `state_file` | no | unset | File that stores the native bookmark after pipeline completion |
| `poll_interval` | no | `"1s"` | Idle wait interval; the native reader clamps each wait to 1–100 ms so shutdown stays responsive |

The service package adds `NT SERVICE\limpid` to the built-in Event Log Readers group. Some security and provider-specific channels require additional permissions. A channel or query that cannot be opened makes input startup fail instead of silently skipping the source.

## Event shape

The JSON object contains:

- `Xml`: the complete rendered event XML, including namespaces and `UserData` that are not projected elsewhere.
- Each child of `System` as a top-level property. Elements with attributes become objects; their text, when present, is stored as `Value`.
- `EventData`: an ordered array of `{ "Name": ..., "Value": ... }` objects. An array is used so duplicate field names are preserved.

For example:

```json
{
  "Xml": "<Event ...>...</Event>",
  "Provider": {"Name": "Microsoft-Windows-Kernel-General"},
  "EventID": "12",
  "Channel": "System",
  "EventData": [
    {"Name": "value", "Value": "first"},
    {"Name": "value", "Value": "second"}
  ]
}
```

The projection preserves provider names and values. It does not normalize events into a vendor-neutral schema. Use the shipped `parse_winevent_json` or `parse_sysmon` snippets when their input contract matches the selected channel and downstream schema.

## Bookmark and delivery behavior

When `state_file` is absent, missing, empty, or unreadable, the subscription starts with events produced after startup. It does not replay the channel from its oldest record.

When a saved bookmark is valid, the subscription resumes after that event. The bookmark advances only after the event completes the pipeline acknowledgement boundary. This gives the input journal-style at-least-once restart behavior: an event may be read again after interruption, but a bookmark is not advanced merely because the native API returned it.

An invalid or stale saved bookmark produces a warning and starts with new events while leaving the old state file in place. If channel clearing or rotation invalidates a live result set, limpid warns and resubscribes from new events. A state-file write failure also warns and keeps the input running; acknowledged events can then be read again after restart.

The bookmark replacement is atomic at the file-name level, but it is not an `fsync` durability guarantee.

# Add device context from the sender address

Attach a site, environment, and device name before sending a log downstream. Keep inventory facts separate from the device's own message: a hostname written inside a log line should not silently override your inventory.

This small example uses two explicit mappings. It does not need a mutable table or a lookup service. Larger or frequently changing inventories need their own loading, refresh, and missing-entry policy; this recipe does not invent one.

## Map known senders and label unknown ones

Create a private output directory in place of `/tmp/limpid-assets`. Replace the documentation-only addresses with the source addresses actually visible to the receiver.

```limpid
control {
    socket "/tmp/limpid-assets/control.sock"
    error_log "/tmp/limpid-assets/error.jsonl"
}

def input devices {
    type syslog_tcp
    bind "127.0.0.1:15514"
}

def output enriched {
    type file
    path "/tmp/limpid-assets/enriched.jsonl"
}

def process device_context {
    workspace.asset = { known: false }
    switch source.ip {
        "192.0.2.10" {
            workspace.asset = {
                known: true,
                name: "edge-fw-a",
                site: "tokyo",
                environment: "production"
            }
        }
        "192.0.2.11" {
            workspace.asset = {
                known: true,
                name: "lab-fw-b",
                site: "osaka",
                environment: "test"
            }
        }
    }
    egress = to_json({
        message: ingress,
        sender_ip: source.ip,
        asset: workspace.asset
    })
}

def pipeline enrich_devices {
    input devices
    process device_context
    output enriched
}
```

Unknown senders remain visible with `asset.known = false`; they are not silently assigned a production device or dropped. The default is set on every event, so a missing match cannot reuse a preceding device's context.

`sender_ip` is transport context. A firewall event's parsed `source.ip` usually describes an endpoint inside the event; those are different concepts. This recipe leaves that inner message untouched. If you add a firewall parser, retain distinct names for the transport sender and the source/destination endpoints.

## Inject controlled sender metadata

Save the following full Event envelopes as `events.jsonl`:

```json
{"ingress":"<134>test: claimed-host=lab-fw-b","source":{"ip":"192.0.2.10","port":514},"received_at":1789285800000000000}
{"ingress":"<134>test: claimed-host=edge-fw-a","source":{"ip":"192.0.2.11","port":514},"received_at":1789285800000000000}
{"ingress":"<134>test: claimed-host=edge-fw-a","source":{"ip":"192.0.2.99","port":514},"received_at":1789285800000000000}
```

`received_at` is required by the live Event replay schema and is an integer count of Unix nanoseconds. The fixed fixture value is sufficient for this local mapping test. Pipeline test mode accepts a smaller input shape, so passing a pipeline-only fixture to live injection is not equivalent. Always inspect the actual outputs: invalid Event lines may be skipped even when the inject command exits successfully.

Save the configuration as `assets.conf`, reserve the listener port, and start the test daemon. From another terminal, use **`--json`** to preserve each envelope's injected source metadata:

```sh
limpid --check --config assets.conf
limpid --config assets.conf
```

```sh
limpidctl --socket /tmp/limpid-assets/control.sock inject input devices --json < events.jsonl
```

The deliberately contradictory `claimed-host` text should have no influence on the mapping:

| Injected sender | Known | Device      | Site    | Environment  |
| --------------- | ----- | ----------- | ------- | ------------ |
| `192.0.2.10`    | true  | `edge-fw-a` | `tokyo` | `production` |
| `192.0.2.11`    | true  | `lab-fw-b`  | `osaka` | `test`       |
| `192.0.2.99`    | false | absent      | absent  | absent       |

After normal shutdown, inspect the output JSON and verify that each `message` still matches its original ingress. The unknown record must not contain the preceding device's name or environment. This tests routing data deliberately supplied through the control interface; it does not test network identity authentication.

## Know what the address proves

An IP-to-device map is enrichment, not authentication. NAT and relays can make many devices share one visible source. UDP sources can be spoofed, and access to the inject interface permits controlled source metadata. Restrict the listener and control interface to the intended senders/operators, and choose a suitable authenticated transport where identity matters.

Before replacing the file output with an external destination, review whether device names, sites, and the original message are appropriate to disclose. This recipe adds context; it does not redact private fields. In the Recipes list, see **Keep private fields out of forwarded logs** when the outbound document must omit them.

## Validation scope

All three Event envelopes were injected into a real limpid 0.9.0 daemon on macOS. The output preserved each original message and sender, matched both known inventories exactly, and left the unknown asset as `{ "known": false }`. The error log stayed empty and the process exited normally with code 0. Contradictory hostnames in the message did not affect the mappings. This does not certify sender authenticity, NAT behavior, or a production inventory source.

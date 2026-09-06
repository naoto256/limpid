# Drop unwanted logs, or keep the first occurrence

Some messages are never useful downstream. Others matter once, but repeated copies hide everything around them. Use `drop` for the first case and a short-lived table for the second.

## Discard every matching message

This configuration discards CHARGEN messages from one firewall and forwards everything else unchanged. The sender address is a documentation-only placeholder; the substring match is case-sensitive.

```limpid
def input firewall {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output downstream {
    type syslog_tcp
    peer { host "192.0.2.20" port 514 }
}

def process discard_chargen {
    if source.ip == "192.0.2.10" and contains(ingress, "CHARGEN") {
        drop
    }
}

def pipeline forward {
    input firewall
    process discard_chargen
    output downstream
}
```

## Keep the first attack log, then suppress repeats

Use this configuration instead when you want evidence of each attack source without forwarding every repeat. For a message such as `CHARGEN Attack log <198.51.100.7/12345>`, the key is `198.51.100.7`: the attack source extracted from the message, not the firewall's sender address.

```limpid
table {
    chargen_seen { max 10000; ttl 300 }
}

def input firewall {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output downstream {
    type syslog_tcp
    peer { host "192.0.2.20" port 514 }
}

def process thin_chargen {
    if source.ip == "192.0.2.10" and contains(ingress, "CHARGEN") {
        let src = regex_extract(ingress, "Attack log <([^/]+)/")
        if src != null and src != "" {
            if table_lookup("chargen_seen", src) != null {
                drop
            }
            table_upsert("chargen_seen", src, "1", 300)
        }
    }
}

def pipeline forward {
    input firewall
    process thin_chargen
    output downstream
}
```

The first matching message records its key and continues to the output. Repeats for that key are discarded for 300 seconds. A discarded repeat does not refresh the expiry; after expiry, the next message can pass and start a new window. Different attack sources have separate entries. Messages from other senders, non-CHARGEN messages, and messages without an extractable attack source pass unchanged.

| Incoming sequence within 300 seconds       | Result  |
| ------------------------------------------ | ------- |
| First CHARGEN message for attack source A  | Keep    |
| Another CHARGEN message for A              | Discard |
| First CHARGEN message for B                | Keep    |
| CHARGEN message with no extractable source | Keep    |
| Non-CHARGEN message                        | Keep    |

## Choose the state boundary

Tables are in memory. A restart clears their state; at `max 10000`, inserting a new key evicts the oldest entry. Either event can allow an attack source through earlier. This is repeated-message suppression, not random sampling, durable deduplication, or a distributed rate limit.

If two destinations need independent suppression, give their processes separate tables—for example, `chargen_seen_primary` and `chargen_seen_secondary`—and use the corresponding table name in both lookup and upsert. Sharing a table shares the suppression state. Keep the filtering before any transformation that removes the fields you match.

Adapt the extraction pattern to the actual message format before use.

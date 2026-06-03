# syslog_tcp

Sends events to one or more remote syslog TCP endpoints. Uses RFC 6587
framing (octet counting or non-transparent). Maintains a per-peer
persistent connection and rotates events across peers in round-robin
order; a peer that errors enters a short cooldown and is skipped until
it recovers.

## Configuration

Single destination:

```
def output relay {
    type syslog_tcp
    framing octet_counting
    peer { host "10.0.0.1" port 514 }
}
```

Multiple destinations (round-robin):

```
def output relay {
    type syslog_tcp
    framing octet_counting
    peers {
        peer { host "10.0.0.1" port 514 }
        peer { host "10.0.0.2" port 514 }
        peer { host "10.0.0.3" port 514 }
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `framing` | no | `octet_counting` | `octet_counting` or `non_transparent` (RFC 6587) |
| `peer` | one of | - | Single-destination block (see [peer block](#peer-block)). Mutually exclusive with `peers`. |
| `peers` | one of | - | Multi-destination block containing repeated `peer` entries. Mutually exclusive with `peer`. |

Exactly one of `peer` or `peers` must be specified.

### peer block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `host` | yes | - | Hostname or IP of the syslog receiver |
| `port` | no | `514` | TCP port |

## Framing

- **`octet_counting`** (default) - `MSG-LEN SP SYSLOG-MSG` per RFC 6587 §3.4.1
- **`non_transparent`** - messages terminated by LF per RFC 6587 §3.4.2

## Multi-destination semantics

When `peers` is used, every event is sent to exactly one peer chosen
in round-robin order. Failover and load-balancing happen at the per-event
level: each `write` advances the rotation cursor by one peer.

A peer enters a **5-second cooldown** when a TCP connect, write, or
flush returns an error. Subsequent rotations skip cooled-down peers
until the cooldown expires; on the next attempt after expiry, the peer
is reconnected and the cooldown is cleared on success or extended on
failure.

If every peer is cooled-down (or fails within the same write call), the
write returns an error and the queued retry path handles the rest. See
the [queue](../outputs/README.md#queue-and-retry) section for retry
behaviour.

## Notes

- The TCP connection to each peer is established lazily on first use
  and reused for subsequent events to that peer.
- A broken connection on a peer is dropped on write error and
  re-established on the next rotation visit (after cooldown).
- For TLS-encrypted syslog, see [syslog_tls](./syslog-tls.md).
- For UDP, see [syslog_udp](./syslog-udp.md).

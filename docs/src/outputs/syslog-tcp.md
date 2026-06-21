# syslog_tcp

Sends events to one or more remote syslog endpoints over TCP, with
optional **per-peer TLS**. A single output may hold a mixed peer list:
TLS-encrypted destinations coexist with plaintext destinations on the
same rotation. Uses RFC 6587 framing (octet counting or non-transparent).
Maintains a per-peer persistent connection and rotates events across
peers in round-robin order; a peer that errors enters a short cooldown
and is skipped until it recovers.

## Configuration

Single plaintext destination:

```
def output relay {
    type syslog_tcp
    framing octet_counting
    peer { host "10.0.0.1" port 514 }
}
```

Multiple plaintext destinations (round-robin):

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

Single TLS destination with custom CA:

```
def output secure {
    type syslog_tcp
    framing octet_counting
    peer {
        host "collector.example.com"
        tls { ca "/etc/limpid/ca.pem" }
    }
}
```

Mixed TLS and plaintext, with shared and per-peer profiles:

```
def output relay {
    type syslog_tcp
    framing octet_counting

    tls {
        corporate_ca { ca "/etc/limpid/corp-ca.pem" }
        mtls_profile {
            ca   "/etc/limpid/ca.pem"
            cert "/etc/limpid/client.crt"
            key  "/etc/limpid/client.key"
        }
    }

    peers {
        peer { host "a.example.com" tls corporate_ca }              // TLS, port 6514
        peer { host "b.example.com" tls mtls_profile }              // mTLS, port 6514
        peer { host "c.example.com" tls { ca "/etc/limpid/c.pem" } } // inline TLS, port 6514
        peer { host "d.example.com" }                                // plaintext, port 514
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `framing` | no | `octet_counting` | `octet_counting` or `non_transparent` (RFC 6587) |
| `tls` | no | - | Map of named TLS profiles. Each entry is a [tls block](#tls-block) keyed by a user-chosen name; peers reference one via `tls <name>`. |
| `peer` | one of | - | Single-destination block (see [peer block](#peer-block)). Mutually exclusive with `peers`. |
| `peers` | one of | - | Multi-destination block containing repeated `peer` entries. Mutually exclusive with `peer`. |

Exactly one of `peer` or `peers` must be specified.

### peer block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `host` | yes | - | Hostname or IP of the syslog receiver. Also used as the TLS SNI / certificate verification name when `tls` is set. |
| `port` | no | per-peer | TCP port. Defaults to `6514` when this peer has a `tls` block (RFC 5425) and `514` otherwise (RFC 6587). |
| `tls` | no | - | Either an inline [tls block](#tls-block) or a bare identifier referencing a profile defined in the outer `tls { ... }` map. Omit to use plaintext TCP. |

### tls block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `ca` | no | (Mozilla roots) | Path to PEM-encoded CA cert for server verification |
| `cert` | no | - | Path to PEM-encoded client certificate (for mTLS) |
| `key` | no | - | Path to PEM-encoded client private key (for mTLS) |

`cert` and `key` are mutually required: either specify both or neither.

## Framing

- **`octet_counting`** (default) - `MSG-LEN SP SYSLOG-MSG` per RFC 6587 §3.4.1
- **`non_transparent`** - messages terminated by LF per RFC 6587 §3.4.2

Framing is output-wide and applies uniformly to every peer regardless
of whether that peer uses TLS.

## Multi-destination semantics

When `peers` is used, every event is sent to exactly one peer chosen
in round-robin order. Failover and load-balancing happen at the
per-event level: each `write` advances the rotation cursor by one peer.

A peer enters a **5-second cooldown** when a TCP connect, TLS handshake
(for TLS peers — invalid certificate, unknown CA, hostname mismatch,
etc.), write, or flush returns an error. Subsequent rotations skip
cooled-down peers until the cooldown expires; on the next attempt
after expiry, the peer is reconnected and the cooldown is cleared on
success or extended on failure.

If every peer is cooled-down (or fails within the same write call), the
write returns an error and the queued retry path handles the rest. See
the [queue](../outputs/README.md#queue-and-retry) section for retry
behaviour.

## Notes

- The TCP (or TLS) connection to each peer is established lazily on
  first use and reused for subsequent events to that peer.
- A broken connection on a peer is dropped on write error and
  re-established on the next rotation visit (after cooldown).
- TLS connectors are built at startup. Errors in CA / cert / key file
  loading or PEM parsing fail-fast before the daemon starts accepting
  events.
- For UDP, see [syslog_udp](./syslog-udp.md).

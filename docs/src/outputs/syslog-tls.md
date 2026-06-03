# syslog_tls

Sends events to one or more remote syslog endpoints over TLS-encrypted
TCP. Supports server certificate verification against a custom CA or
the Mozilla root store, plus optional mutual TLS (client certificate).
Default port is 6514 (RFC 5425). Round-robin rotation and cooldown
semantics match [syslog_tcp](./syslog-tcp.md).

## Configuration

Single destination with custom CA:

```
def output secure {
    type syslog_tls
    framing octet_counting
    peer {
        host "collector.example.com"
        port 6514
        tls { ca "/etc/limpid/ca.pem" }
    }
}
```

Multiple destinations with shared and per-peer profiles:

```
def output secure {
    type syslog_tls
    framing octet_counting

    tls {
        corporate_ca { ca "/etc/limpid/corp-ca.pem" }
        mtls_profile {
            ca "/etc/limpid/ca.pem"
            cert "/etc/limpid/client.crt"
            key "/etc/limpid/client.key"
        }
    }

    peers {
        peer { host "a.example.com" port 6514 tls corporate_ca }
        peer { host "b.example.com" port 6514 tls corporate_ca }
        peer { host "c.example.com" port 6514 tls mtls_profile }
        peer { host "d.example.com" port 6514 }    // system trust store
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `framing` | no | `octet_counting` | `octet_counting` or `non_transparent` (RFC 6587) |
| `tls` | no | - | Map of named TLS profiles. Each entry is a [tls block](#tls-block) keyed by a user-chosen name; referenced from peers via `tls <name>`. |
| `peer` | one of | - | Single-destination block. Mutually exclusive with `peers`. |
| `peers` | one of | - | Multi-destination block. Mutually exclusive with `peer`. |

Exactly one of `peer` or `peers` must be specified.

### peer block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `host` | yes | - | Hostname or IP. Also used as the TLS SNI / certificate verification name. |
| `port` | no | `6514` | TCP port |
| `tls` | no | - | Either an inline [tls block](#tls-block) or a bare identifier referencing a profile defined in the outer `tls { ... }` map. Omit to use the Mozilla root store with no client certificate. |

### tls block

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `ca` | no | (Mozilla roots) | Path to PEM-encoded CA cert for server verification |
| `cert` | no | - | Path to PEM-encoded client certificate (for mTLS) |
| `key` | no | - | Path to PEM-encoded client private key (for mTLS) |

`cert` and `key` are mutually required: either specify both or neither.

## Framing

Identical to [syslog_tcp](./syslog-tcp.md#framing).

## Multi-destination semantics

Identical to [syslog_tcp](./syslog-tcp.md#multi-destination-semantics).
Cooldown is triggered by TCP connect failures, TLS handshake errors
(invalid certificate, unknown CA, hostname mismatch, etc.), and write
errors.

## Notes

- TLS connectors are built at startup. Errors in CA / cert / key file
  loading or PEM parsing fail-fast before the daemon starts accepting
  events.
- The hostname in each peer's `host` field is used both for TCP connect
  and as the TLS server name for SNI and certificate-name verification.
  Use the name as it appears in the server's certificate.
- For plaintext TCP, see [syslog_tcp](./syslog-tcp.md). For UDP, see
  [syslog_udp](./syslog-udp.md).

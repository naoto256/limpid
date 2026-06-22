# syslog_tcp

Receives syslog messages over TCP with RFC 6587 framing support, with
optional TLS termination (mTLS via client certificate verification).

## Configuration

Plaintext:

```
def input fw_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
    framing auto
    rate_limit 10000
    max_connections 1024
}
```

TLS-terminated (default port 6514):

```
def input secure {
    type syslog_tcp
    framing auto
    tls {
        cert "/etc/limpid/certs/server.crt"
        key  "/etc/limpid/certs/server.key"
    }
}
```

mTLS (clients must present a cert signed by the configured CA):

```
def input mtls_relay {
    type syslog_tcp
    tls {
        cert "/etc/limpid/certs/server.crt"
        key  "/etc/limpid/certs/server.key"
        ca   "/etc/limpid/certs/client-ca.crt"
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | per `tls` (see below) | Listen address |
| `framing` | no | `auto` | `auto`, `octet_counting`, or `non_transparent` |
| `tls` | no | - | TLS block (see [tls block](#tls-block)). Omit for plaintext. |
| `rate_limit` | no | unlimited | Maximum events per second |
| `max_connections` | no | `1024` | Maximum simultaneous TCP connections |

`bind` default port flips with the `tls` block: **6514** (RFC 5425) when
`tls` is present, **514** (RFC 6587) otherwise.

### tls block

| Property | Required | Description |
|----------|----------|-------------|
| `cert` | yes | Path to PEM-encoded server certificate |
| `key` | yes | Path to PEM-encoded private key |
| `ca` | no | Path to CA certificate for **client** verification (mTLS). Without `ca`, any client may connect. |

When `ca` is set, clients must present a certificate signed by that CA;
TLS handshakes from clients without a valid client cert are rejected.

## Framing modes

Per [RFC 6587](https://www.rfc-editor.org/rfc/rfc6587):

- **`auto`** (default) — auto-detects per connection based on the first byte:
  - Digit (1-9) → octet counting
  - `<` → non-transparent framing (LF/CRLF/NUL delimited)
- **`octet_counting`** — `MSG-LEN SP SYSLOG-MSG` format
- **`non_transparent`** — messages delimited by LF, CRLF, or NUL

Framing detection runs after the TLS handshake when `tls` is configured.

## Notes

- PRI validation is enforced on all messages.
- Idle connections are closed after 300 seconds.
- Maximum message size: 1 MiB.
- Connections exceeding `max_connections` are rejected immediately.
- TLS server config (cert / key / CA loading + parsing) happens at
  daemon start — invalid files fail-fast before the listener binds.
- TLS handshakes are bounded at 10 s. A client that opens TCP but
  never completes the handshake is dropped after the timeout so it
  cannot pin a connection slot indefinitely.

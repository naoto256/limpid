# LTP — Authenticated Node Transport

LTP connects limpid nodes without exposing a plaintext fallback. Each
connection uses TLS 1.3 mutual raw-public-key authentication. The configured
RFC 8410 Ed25519 SPKI public key pins the peer, and the first application frame
binds that authenticated key to its configured `node_id`. X.509 certificates
and unlisted keys are rejected.

LTP preserves the event's UUIDv7 key and carries its payload plus a bounded hop
history. The history is runtime metadata: it is visible in JSON tap and is
preserved by disk queues and the dead-letter log, but it is not exposed to the
DSL workspace. Each accepting input rejects a cycle or an incoming history at
its `max_hops` limit before appending its own arrival stamp. An output seals its
local stamp with the successful attempt's departure time. Retries reuse the
same bounded history and refresh only that departure time, so a failed attempt
does not create another hop.

## Provision identities

Every daemon that uses an LTP input or output needs a `node_key`. If `node_id`
is omitted, limpid uses the hostname resolved once at startup; deployments with
multiple instances on the same host should set an explicit, deployment-unique
`node_id`:

```bash
limpidctl ltp keygen /etc/limpid/node-key.pem
```

The command creates a non-overwriting `0600` PKCS#8 private-key file and prints
one base64-encoded SPKI public key to stdout. Exchange only that public line.
Configure the private path and a deployment-unique node identity at top level:

```limpid
node_id "edge-a"
node_key "/etc/limpid/node-key.pem"
```

Startup and reload open the key without following symlinks, validate it through
the same file descriptor, and require a regular file owned by the daemon's
effective user with mode exactly `0400` or `0600`. `limpid --check` validates
the LTP configuration and public keys without reading the private-key file.

## Receive from peers

An input lists every peer permitted to connect. Repeating `peer` is allowed;
duplicate node identities or public keys are rejected at configuration time.

```limpid
def input from_edge {
    type ltp
    bind "0.0.0.0:7514"
    peer {
        node_id "edge-a"
        pubkey "<edge-a SPKI base64>"
    }
    max_hops 16
    max_connections 1024
}
```

| Property | Default | Meaning |
| --- | --- | --- |
| `bind` | `0.0.0.0:7514` | TCP listen address. |
| `peer { node_id pubkey }` | required | An authenticated upstream identity; repeat for each permitted peer. |
| `max_hops` | `16` | Incoming hop limit, from 1 through 16. An event already at the limit is dropped. |
| `max_connections` | `1024` | Maximum concurrent accepted connections. |

Multiple logical LTP inputs may use the exact same `bind` string. Limpid opens
one physical listener for that group, authenticates against the union of its
declared peers, and dispatches each connection to the logical input that owns
the presented public key. The hello `node_id` must then match that key. Events,
input counters, and pipeline delivery belong only to the selected input; the
hello frame is not counted as an input payload.

```limpid
def input ltp_jump01 {
    type ltp
    bind "0.0.0.0:7514"
    peer { node_id "jump01" pubkey "<jump01 SPKI base64>" }
    max_connections 1024
}

def input ltp_jump02 {
    type ltp
    bind "0.0.0.0:7514"
    peer { node_id "jump02" pubkey "<jump02 SPKI base64>" }
    max_connections 1024
}

def pipeline from_jump01 { input ltp_jump01; finish }
def pipeline from_jump02 { input ltp_jump02; finish }
```

Peer `node_id` and public keys must be unique across a shared-listener group.
`max_connections` is listener-wide and must have the same value on every group
member; `max_hops` remains specific to each logical input. Exact bind-string
equality is required for sharing—hostnames and equivalent textual addresses
are not normalized. Non-identical IP binds on the same port are rejected when
their scopes overlap, such as `0.0.0.0:7514` with `127.0.0.1:7514`, or
`[::]:7514` with `[::1]:7514`. Distinct specific addresses remain separate
listeners. Whether `[::]:7514` also conflicts with `0.0.0.0:7514` depends on
the platform's dual-stack socket policy, so that cross-family pair may pass
static overlap validation and then fail at bind time. A bind failure aborts
startup for the whole group and names all affected inputs; Limpid does not
continue with a partially active group.

The wire key must be exactly 16 bytes, use the RFC 4122 variant, and identify a
UUIDv7. Limpid never replaces an invalid or missing key. After authentication,
the hello `node_id` must match the identity assigned to the presented public
key. Protocol errors close that connection; subsequent valid connections are
independent.

## Send to one peer

An LTP output is unbatched and declares exactly one destination:

```limpid
def output to_core {
    type ltp
    peer {
        node_id "core-b"
        pubkey "<core-b SPKI base64>"
        endpoint "core-b.example:7514"
    }
    queue { type disk path "/var/lib/limpid/queues/to-core" }
    retry { max_attempts 10 initial_wait "1s" }
}
```

`endpoint` accepts `host` or `host:port`; the omitted port is `7514`. A new or
reconnected TLS session always sends the authenticated node hello before its
first event. Connection, handshake, and write failures use the ordinary output
retry and dead-letter contract. A payload larger than 16 MiB is a permanent
per-event failure and goes directly to that contract without connecting or
consuming retries. A successful event increments output bytes by the complete
outer event frame only; TLS, TCP, and hello overhead are excluded.

The receiver applies normal channel backpressure, and shutdown uses the same
bounded drain semantics as other outputs. LTP adds no application
acknowledgement; a successful final write and flush is the sender-side handoff
boundary, not confirmation of peer acceptance or durable storage. An ambiguous
connection or write failure follows ordinary retry behavior and can therefore
deliver a duplicate.

## Hop timing

Each hop records `node_id`, `arrival_unix_nano`, and `departure_unix_nano`.
Input appends arrival with departure zero; the corresponding local output fills
departure immediately before its final event write. A node that receives and
then forwards an event therefore contributes one stamp, while the receiving
node's input contributes the next arrival stamp. See [Metrics](./operations/metrics.md#ltp)
for the derived network and intra-node latency segments and rejection counters.

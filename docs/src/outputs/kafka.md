# kafka

Produces events to an Apache Kafka topic. Uses librdkafka internally for batching, compression, retries, and connection management.

Requires the `kafka` feature at build time:

```bash
cargo build --release -p limpid --features kafka
```

## Configuration

```limpid
def output events {
    type kafka
    brokers "kafka1:9092,kafka2:9092"
    topic "syslog-events"
    compression snappy
    acks all
    key source
    queue_timeout "5s"
}
```

TLS to the brokers (with optional client cert for mTLS):

```limpid
def output secure {
    type kafka
    brokers "kafka1.example.com:9093,kafka2.example.com:9093"
    topic "syslog-events"
    tls {
        ca   "/etc/limpid/kafka-ca.pem"
        cert "/etc/limpid/kafka-client.crt"   // optional; required only for mTLS
        key  "/etc/limpid/kafka-client.key"   // optional; pairs with cert
    }
}
```

SASL/SCRAM (over TLS — the typical production combo):

```limpid
def output authenticated {
    type kafka
    brokers "kafka1.example.com:9094"
    topic "syslog-events"
    tls { ca "/etc/limpid/kafka-ca.pem" }
    sasl {
        mechanism scram_sha_512
        username "limpid-producer"
        password_file "/etc/limpid/kafka.pw"   // chmod 600
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `brokers` | yes | — | Comma-separated list of Kafka brokers (bootstrap list) |
| `topic` | yes | — | Target topic name |
| `compression` | no | `none` | `none`, `gzip`, `snappy`, `lz4`, `zstd` |
| `acks` | no | `all` | `0` (fire-and-forget), `1` (leader only), `all` (all replicas) |
| `key` | no | none | Partition key. Only the literal `source` (= source IP address) is accepted; any other identifier is rejected at config-load time. See [Partition key](#partition-key). |
| `queue_timeout` | no | `5s` | Max wait when rdkafka's internal queue is full |
| `tls` | no | — | TLS block (see [tls block](#tls-block)). Omit for plaintext. |
| `sasl` | no | — | SASL block (see [sasl block](#sasl-block)). Omit for no auth. |

`security.protocol` is derived from which blocks are present:

| `tls` | `sasl` | result |
|---|---|---|
| absent | absent | `plaintext` (librdkafka default) |
| present | absent | `ssl` |
| absent | present | `sasl_plaintext` (rejected when `mechanism` is `plain` — see [SASL/PLAIN requires TLS](#saslplain-requires-tls)) |
| present | present | `sasl_ssl` (the recommended production combo) |

### tls block

| Property | Required | Description |
|----------|----------|-------------|
| `ca` | no | Path to PEM-encoded CA cert for broker verification. Omit to use the system root store. |
| `cert` | no | Path to PEM-encoded client certificate (for mTLS). Pairs with `key`. |
| `key` | no | Path to PEM-encoded client private key. Pairs with `cert`. |

`cert` and `key` are both-or-neither: specify them together for mTLS, or
omit both for one-way TLS.

### sasl block

| Property | Required | Description |
|----------|----------|-------------|
| `mechanism` | yes | One of `plain`, `scram_sha_256`, `scram_sha_512`. The DSL ident grammar forbids `-`, so the mechanism is spelled with underscores; limpid maps it to the librdkafka canonical spelling (`SCRAM-SHA-256` / `SCRAM-SHA-512`) internally. |
| `username` | yes | SASL username (not a secret; goes in the config) |
| `password_file` | yes | Path to a file containing the SASL password (the **only** way to set the password — inline `password` is intentionally not supported) |

The `password_file` is read once at daemon start; rotate it and restart
the daemon to refresh credentials. `chmod 600` it and ensure the file
is owned by (or at least readable by) the daemon's service user — under
the packaged systemd unit that is `syslog` (`User=syslog Group=syslog`
in `limpid.service`), so a `password_file` at `/etc/limpid/kafka.pw`
should be `syslog:syslog 0600`. Custom deploys should substitute
whichever user the daemon runs as. A trailing newline is stripped (`\r\n`,
bare `\n`, and bare `\r` are all handled), so `echo "secret" >
/etc/limpid/kafka.pw` works as expected and a CRLF-terminated file from
a Windows host authenticates correctly. An empty file is rejected —
that's almost always a misconfigured secret, not a deliberate empty
password.

### SASL/PLAIN requires TLS

`mechanism plain` transmits the username and password in clear text on
the wire — the only safe transport is TLS. limpid rejects this
combination at config-load time:

```limpid
def output lake {
    type kafka
    brokers "..."
    topic "events"
    sasl {
        mechanism plain        # ← plain on its own is rejected
        username "limpid"
        password_file "/etc/limpid/kafka.pw"
    }
    # NO tls block → daemon refuses to start
}
```

Add a `tls { ... }` block (server CA, optionally `cert` / `key` for
mTLS) to permit `plain`, or switch the mechanism to `scram_sha_256` /
`scram_sha_512` — SCRAM uses a challenge-response so the password is
never sent on the wire and runs safely without TLS. The Kafka project
and Confluent both require TLS for `PLAIN`; limpid enforces this at
config-load rather than letting the daemon start and leak credentials.

Why no inline `password`: inline credentials end up in version-control
diffs, backups, and log output of any tool that pretty-prints the config.
Treating SASL passwords with the same disposition as TLS private keys
(file on disk, restrictive perms) keeps both secrets on the same operational
footing.

## Partition key

The `key` property determines which event field is used as the Kafka partition key. Only the literal value `source` is accepted — events from the same source IP go to the same partition (per-source ordering). Any other identifier (including `workspace.*` paths that earlier limpid releases used for per-tenant or per-field partitioning) is rejected at config-load time with a migration message.

| Value | Key source |
|-------|------------|
| `source` | Source IP address (event-intrinsic, always available) |

If `key` is omitted, the event is sent without a partition key (round-robin across partitions).

For per-tenant or per-content partitioning, split traffic into separate `output kafka` blocks from the pipeline body and give each one its own `topic` (or its own broker cluster):

```limpid
def output kafka_apac { type kafka brokers "..." topic "logs-apac" }
def output kafka_emea { type kafka brokers "..." topic "logs-emea" }

def pipeline route {
    input syslog_udp
    process parse_fortigate | classify_region   // sets workspace.region
    switch workspace.region {
        "apac" { output kafka_apac }
        "emea" { output kafka_emea }
    }
}
```

Routing decisions stay in the pipeline body; the output configs stay static and addressable.

## Notes

- rdkafka handles batching and compression internally — no manual batch configuration needed (unlike [http](./http.md)).
- On graceful shutdown (`SIGTERM`, `SIGHUP` reload, `systemctl stop`), the producer hands each pending message to librdkafka via `try_send`; the pending-envelope wait is bounded by librdkafka's own `queue_timeout` and `message.timeout.ms` (there is no additional outer 5-second wrapper — a previous version had one and it was removed because it cut librdkafka's delivery attempt short at the wire boundary, producing ambiguous outcomes). A message whose `try_send` cannot complete before shutdown routes through the ambiguous DLQ path — `Dropped` on a disk queue so the fail-stop wedge holds the cursor for next-start reconciliation, folded to `Recovered` on a memory queue for lack of a replay path. A DLQ record is written for reconciliation in either case.
- The internal delivery timeout (`message.timeout.ms`) is 30 seconds. If a message can't be delivered within that time, it's returned as an error and the output's `retry { ... }` budget (driven inside `consume()`) handles re-delivery. On retry exhaustion the payload routes to `control { error_log "..." }` as an Output-flavor DLQ record; see [Outputs → Recovery (error_log)](./README.md#recovery-error_log).

## Example

```limpid
def output siem_kafka {
    type kafka
    brokers "kafka1:9092,kafka2:9092,kafka3:9092"
    topic "firewall-logs"
    compression lz4
    key source
}

def pipeline forward {
    input syslog_udp
    process {
        cef.parse(ingress)
        egress = to_json()
    }
    output siem_kafka
}
```

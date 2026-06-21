# otlp_grpc

Forwards events to one or more OpenTelemetry collectors / OTLP-compatible SaaS backends over OTLP/gRPC. Multiple peers are tried in round-robin order with per-peer cooldown on failure.

Each Event's `egress` is expected to be the singleton ResourceLogs protobuf bytes produced by [`otlp.encode_resourcelog_protobuf`](../functions/expression-functions.md#otlp). The output buffers these per-Event ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the batch in an `ExportLogsServiceRequest`, and ships it.

> Why limpid's OTLP behaves the way it does — Resource attributes are user-authored not auto-detected, `partial_success` is not retried selectively, `batch_level` is wire-only and semantically null — is documented in [OTLP — design rationale](../otlp.md). The reference table below covers *how* to configure; the design page covers *why* the defaults are what they are.

## Configuration

```
def output otlp_out {
    type otlp_grpc
    peers {
        peer {
            endpoint "https://collector-a.example.com:4317"
            tls { ca "/etc/limpid/ca.crt" }
        }
        peer {
            endpoint "https://collector-b.example.com:4317"
            tls {
                ca   "/etc/limpid/ca.crt"
                cert "/etc/limpid/client.crt"   # mTLS
                key  "/etc/limpid/client.key"
            }
        }
    }
    batch_size 512
    batch_timeout "5s"
    headers {
        Authorization "Bearer ${env.OTLP_TOKEN}"
    }
}
```

A single-peer setup can use the `peer { ... }` shorthand (same shape `output syslog_tcp` accepts):

```
def output otlp_out {
    type otlp_grpc
    peer {
        endpoint "https://collector.example.com:4317"
        tls { ca "/etc/limpid/ca.crt" }
    }
}
```

`peer { ... }` and `peers { peer { ... } ... }` are mutually exclusive — exactly one of the two must be present.

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `peer { endpoint tls{...} }` or `peers { peer { ... } ... }` | yes (one of) | — | One or more peer blocks. See [§ peers](#peers) below. |
| `batch_size` | no | `1` | Flush after this many Events. `1` ships every Event immediately. |
| `batch_timeout` | no | `5s` | Flush deferred Events after this duration. |
| `batch_level` | no | `none` | One of `none` / `resource` / `scope`. See [§ batch_level](#batch_level). |
| `headers` | no | — | gRPC metadata added to every batch. Keys are lower-cased per HTTP/2 / gRPC convention; tonic rejects mixed-case (e.g. `Authorization`). |
| `retry { max_attempts initial_wait max_wait backoff }` | no | shared default (5 attempts, 1s → 60s exponential) | Per-flush retry budget. Inside one budget the rotation transparently picks the next available peer; the budget caps total attempts across all peers for a single batch. |

### peers

Each `peer` block configures one collector endpoint:

| Per-peer property | Required | Description |
|-------------------|----------|-------------|
| `endpoint` | yes | gRPC server URL. `https://` selects TLS, `http://` selects plaintext. The `LogsService.Export` path is implicit. |
| `tls.ca` | no | Custom CA certificate file (PEM) for this peer. Falls back to the system root store if omitted. |
| `tls.cert`, `tls.key` | no (paired) | Client certificate and private key for mTLS, as separate PEM files (chmod 600 the key). Both must be present together. |

On each flush, peers are tried in round-robin order. A peer that fails the request is marked cooled-down for ~5s and skipped on subsequent flushes until the cooldown expires. The cursor advances per flush so successive flushes start at successive peers; within one flush the `retry` budget protects against transient failures by rotating to the next available peer.

> `verify false` is intentionally not exposed. tonic does not support insecure-skip-verify the way reqwest does; use an `http://` endpoint for plaintext development setups or terminate TLS at a sidecar.

## Pipeline contract

The output expects `egress` to already be valid singleton ResourceLogs proto bytes. It does **not** re-encode — that's the process layer's job. See [otlp_http § Pipeline contract](./otlp_http.md#pipeline-contract) for the encoder pattern; the same wiring applies regardless of transport.

For OTLP-in / OTLP-out topologies, the input can write a valid singleton ResourceLogs to `egress` and this output ships it as-is, no process required.

## `batch_level`

Identical semantics to the [otlp_http batch_level](./otlp_http.md#batch_level) — three wire-form choices that all produce semantically identical OTLP at the receiver.

## Notes

- `partial_success` on the response (rejected log records) is logged as a warning. The internal `retry { … }` block does not branch on `partial_success` — it retries the whole batch on transport failures only. A finer "retry just the rejects" policy is queued for a later release.
- Server TLS uses rustls (aws-lc-rs provider). System root certificates are loaded via tonic's `tls-roots`; supply `tls { ca }` to add a custom CA on top.
- For HTTP transport see [otlp_http](./otlp_http.md).

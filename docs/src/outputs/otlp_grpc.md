# otlp_grpc

Forwards events to an OpenTelemetry collector or OTLP-compatible SaaS backend over OTLP/gRPC.

Each Event's `egress` is expected to be the singleton ResourceLogs protobuf bytes produced by [`otlp.encode_resourcelog_protobuf`](../functions/expression-functions.md#otlp). The output buffers these per-Event ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the batch in an `ExportLogsServiceRequest`, and ships it.

> Why limpid's OTLP behaves the way it does — Resource attributes are user-authored not auto-detected, `partial_success` is not retried selectively, `batch_level` is wire-only and semantically null — is documented in [OTLP — design rationale](../otlp.md). The reference table below covers *how* to configure; the design page covers *why* the defaults are what they are.

## Configuration

```
def output otlp_out {
    type otlp_grpc
    endpoint "https://collector.example.com:4317"
    batch_size 512
    batch_timeout "5s"
    headers {
        Authorization "Bearer ${env.OTLP_TOKEN}"
    }
    tls {
        ca "/etc/limpid/ca.crt"
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `endpoint` | yes | — | gRPC server URL. `https://` selects TLS, `http://` selects plaintext. The `LogsService.Export` path is implicit. |
| `batch_size` | no | `1` | Flush after this many Events. `1` ships every Event immediately. |
| `batch_timeout` | no | `5s` | Flush deferred Events after this duration. |
| `batch_level` | no | `none` | One of `none` / `resource` / `scope`. See [§ batch_level](#batch_level). |
| `headers` | no | — | gRPC metadata added to every batch. Keys are lower-cased per HTTP/2 / gRPC convention; tonic rejects mixed-case (e.g. `Authorization`). |
| `tls.ca` | no | system roots | Custom CA certificate file (PEM). |
| `retry { max_attempts initial_wait max_wait backoff }` | no | shared default (5 attempts, 1s → 60s exponential) | Per-batch retry policy. Retries the **whole** ExportLogsServiceRequest internally so a transient failure does not lose buffered Events. Same `retry { … }` shape every other output uses. |

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

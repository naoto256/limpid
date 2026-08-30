# otlp_grpc

Forwards events to one or more OpenTelemetry collectors / OTLP-compatible SaaS backends over OTLP/gRPC. Multiple peers are tried in round-robin order with per-peer cooldown on failure.

Each Event's `egress` is expected to be the singleton ResourceLogs protobuf bytes produced by [`otlp.encode_resourcelog_protobuf`](../functions/expression-functions.md#otlpencode_resourcelog_protobufhashlit--bytes). The output buffers these per-Event ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the batch in an `ExportLogsServiceRequest`, and ships it.

> Why limpid's OTLP behaves the way it does — Resource attributes are source-adapter-owned rather than auto-detected, `partial_success` is not retried selectively, `batch_level` is wire-only and semantically null — is documented in [OTLP — design rationale](../otlp.md). The reference table below covers *how* to configure; the design page covers *why* the defaults are what they are.

## Configuration

```limpid
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

```limpid
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
| `endpoint` | yes | gRPC server URL. `https://` selects TLS, `http://` selects plaintext. The `LogsService.Export` path is implicit. A `tls { ... }` block is rejected at config-load time if this is not an `https://` URL — tonic only negotiates TLS on https endpoints, so a tls block on a plaintext endpoint would silently ship in clear text. |
| `tls.ca` | no | Custom CA certificate file (PEM) for this peer. Falls back to the system root store if omitted. |
| `tls.cert`, `tls.key` | no (paired) | Client certificate and private key for mTLS, as separate PEM files (chmod 600 the key). Both must be present together. |

On each flush, peers are tried in round-robin order. A peer that fails the request is marked cooled-down for ~5s and skipped on subsequent flushes until the cooldown expires. The cursor advances per flush so successive flushes start at successive peers; within one flush the `retry` budget protects against transient failures by rotating to the next available peer.

Every gRPC `Export` call is bounded by a 30s timeout. A peer that accepts the connection but never returns a HEADERS frame counts as a failure and yields to the next peer in the rotation.

> `verify false` is intentionally not exposed. tonic does not support insecure-skip-verify the way reqwest does; use an `http://` endpoint for plaintext development setups or terminate TLS at a sidecar.

## Pipeline contract

The output expects `egress` to already be valid singleton ResourceLogs proto bytes. It does **not** re-encode — that's the process layer's job. See [otlp_http § Pipeline contract](./otlp_http.md#pipeline-contract) for the encoder pattern; the same wiring applies regardless of transport.

For OTLP-in / OTLP-out topologies, the input can write a valid singleton ResourceLogs to `egress` and this output ships it as-is, no process required.

## `batch_level`

Identical semantics to the [otlp_http batch_level](./otlp_http.md#batch_level) — three wire-form choices that all produce semantically identical OTLP at the receiver.

## Notes

- `partial_success` on the response (rejected log records) bumps `events_failed` by the rejected count and routes the trailing N events (where N = `rejected_log_records`) to `control { error_log "..." }` as Output-flavor DLQ records with `reason = "collector reported partial_success rejection"`. The internal `retry { … }` block does not branch on `partial_success` — it retries the whole batch on transport failures only. A finer "retry just the rejects" policy is queued for a later release.
- **`partial_success.rejected_log_records` attribution is approximate.** The OTLP response reports a rejected *count* for the batch, not the identity of each rejected log record. limpid splits the batch into Delivered + Recovered along the trailing N entries and routes the Recovered tail to the error log as described above. Treat those DLQ entries as a batch-level rejection split into per-event records for replay, not as proof that the collector selected those exact events — the collector did not identify them. Metric totals (`events_written`, `events_failed`) are accurate; per-event attribution is not.
- On a transport-level **final** failure (retry budget exhausted), the drained batch is **not** restored to the in-memory buffer — all still-shippable events are routed to `control { error_log "..." }` as Output-flavor DLQ records and `events_failed` is bumped accordingly.
- On **graceful shutdown** (`SIGTERM`, `SIGHUP` reload, `systemctl stop`, or an explicit `shutdown()` API call), a bounded final drain runs — one flush attempt per parked payload bounded by `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` (3 s) plus the per-actor in-flight cancel. The disposition of each drained event depends on where its failure fired: a per-event render failure (proved pre-boundary) or a retry-exhausted terminal drop routes as `Recovered`, while an in-flight cancel or the 3-second attempt timeout leaves the wire state ambiguous and routes through the ambiguous DLQ path — `Dropped` on a disk queue (fail-stop wedge holds the cursor for next-start reconciliation against the DLQ record) and folded to `Recovered` on a memory queue (no replay path exists). In every case the DLQ record — `event.source`, `event.received_at`, `event.ingress`, and `event.egress` reflect the original per-event provenance because the batched output parks the source `Event` alongside its `QueueAckHandle` until the flush resolves. Without `error_log` configured, the daemon emits a one-line `tracing::error!` summary per drain-failure event (site + reason, no payload) — the payload is not persisted anywhere by default. To attach metadata or the full JSONL to the tracing line, set `control { error_log_fallback "meta" | "full" }` alongside `error_log` (see the [tracing fallback ladder](../operations/error-log.md#tracing-fallback-ladder-error_log_fallback)). **`SIGKILL` (`kill -9`) does not run this path** — actor tasks are aborted and the stack-local buffer is lost; do not send `SIGKILL` directly to the daemon. See [Queue and retry → Recovery (error_log)](./README.md#recovery-error_log).
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).
- Client TLS uses tonic's TLS integration (`ClientTlsConfig` backed by rustls with the ring provider). System root certificates are loaded via tonic's `tls-native-roots` feature; supply per-peer `tls { ca }` to add a custom CA on top.
- For HTTP transport see [otlp_http](./otlp_http.md).

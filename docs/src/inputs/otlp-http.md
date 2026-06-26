# otlp_http

Receives OpenTelemetry logs over the OTLP/HTTP transport. Listens for `POST /v1/logs` and accepts both `application/x-protobuf` (canonical) and `application/json` request bodies.

> Why limpid's OTLP behaves the way it does (Resource attributes are user-authored, `received_at` ≠ `time_unix_nano`, partial_success is not retried, …) is documented in [OTLP — design rationale](../otlp.md). Read that before opening an issue about a missing default.

## Configuration

```
def input otlp_in {
    type otlp_http
    bind "0.0.0.0:4318"            // OTLP/HTTP default port
    body_limit "16MB"              // optional per-request size cap
    rate_limit 10000               // optional events/sec budget
    request_rate_limit 1000        // optional req/sec budget
    max_concurrent_requests 64     // optional in-flight req cap

    // Optional TLS (HTTPS). Omit the block for plaintext HTTP.
    tls {
        cert "/etc/limpid/cert.pem"
        key  "/etc/limpid/key.pem"
        ca   "/etc/limpid/client-ca.pem"   // optional; enables mTLS
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:4318` | TCP listen address |
| `body_limit` | no | `16MB` | Per-request body size cap. Larger requests are rejected with HTTP 413 *Payload Too Large* before any decode work runs. Accepts `KB` / `MB` / `GB` suffixes or a bare byte count. Tune up for OTLP collectors that batch tens of MB of logs per RPC, down for hostile-network ingest. |
| `rate_limit` | no | unlimited | Sustained **events**-per-second cap (positive integer). Each emitted Event consumes 1 token; over-budget records `acquire().await` until the token bucket refills. Applied *after* request decode and split. Same implementation as the `syslog_*` inputs. |
| `request_rate_limit` | no | unlimited | Sustained **requests**-per-second cap (positive integer). One token per RPC, applied *before* decode. Smooths sustained QPS without bounding peak concurrency — pair with `max_concurrent_requests` for memory protection. |
| `max_concurrent_requests` | no | unlimited | In-flight request cap (positive integer). Worst-case decode memory becomes `max_concurrent_requests × body_limit`, turning the open-ended decode-amplification path into a known quantity. Excess requests are rejected with HTTP 503 *Service Unavailable* (fail-fast — OTLP senders typically retry, so backpressuring the socket would amplify overload). |
| `tls` | no | - | Optional TLS block (see [tls block](#tls-block)). When present the listener accepts HTTPS only — there is no HTTP fallback on the same port. |

### tls block

| Property | Required | Description |
|----------|----------|-------------|
| `cert` | yes | Path to PEM-encoded server certificate |
| `key` | yes | Path to PEM-encoded server private key |
| `ca` | no | Path to CA cert for **client** verification (mTLS). With `ca`, clients without a valid cert signed by it are rejected at handshake. |

Cert / key / CA files are loaded and parsed at daemon start; bad files
fail-fast before the listener binds. Same shape as `input syslog_tcp`
and `input otlp_grpc`.

The four budgets stack as orthogonal defense layers. A typical exposed-ingress preset:

```
body_limit "16MB"              # bytes per request
max_concurrent_requests 64     # peak concurrency → ≤1 GiB worst-case decode
request_rate_limit 1000        # sustained RPS, smooths bursts
rate_limit 100000              # pipeline send rate (events/sec)
```

For a loopback / sidecar deployment you can typically omit all four — the four defaults (16 MiB body, no other cap) match what the OpenTelemetry collector itself does.

## Per-Event shape

Each LogRecord in the incoming `ExportLogsServiceRequest` becomes one Event. The input does not interpret payload semantics (Principle 2 — input is dumb transport); decoding is the process layer's job.

| Field | Value |
|-------|-------|
| `ingress` | singleton ResourceLogs (1 Resource + 1 Scope + 1 LogRecord) encoded as protobuf wire bytes |
| `egress` | identical to `ingress` (process layer rewrites if needed) |
| `source` | TCP peer address |
| `received_at` | `Utc::now()` at request handling time |
| `workspace` | empty |

To structure the LogRecord into workspace fields, decode it explicitly in a process:

```
def process unpack_otlp {
    workspace.otlp = otlp.decode_resourcelog_protobuf(ingress)
    // workspace.otlp.scope_logs[0].log_records[0].body.string_value, etc.
}
```

See [`otlp.decode_resourcelog_protobuf`](../functions/expression-functions.md#otlpdecode_resourcelog_protobufbytes--object).

## Splitting policy

A request may carry many ResourceLogs / ScopeLogs / LogRecords. The input splits along the LogRecord axis: one LogRecord per Event. Resource and Scope metadata are preserved on each split — the per-Event ResourceLogs is a singleton (one Resource + one Scope) so all the originating context travels with the LogRecord.

This matches Principle 4 (atomic events through the pipeline): the input is the only layer with the right to split, and it splits to the smallest meaningful unit.

## Content-type detection

| Header | Decoder |
|--------|---------|
| `application/x-protobuf`, `application/protobuf` | prost (canonical) |
| `application/json` | serde_json (camelCase) |
| missing or other | falls back to protobuf decode |

A decode failure returns HTTP 400 and increments `events_invalid`. Successful but empty requests return HTTP 200 with no events emitted.

## Pure pass-through

If the pipeline has no process layer, an OTLP/HTTP → `otlp_http` (or `otlp_grpc`) output topology relays without re-encoding — `egress` is already valid singleton ResourceLogs proto bytes:

```
def pipeline otlp_relay {
    input otlp_in
    output otlp_out
}
```

## TLS

Native HTTPS is served via the [`tls` block](#tls-block) at the
top of this page (since v0.7.6) — `tls { cert key }` for plain server TLS, plus
optional `ca` for mTLS (client-cert verification at handshake). The
same shape is shared with [`input syslog_tcp`](./syslog-tcp.md) and
[`input otlp_grpc`](./otlp-grpc.md). Without the block, the listener
serves plaintext HTTP on the configured port.

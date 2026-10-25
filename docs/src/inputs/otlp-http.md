# otlp_http

Receives OpenTelemetry logs over the OTLP/HTTP transport. Listens for `POST /v1/logs` and accepts both `application/x-protobuf` (canonical) and `application/json` request bodies.

> Why limpid's OTLP behaves the way it does (external-log Resource identity describes the source rather than the forwarder, `received_at` is observation time rather than event time, partial_success is not retried, ...) is documented in [OTLP — design rationale](../otlp.md). Read that before opening an issue about a missing default.

## Configuration

```limpid
def input otlp_in {
    type otlp_http
    bind "0.0.0.0:4318"            // OTLP/HTTP default port
    body_limit "16MB"              // optional per-request size cap
    body_timeout "30s"             // total body receive deadline
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
| `body_timeout` | no | `30s` | Total time allowed to receive an admitted request body. Returns HTTP 408 *Request Timeout* and releases its concurrency permit on expiry. Accepts `ms`, `s`, `m`, or bare milliseconds; zero and invalid durations fail startup. Does not cover headers, decode, throttling, or dispatch. |
| `rate_limit` | no | unlimited | Sustained **events**-per-second cap (positive integer). Each emitted Event consumes 1 token; over-budget records `acquire().await` until the token bucket refills. Applied *after* request decode and split. Same implementation as the `syslog_*` inputs. |
| `request_rate_limit` | no | unlimited | Sustained **requests**-per-second cap (positive integer). One token per RPC, applied *before* decode. Smooths sustained QPS without bounding peak concurrency — pair with `max_concurrent_requests` for memory protection. |
| `max_concurrent_requests` | no | unlimited | In-flight request cap (positive integer). The permit is acquired before reading the body and held through decode and dispatch, bounding buffered request bodies to `max_concurrent_requests × body_limit` bytes. Decoded requests, emitted events, and transport buffers are outside this bound. Excess requests are rejected without reading their bodies with HTTP 503 *Service Unavailable* (fail-fast — OTLP senders typically retry, so backpressuring the socket would amplify overload). |
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

The five budgets stack as orthogonal defense layers. A typical exposed-ingress preset:

```
body_limit "16MB"              # bytes per request
max_concurrent_requests 64     # ≤1 GiB buffered request bodies only
body_timeout "30s"             # total body receive deadline
request_rate_limit 1000        # sustained RPS, smooths bursts
rate_limit 100000              # pipeline send rate (events/sec)
```

Decoded data and split events can be much larger than the received body: Resource and Scope metadata are copied into every LogRecord event. Emitted events remain in the input channel and pipeline after the request releases its permit. The fixed 4096-event input channel and downstream backpressure control queued event counts and progress; they do not establish a fixed byte or total-memory bound.

Omitting these properties keeps the 16 MiB body cap and 30s body receive deadline, with no concurrency or rate cap.

The default deadline requires about 0.55 MB/s to deliver a full 16 MiB batch within 30 seconds. Collectors sending large batches over slower links can now receive 408; increase `body_timeout` to accommodate their expected transfer time.

For inputs exposed to untrusted networks such as the Internet, use mTLS or a front-end proxy that authenticates clients and limits connections. A body deadline limits one request's permit holding time; clients repeatedly reconnecting with stalled uploads can still occupy the available permits and cause other senders to receive 503.

### Rejection order and receive metrics

1. A declared `Content-Length` exceeding `body_limit` returns 413 immediately, before acquiring a permit or reading the body, including when concurrency is full.
2. Otherwise, full concurrency returns 503 without reading the body. This also applies to oversized bodies whose size is not declared (for example, chunked uploads).
3. Admitted bodies are collected under the byte limit and total receive deadline. Exceeding the byte limit returns 413; expiry returns 408. For streamed bodies, whichever condition is observed first determines the response. Both release the permit. Request-rate throttling, decode, and dispatch follow successful collection.

`events_received` counts completely collected requests, including empty requests and requests that subsequently fail decoding; it does not count individual LogRecords. `bytes_received` counts their complete body bytes before decoding. Admission failures (503), oversized bodies (413), and timed-out bodies (408) contribute neither counter, including any partial bytes received. A later dispatch failure can still return 503 after these counters have increased. Previously, concurrency rejections were counted after their bodies had been read; dashboards may therefore show lower receive counts under overload.

## Per-Event shape

Each LogRecord in the incoming `ExportLogsServiceRequest` becomes one Event. The input does not interpret payload semantics (Principle 2 — input is dumb transport); decoding is the process layer's job.

| Field | Value |
|-------|-------|
| `ingress` | singleton ResourceLogs (1 Resource + 1 Scope + 1 LogRecord) encoded as protobuf wire bytes |
| `egress` | identical to `ingress` (process layer rewrites if needed) |
| `source` | TCP peer address |
| `received_at` | The host wall clock at request handling time |
| `workspace` | empty |

To structure the LogRecord into workspace fields, decode it explicitly in a process:

```limpid
def process unpack_otlp {
    workspace.otlp = otlp.decode_resourcelog_protobuf(ingress)
    // first(first(workspace.otlp.scope_logs).log_records).body.string_value, etc.
}
```

See [`otlp.decode_resourcelog_protobuf`](../functions/expression-functions.md#otlpdecode_resourcelog_protobufbytes_or_string--object).

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

```limpid
def pipeline otlp_relay {
    input otlp_in
    output otlp_out
}
```

## TLS

Native HTTPS is served via the [`tls` block](#tls-block) at the
top of this page — `tls { cert key }` for plain server TLS, plus
optional `ca` for mTLS (client-cert verification at handshake). The
same shape is shared with [`input syslog_tcp`](./syslog-tcp.md) and
[`input otlp_grpc`](./otlp-grpc.md). Without the block, the listener
serves plaintext HTTP on the configured port.

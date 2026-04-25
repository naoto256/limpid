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

See [`otlp.decode_resourcelog_protobuf`](../processing/functions.md#otlp).

## Splitting policy

A request may carry many ResourceLogs / ScopeLogs / LogRecords. The input splits along the LogRecord axis: one LogRecord per Event. Resource and Scope metadata are preserved on each split — the per-Event ResourceLogs is a singleton (one Resource + one Scope) so all the originating context travels with the LogRecord.

This matches Principle 6 (pipelines do not change Event granularity): the input is the only layer with the right to split, and it splits to the smallest meaningful unit.

## Content-type detection

| Header | Decoder |
|--------|---------|
| `application/x-protobuf`, `application/protobuf` | prost (canonical) |
| `application/json` | serde_json (camelCase) |
| missing or other | falls back to protobuf decode |

A decode failure returns HTTP 400 and increments `events_invalid`. Successful but empty requests return HTTP 200 with no events emitted.

## Pure pass-through

If the pipeline has no process layer, an OTLP/HTTP → `otlp` output topology relays without re-encoding — `egress` is already valid singleton ResourceLogs proto bytes:

```
def pipeline otlp_relay {
    input otlp_in
    output otlp_out
}
```

## TLS

Server-side TLS is **not** implemented for `otlp_http` as of v0.5.0. Front the input with a reverse proxy (envoy, nginx, traefik) for TLS termination, or use [`otlp_grpc`](./otlp-grpc.md) — its `tls { cert key ca }` block does ship in v0.5.0 and supports both plain TLS and mutual TLS. Native HTTPS support for `otlp_http` is queued for v0.5.x.

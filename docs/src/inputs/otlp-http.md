# otlp_http

Receives OpenTelemetry logs over the OTLP/HTTP transport. Listens for `POST /v1/logs` and accepts both `application/x-protobuf` (canonical) and `application/json` request bodies.

## Configuration

```
def input otlp_in {
    type otlp_http
    bind "0.0.0.0:4318"   // OTLP/HTTP default port
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:4318` | TCP listen address |

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

Server-side TLS is not implemented in v0.5.0. Front the input with a reverse proxy (envoy, nginx, traefik) for TLS termination, or use the `otlp_grpc` input with a TLS-terminating sidecar. Native TLS support is queued for v0.5.x.

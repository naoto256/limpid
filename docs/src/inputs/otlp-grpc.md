# otlp_grpc

Receives OpenTelemetry logs over the OTLP/gRPC transport. Hosts the `opentelemetry.proto.collector.logs.v1.LogsService` gRPC service.

## Configuration

```
def input otlp_in {
    type otlp_grpc
    bind "0.0.0.0:4317"   // OTLP/gRPC default port
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:4317` | TCP listen address |

## Per-Event shape

Identical to [`otlp_http`](./otlp-http.md): one LogRecord becomes one Event with `ingress` set to the singleton ResourceLogs proto bytes (1 Resource + 1 Scope + 1 LogRecord). Resource and Scope metadata are preserved per-Event.

| Field | Value |
|-------|-------|
| `ingress` | singleton ResourceLogs proto bytes |
| `egress` | identical to `ingress` |
| `source` | TCP peer address |
| `received_at` | `Utc::now()` when the RPC arrived |
| `workspace` | empty |

## Reply

The handler returns an empty `ExportLogsServiceResponse` on full success. If some LogRecords could not be re-encoded (very rare), the response carries `partial_success` with `rejected_log_records` set so the sender can retry just those — matching the OTLP collector convention.

```protobuf
message ExportLogsServiceResponse {
    ExportLogsPartialSuccess partial_success = 1;  // populated only on partial fail
}
message ExportLogsPartialSuccess {
    int64  rejected_log_records  = 1;
    string error_message         = 2;
}
```

## TLS / mTLS

Server-side TLS is not implemented in v0.5.0. Front with a TLS-terminating sidecar (envoy, nginx with grpc-tls, traefik) — the gRPC framing survives proxy termination because TLS sits below it. Native server TLS is queued for v0.5.x.

## Pure pass-through

The gRPC input pairs naturally with the gRPC output for collector-to-collector relay:

```
def pipeline otlp_relay {
    input otlp_grpc_in
    output otlp_grpc_out  // type otlp, protocol "grpc"
}
```

## Splitting policy

Same as `otlp_http` (see that page's Splitting policy section). One Export RPC may carry many LogRecords; each becomes one Event with full Resource / Scope context preserved.

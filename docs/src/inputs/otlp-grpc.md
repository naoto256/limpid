# otlp_grpc

Receives OpenTelemetry logs over the OTLP/gRPC transport. Hosts the `opentelemetry.proto.collector.logs.v1.LogsService` gRPC service.

> Why limpid's OTLP behaves the way it does (Resource attributes are user-authored, `received_at` ≠ `time_unix_nano`, partial_success is not retried, …) is documented in [OTLP — design rationale](../otlp.md). Read that before opening an issue about a missing default.

## Configuration

```
def input otlp_in {
    type otlp_grpc
    bind "0.0.0.0:4317"   // OTLP/gRPC default port
    rate_limit 10000      // optional events/sec budget
    tls {                 // optional, plaintext when omitted
        cert "/etc/limpid/tls/server.crt"
        key  "/etc/limpid/tls/server.key"
        ca   "/etc/limpid/tls/ca.crt"      // optional, enables mTLS
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `bind` | no | `0.0.0.0:4317` | TCP listen address |
| `rate_limit` | no | unlimited | Sustained events-per-second cap (positive integer). Per-Event token-bucket throttle, identical to `otlp_http` and the `syslog_*` inputs. |
| `tls.cert` | yes (when `tls` present) | — | Server certificate PEM file |
| `tls.key` | yes (when `tls` present) | — | Server private key PEM file |
| `tls.ca` | no | — | Client-CA PEM. When present, the server requires and verifies a client certificate (mutual TLS). |

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

Server-side TLS is configured with the optional `tls { … }` block (see Properties above). With `cert` + `key` only the server presents a certificate and accepts any client; adding `ca` switches it into mutual-TLS mode where every client must present a certificate signed by that CA root.

Cert / key files are loaded once at startup via a blocking task pool, so a slow disk does not stall the tokio reactor. `SIGHUP` reload re-reads them, picking up rotated material.

For setups where TLS terminates outside limpid (envoy, nginx, traefik, cloud LB), omit the block and listen plaintext on a loopback interface.

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

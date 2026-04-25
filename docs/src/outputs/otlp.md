# otlp

Forwards events to an OpenTelemetry collector or OTLP-compatible SaaS backend over any of the three OTLP transports: `http_json`, `http_protobuf`, `grpc`.

Each Event's `egress` is expected to be the singleton ResourceLogs protobuf bytes produced by [`otlp.encode_resourcelog_protobuf`](../processing/functions.md#otlp). The output buffers these per-Event ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the batch in an `ExportLogsServiceRequest`, and ships it.

## Configuration

```
def output otlp_out {
    type otlp
    endpoint "https://collector.example.com:4318/v1/logs"
    protocol "http_protobuf"   // http_json | http_protobuf | grpc
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
| `endpoint` | yes | — | OTLP endpoint URL. Full path including `/v1/logs` for HTTP transports. |
| `protocol` | no | `http_protobuf` | One of `http_json`, `http_protobuf`, `grpc`. |
| `batch_size` | no | `1` | Flush after this many Events. `1` ships every Event immediately. |
| `batch_timeout` | no | `5s` | Flush deferred Events after this duration. |
| `batch_level` | no | `none` | v0.5.0 only supports `none` (pure concat). |
| `headers` | no | — | HTTP headers (HTTP transports) / gRPC metadata (gRPC). Map keys are lower-cased for gRPC per HTTP/2 convention. |
| `tls.ca` | no | system roots | Custom CA certificate file (PEM). |
| `verify` | no | `true` | TLS verify (HTTP only — gRPC does not support `verify false`; use `http://` for plaintext). |

## Endpoint conventions

| Transport | Endpoint shape |
|-----------|---------------|
| `http_json` / `http_protobuf` | full URL including `/v1/logs` (limpid does not append it). |
| `grpc` | gRPC server URL — the `LogsService.Export` path is implicit. `https://` triggers TLS, `http://` is plaintext. |

## Pipeline contract

The output expects `egress` to already be valid singleton ResourceLogs proto bytes. It does **not** re-encode — that's the process layer's job. Typical wiring:

```
def process compose_otlp_from_ocsf {
    workspace.otlp = {
        resource: { attributes: [
            { key: "service.name", value: { string_value: workspace.metadata.product.name } }
        ]},
        scope_logs: [{
            scope: { name: "limpid", version: "0.5.0" },
            log_records: [{
                time_unix_nano: workspace.event_time_ns,
                severity_number: 9,
                severity_text: "INFO",
                body: { string_value: to_json(workspace.ocsf) }
            }]
        }]
    }
    egress = otlp.encode_resourcelog_protobuf(workspace.otlp)
}

def pipeline syslog_to_otlp {
    input syslog_udp
    process parse_fortigate
          | compose_ocsf_detection_finding
          | compose_otlp_from_ocsf
    output otlp_out
}
```

If `egress` is not a valid ResourceLogs proto, flush errors with `pipeline egress is not a valid ResourceLogs proto (wire it through 'otlp.encode_resourcelog_protobuf')`.

## Pure relay

For OTLP-in / OTLP-out topologies, no process is required — the input writes a valid singleton ResourceLogs to `egress`, and the output ships it as-is:

```
def pipeline otlp_relay {
    input otlp_in        // type otlp_http or otlp_grpc
    output otlp_out
}
```

## `batch_level`

OTLP receivers accept an `ExportLogsServiceRequest` with multiple `ResourceLogs` entries, even when several share the same `Resource` or `(Resource, Scope)` pair. The proto3 `repeated` semantics make a "pure concat" batch (one entry per Event) and a merged batch (entries collapsed by Resource / Scope) **semantically identical** at the receiver — same set of LogRecords arrive, only the framing differs.

| `batch_level` | Wire form | Status |
|---------------|-----------|--------|
| `none` (default) | one ResourceLogs entry per Event | **shipped in v0.5.0** |
| `resource` | merge same-Resource Events into one ResourceLogs | queued for v0.5.x |
| `scope` | merge same-(Resource, Scope) into one ScopeLogs | queued for v0.5.x |

The merging modes are wire-efficiency optimisations; if your batch sizes are modest (hundreds of Events), `none` is fine.

## gRPC notes

- `partial_success` on the response (rejected log records) is logged as a warning. Retry / drop policy is queued for v0.5.x.
- Headers map to gRPC metadata. Tonic enforces lower-case keys; limpid lower-cases on the way through.
- Server TLS uses rustls (aws-lc-rs provider). System root certificates are loaded via `tonic`'s `tls-roots`; supply `tls { ca }` to add a custom CA.

## HTTP notes

- HTTP/JSON serializes per the OTLP/JSON canonical mapping (camelCase, u64-as-string, bytes-as-hex).
- HTTP/protobuf is the canonical OTLP wire form.
- The same `tls { ca }` block applies; `verify false` skips certificate verification (development only).

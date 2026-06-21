# otlp_http

Forwards events to an OpenTelemetry collector or OTLP-compatible SaaS backend over OTLP/HTTP, in either `http_protobuf` (default) or `http_json` wire format.

Each Event's `egress` is expected to be the singleton ResourceLogs protobuf bytes produced by [`otlp.encode_resourcelog_protobuf`](../functions/expression-functions.md#otlp). The output buffers these per-Event ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the batch in an `ExportLogsServiceRequest`, and ships it.

> Why limpid's OTLP behaves the way it does — Resource attributes are user-authored not auto-detected, `partial_success` is not retried selectively, `batch_level` is wire-only and semantically null — is documented in [OTLP — design rationale](../otlp.md). The reference table below covers *how* to configure; the design page covers *why* the defaults are what they are.

## Configuration

```
def output otlp_out {
    type otlp_http
    endpoint "https://collector.example.com:4318/v1/logs"
    protocol "http_protobuf"   // http_protobuf | http_json
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
| `endpoint` | yes | — | Full OTLP/HTTP URL including `/v1/logs` (limpid does not append it). |
| `protocol` | no | `http_protobuf` | `http_protobuf` (canonical OTLP wire form) or `http_json` (OTLP/JSON canonical mapping). |
| `batch_size` | no | `1` | Flush after this many Events. `1` ships every Event immediately. |
| `batch_timeout` | no | `5s` | Flush deferred Events after this duration. |
| `batch_level` | no | `none` | One of `none` / `resource` / `scope`. See [§ batch_level](#batch_level). |
| `headers` | no | — | HTTP request headers added to every batch. |
| `tls.ca` | no | system roots | Custom CA certificate file (PEM). |
| `verify` | no | `true` | Verify the server's TLS certificate. `false` skips verification (development only). |
| `retry { max_attempts initial_wait max_wait backoff }` | no | shared default (5 attempts, 1s → 60s exponential) | Per-batch retry policy. Retries the **whole** ExportLogsServiceRequest internally so a transient failure does not lose buffered Events. Same `retry { … }` shape every other output uses. |

## Pipeline contract

The output expects `egress` to already be valid singleton ResourceLogs proto bytes. It does **not** re-encode — that's the process layer's job. Typical wiring:

```
def process compose_otlp_from_ocsf {
    workspace.otlp = {
        resource: { attributes: [
            { key: "service.name", value: { string_value: workspace.limpid.metadata.product.name } }
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

| `batch_level` | Wire form | CPU | Wire size |
|---------------|-----------|-----|-----------|
| `none` (default) | one ResourceLogs entry per Event | cheapest | largest |
| `resource` | merge same-Resource Events into one ResourceLogs | + linear Resource scan | smaller |
| `scope` | merge same-(Resource, Scope) into one ScopeLogs | + Scope scan inside each Resource | smallest |

Resource / Scope equality is order-insensitive on attributes — proto3 does not promise a canonical attribute order on the wire, so attribute lists are sorted by key before comparison.

The merging modes are wire-efficiency optimisations; if your batch sizes are modest (hundreds of Events), `none` is fine. For collector → SaaS hops where every byte counts, `scope` is usually the right choice.

## Notes

- `http_protobuf` is the canonical OTLP wire form; `http_json` serializes per the OTLP/JSON canonical mapping (camelCase, u64-as-string, bytes-as-hex).
- `verify false` skips certificate verification — development only.
- For gRPC transport see [otlp_grpc](./otlp_grpc.md).

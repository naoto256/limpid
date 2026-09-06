# Send syslog to Loki with JSON or OTLP

Send syslog to Loki in either of two ways: build its native JSON push payload, or send an OTLP log record and let Loki map the attributes. Both keep the original log line; the difference is who defines the stream labels and metadata.

|                   | HTTP/JSON                                    | OTLP/HTTP                                     |
| ----------------- | -------------------------------------------- | --------------------------------------------- |
| limpid output     | `http`                                       | `otlp_http`                                   |
| Endpoint          | `/loki/api/v1/push`                          | `/otlp/v1/logs`                               |
| Payload           | Loki `streams` JSON                          | OTLP protobuf                                 |
| Labels            | Explicit `stream` object                     | Receiver-selected resource attributes         |
| Additional fields | In the line, or optional structured metadata | OTLP attributes mapped to structured metadata |

Choose one route for a given event unless duplicate ingestion is intentional. The examples below are alternatives, not two outputs to enable together.

## Option A: build Loki's JSON payload

Loki groups log lines into streams identified by labels. Each `values` entry contains a **string timestamp in Unix nanoseconds** and the log line. Use `to_json` to handle quotes, backslashes, and Unicode rather than assembling JSON by string concatenation.

```limpid
control {
    error_log "/var/log/limpid/errors.jsonl"
}

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output loki {
    type http
    peer { url "http://127.0.0.1:3100/loki/api/v1/push" }
    content_type "application/json"
    batch_size 1
}

def process loki_json {
    egress = to_json({
        streams: [{
            stream: { job: "syslog" },
            values: [["${to_int(received_at)}", ingress]]
        }]
    })
}

def pipeline syslog_to_loki {
    input syslog_local
    process loki_json
    output loki
}
```

This example assumes Loki is reachable locally without authentication. The loopback input is intentional: use a controlled listener and network policy if accepting remote senders. Give limpid write access to the error-log directory. UDP can lose messages; this is a small forwarding example, not a lossless transport guarantee.

### Two details that matter

- `"${to_int(received_at)}"` produces a string containing Unix nanoseconds. `${received_at}` alone produces a formatted timestamp instead. This recipe uses arrival time at limpid, not the device's event time.
- Keep `batch_size 1`: each event is already a complete JSON document with its own `streams` envelope. Generic HTTP batching does not merge these documents into a single Loki payload. Raising the value is not a valid way to batch this recipe.

The original syslog header and message stay in `ingress`, so both are stored as the Loki log line. The input expects a valid syslog message, not arbitrary text. The fixed `job` label keeps stream cardinality bounded; do not turn request IDs, arbitrary messages, or client IPs into labels. Add a few stable labels only when they are useful for querying.

See the [Loki HTTP API](https://grafana.com/docs/loki/latest/reference/loki-http-api/) for the JSON push format and [label guidance](https://grafana.com/docs/loki/latest/get-started/labels/) for stream design.

## Configure the receiving side

Use an existing, healthy Loki deployment with its distributor or single-binary HTTP endpoint available. `/loki/api/v1/push` is the ingestion endpoint—not the Grafana UI URL and not the OTLP endpoint. The receiving deployment must already have working schema/storage configuration; this recipe does not replace it.

For a remote destination, use HTTPS with certificate verification and configure the required authentication at your gateway. Loki's tenant selection header `X-Scope-OrgID` is not an authentication credential. Multi-tenant deployments require the appropriate tenant on both writes and queries; do not disable authentication or tenant isolation merely to reuse the loopback example.

The two-element `values` entries above do not use structured metadata. You do not need to enable it just for this payload. If adding structured metadata later, check the receiving Loki version, schema, and limits first.

## Check the line you actually stored

Send a valid syslog message carrying a distinct test marker, then query only that marker with the `job` selector:

```logql
{job="syslog"} |= "LOKI_RECIPE_TEST"
```

Compare the complete stored line, including punctuation and non-ASCII characters. A successful HTTP request is useful evidence, but querying the record confirms that it is available to readers. Also inspect output failures and the error log; delivery retries are not an exactly-once guarantee.

## Option B: send OTLP and map attributes in Loki

Use Loki's native OTLP/HTTP endpoint, with the full `/otlp/v1/logs` path: limpid does not append `/v1/logs`. This is not Loki's JSON push API, even though both use HTTP. The example selects protobuf transport explicitly.

```limpid
include "/usr/share/limpid/snippets/composers/compose_otlp.limpid"

control {
    error_log "/var/log/limpid/errors.jsonl"
}

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output loki_otlp {
    type otlp_http
    peer { endpoint "http://127.0.0.1:3100/otlp/v1/logs" }
    protocol "http_protobuf"
    batch_size 1
}

def process syslog_for_loki {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "source.ip", value: { string_value: source.ip } }
    ]
}

def pipeline syslog_to_loki {
    input syslog_local
    process syslog_for_loki | compose_otlp | otlp_to_egress
    output loki_otlp
}
```

The source-specific process chooses the body and attributes. The shared composer builds one ResourceLogs protobuf value; `otlp_http` wraps it in the export request. Unlike generic HTTP/JSON, the OTLP output understands how to batch these records, so its batch size can be tuned after measuring your workload.

### Loki: choose labels deliberately

Native OTLP ingestion needs structured metadata enabled and compatible storage/schema configuration (TSDB with schema v13). Merge the following **limits fragment** into your own Loki configuration; it is not a complete storage or server configuration:

```yaml
limits_config:
  allow_structured_metadata: true
  otlp_config:
    resource_attributes:
      ignore_defaults: true
      attributes_config:
        - action: index_label
          attributes:
            - service.name
```

This selects only `service.name` from the resource attributes for indexing. It does not change the native JSON route's explicit labels. Preserve other required limits and tenant overrides when merging it; do not replace a deployment's whole configuration with this fragment or rewrite historical schema entries.

| OTLP field in this example        | Loki representation                                 |
| --------------------------------- | --------------------------------------------------- |
| Resource attribute `service.name` | Index label `service_name`                          |
| Log-record body                   | Stored log line                                     |
| Log-record timestamp              | Log timestamp, using limpid receipt time here       |
| Log attribute `source.ip`         | Structured metadata `source_ip`, not an index label |

Loki normalizes dots in attribute names to underscores. Select the stream with the normalized label and filter metadata separately:

```logql
{service_name="syslog-forwarder"} |= "LOKI_RECIPE_TEST"
```

```logql
{service_name="syslog-forwarder"} | source_ip="192.0.2.10"
```

The address above is a documentation placeholder. Keeping sender addresses out of index labels avoids creating a new stream for every address. Resource attributes and log attributes are different locations: putting `service.name` in log attributes will not satisfy the resource-attribute indexing rule above.

See Grafana's [native OTLP ingestion and mapping guide](https://grafana.com/docs/loki/latest/send-data/otel/) and [structured metadata requirements](https://grafana.com/docs/loki/latest/get-started/labels/structured-metadata/). The same HTTPS, gateway authentication, and tenant-selection considerations described for Option A apply here. Do not disable verification or authentication to make this example work remotely.

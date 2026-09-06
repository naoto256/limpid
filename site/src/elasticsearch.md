# Send syslog to Elasticsearch with Bulk or OTLP

Make syslog searchable in Elasticsearch without a Logstash hop. Build a JSON document when you want to choose its fields, or emit an OTLP log record when you want a telemetry-shaped document. Use Kibana to explore the resulting index or data stream.

|                             | Bulk API                 | Native OTLP/HTTP                        |
| --------------------------- | ------------------------ | --------------------------------------- |
| limpid output               | `http`                   | `otlp_http`                             |
| Endpoint                    | `/limpid-syslog/_bulk`   | `/_otlp/v1/logs`                        |
| Wire format                 | NDJSON action + document | OTLP protobuf                           |
| Destination in this example | `limpid-syslog` index    | `logs-generic.otel-default` data stream |
| Original line               | `message`                | `body.text`                             |

Choose one route for each event. These are two alternative configurations, not two outputs to enable together.

## Configure Elasticsearch

For Bulk, prepare the `limpid-syslog` index or its index template, field mappings, and retention policy. Give the writer permission to index documents there. For native OTLP, use an Elasticsearch version that exposes `/_otlp/v1/logs`; its default log destination is `logs-generic.otel-default`. The OTLP API key needs `create_doc` and `auto_configure` privileges on the destination data streams.

The configurations below use a local HTTP endpoint at `127.0.0.1:9200`. Replace it with your deployment's endpoint and configure its HTTPS trust and authentication in both limpid and the query client. Do not disable authentication to match the example. Keep API keys out of shared configuration files.

Elasticsearch's native endpoint is not the APM Server endpoint. For Elastic Cloud or a Collector-based topology, use the endpoint and routing policy provided by that deployment. See the [native OTLP endpoint guide](https://www.elastic.co/docs/manage-data/ingest/otlp-endpoint).

## Option A: choose the document with Bulk

Each incoming line becomes one indexing action and one JSON document. `to_json` escapes the content; the concatenated newlines provide NDJSON framing, including the required final newline.

```limpid
def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output elasticsearch {
    type http
    peer { url "http://127.0.0.1:9200/limpid-syslog/_bulk" }
    content_type "application/x-ndjson"
    batch_size 1
}

def process make_document {
    egress = to_json({ index: {} }) + "\n"
           + to_json({ message: ingress, received: "${received_at}" }) + "\n"
}

def pipeline syslog_to_elastic {
    input syslog_local
    process make_document
    output elasticsearch
}
```

The URL selects the index. The document keeps the original line in `message` and formats limpid's arrival time in `received`. This is not device event-time parsing.

Keep `batch_size 1` for this configuration: each event already contains its action/document pair and trailing newline.

### HTTP success is not per-document success

Elasticsearch can return HTTP 200 with `errors: true` and failed entries in `items`. limpid's generic HTTP output checks the HTTP status; it does **not** interpret Elasticsearch's item-level results. A mapping rejection can therefore be acknowledged at the transport layer without the document being indexed. A disk queue does not close that gap. Where per-document recovery is required, use a Bulk-aware ingestion component and monitor rejected items. See the [Bulk API response contract](https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-bulk).

## Option B: send an OTLP log record

Use the installed OTLP composer to build protobuf bytes. The resource identifies the service, the body preserves the syslog line, and a log attribute carries the sender IP.

```limpid
include "/usr/share/limpid/snippets/composers/compose_otlp.limpid"

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output elasticsearch {
    type otlp_http
    peer { endpoint "http://127.0.0.1:9200/_otlp/v1/logs" }
    protocol http_protobuf
    batch_size 1
}

def process syslog_for_elastic {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "source.ip", value: { string_value: source.ip } }
    ]
}

def pipeline syslog_to_elastic {
    input syslog_local
    process syslog_for_elastic | compose_otlp | otlp_to_egress
    output elasticsearch
}
```

This endpoint accepts OTLP/HTTP protobuf, not OTLP/gRPC or OTLP JSON. Its default log destination is `logs-generic.otel-default`; `data_stream.dataset` and `data_stream.namespace` attributes can select a different data stream. This example leaves them unset. Request retries are not an exactly-once guarantee. Consult the [OTLP routing and delivery limitations](https://www.elastic.co/docs/manage-data/ingest/otlp-endpoint) when choosing a deployment topology.

Elasticsearch stores the OTLP values in these fields:

| OTLP value                | Searchable document field          |
| ------------------------- | ---------------------------------- |
| Body string               | `body.text`                        |
| Resource `service.name`   | `resource.attributes.service.name` |
| Log attribute `source.ip` | `attributes.source.ip`             |
| Log timestamp             | `@timestamp`                       |

That is intentionally different from the Bulk example's hand-built `message` and `received` fields. Changing the output protocol alone does not make those two document shapes identical.

## Find the logs in Kibana

For Bulk, select `limpid-syslog` and search the `message` field. For OTLP, select `logs-generic.otel-default` and search `body.text`.

Allow for Elasticsearch's refresh interval before expecting a new document in search. In Kibana, select the corresponding index or data stream. Use `@timestamp` for the OTLP time filter; for Bulk, use `received` if mapped as a date, or create the data view without a time filter.

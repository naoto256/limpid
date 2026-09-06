# Send syslog to Elasticsearch

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

## Choose raw forwarding or parsed fields

Each option starts by forwarding the original line without parsing it. Its FortiGate variation then extracts fields from a CEF-formatted IPS event. Use the same sample for both:

```json
{
  "ingress": "<134>Sep  6 10:00:00 fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|deviceExternalId=FG-EXAMPLE cat=utm:ips FTNTFGTsubtype=ips FTNTFGTseverity=high src=192.0.2.10 spt=36208 dst=198.51.100.5 dpt=9100 proto=6 act=detected FTNTFGTattack=Example.Signature FTNTFGTattackid=12345 msg=Example attack detected",
  "source": {
    "ip": "192.0.2.1",
    "port": 514
  }
}
```

Save it as `event.json`. For the parsed variations, keep the [snippet library](https://github.com/naoto256/limpid/tree/v0.8.4/packaging/snippets) from the same release as your binary under `packaging/snippets/` beside the configuration. Keep its directory structure for helper includes. The example declares the device timezone as `UTC`; replace that with the device's actual IANA timezone or fixed offset. RFC 3164 has no year, so the parser supplies the runtime year. This sample is FortiGate **CEF**, not FortiGate's other syslog formats.

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

### Parse FortiGate fields before indexing

Create a new index with explicit field types before sending the parsed variation. In Kibana Dev Tools:

```http
PUT limpid-fortigate
{
  "mappings": {
    "properties": {
      "message": { "type": "text" },
      "event_time": { "type": "date", "format": "epoch_millis" },
      "severity_number": { "type": "integer" },
      "source": { "properties": {
        "ip": { "type": "ip" },
        "port": { "type": "integer" }
      }},
      "destination": { "properties": {
        "ip": { "type": "ip" },
        "port": { "type": "integer" }
      }},
      "rule": { "properties": {
        "name": { "type": "keyword" }
      }}
    }
  }
}
```

Use an unused index name if this index already exists with different mappings; field types cannot simply be changed in place. Keep Option A's input and output, but change the output URL to `http://127.0.0.1:9200/limpid-fortigate/_bulk`. Replace its pipeline with this fragment:

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def process fortigate_document {
    let document = {
        message: ingress,
        event_time: to_int(workspace.lsis.parsed.time / 1000000),
        severity_number: workspace.lsis.parsed.severity_number,
        source: {
            ip: workspace.lsis.parsed.src_endpoint.ip,
            port: workspace.lsis.parsed.src_endpoint.port
        },
        destination: {
            ip: workspace.lsis.parsed.dst_endpoint.ip,
            port: workspace.lsis.parsed.dst_endpoint.port
        },
        rule: { name: workspace.lsis.parsed.finding_info.title }
    }
    egress = to_json({ index: {} }) + "\n" + to_json(document) + "\n"
}

def pipeline syslog_to_elastic {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef | fortigate_document
    output elasticsearch
}
```

The document retains the full original line in `message`, but now IPs support IP queries, ports support numeric ranges, and the rule name supports exact filtering and aggregation. `event_time` is the parsed device time in epoch milliseconds; use it as the Kibana data view's time field. CEF priority `7` becomes `severity_number=19`, independently of the outer syslog PRI.

For example, this query combines an IP subnet with a destination port and counts events by detection rule:

```http
GET limpid-fortigate/_search
{
  "size": 0,
  "query": {
    "bool": {
      "filter": [
        { "term": { "source.ip": "192.0.2.0/24" } },
        { "term": { "destination.port": 9100 } }
      ]
    }
  },
  "aggs": {
    "rules": { "terms": { "field": "rule.name" } }
  }
}
```

The sample matches with `rule.name=Example.Signature`, source port `36208`, and destination IP `198.51.100.5`. The Bulk item-level failure caveat above still applies: a successful HTTP status alone does not prove these mappings accepted the document. See [explicit mapping](https://www.elastic.co/docs/manage-data/data-store/mapping/explicit-mapping) and the [IP field type](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/ip).

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

### Parse FortiGate fields before composing OTLP

Keep Option B's input, output, and composer include. Replace its pipeline with this fragment. Do not call the original body-only process: it would overwrite the parsed event time or adapter fields.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def pipeline syslog_to_elastic {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef
          | fortigate_cef_to_otlp | compose_otlp | otlp_to_egress
    output elasticsearch
}
```

The parser extracts device facts; its bundled `fortigate_cef_to_otlp` adapter chooses their OTLP locations. The shared composer supplies the wire format. For this sample:

| Location                   | Result                                                                                                                                                      |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Resource attributes        | `observer.vendor=Fortinet`, `observer.product=Fortigate`, `observer.type=firewall`                                                                          |
| Log attributes             | `source.ip=192.0.2.10`, `source.port=36208`, `destination.ip=198.51.100.5`, `destination.port=9100`, `rule.name=Example.Signature`, `event.action=detected` |
| Body                       | CEF message beginning `CEF:0\|Fortinet\|Fortigate\|`, without the syslog wrapper                                                                            |
| Event time / observed time | Parsed device time / limpid receipt time                                                                                                                    |
| SeverityNumber             | `19`, from CEF priority `7`                                                                                                                                 |

This adapter does not invent `service.name` or an instrumentation scope. Here, `source.ip` means the endpoint inside the firewall event, not the sender of the UDP packet. If you also need the full wire line in the OTLP body, set `workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }` in a separate process **after** the adapter and before the composer.

Elasticsearch's native OTLP endpoint manages the data-stream template. The Bulk mapping above does not apply to it. With the native OTel mappings in Elasticsearch 9.5, these fields are already typed:

| Field                                                   | Type      | Use                                  |
| ------------------------------------------------------- | --------- | ------------------------------------ |
| `attributes.source.ip`, `attributes.destination.ip`     | `ip`      | Address or subnet filters            |
| `attributes.source.port`, `attributes.destination.port` | `long`    | Numeric filters and ranges           |
| `attributes.rule.name`                                  | `keyword` | Exact matches and terms aggregations |
| `@timestamp`                                            | `date`    | Device event-time filter             |

Check `_field_caps` on your destination if its version or templates differ; do not replace the managed template with the Bulk mapping. The body remains in `body.text`, vendor identity in `resource.attributes.observer.vendor`, and severity in `severity_number`.

The equivalent structured query uses the OTLP field paths:

```http
GET logs-generic.otel-default/_search
{
  "size": 0,
  "query": {
    "bool": {
      "filter": [
        { "term": { "attributes.source.ip": "192.0.2.0/24" } },
        { "term": { "attributes.destination.port": 9100 } }
      ]
    }
  },
  "aggs": {
    "rules": { "terms": { "field": "attributes.rule.name" } }
  }
}
```

### Inspect the parsed variation locally

Save one assembled variation as `fortigate.conf` and use the sample `event.json` above:

```sh
limpid --check --config fortigate.conf
limpid --test-pipeline syslog_to_elastic --config fortigate.conf --input "$(cat event.json)"
```

Test mode processes the event without starting the listener or sending to the destination. JSON egress can be read directly; OTLP egress is protobuf bytes, not a readable JSON trace. To inspect it locally, temporarily append a process with `egress = to_json(otlp.decode_resourcelog_protobuf(egress))` after `otlp_to_egress`, run test mode, then remove that inspection process before sending to an OTLP output. Confirm the destination's stored fields separately when you enable delivery. A different FortiGate category can populate different fields or be rejected by the parser.

## Find the logs in Kibana

For Bulk, select `limpid-syslog` and search the `message` field. For OTLP, select `logs-generic.otel-default` and search `body.text`.

Allow for Elasticsearch's refresh interval before expecting a new document in search. In Kibana, select the corresponding index or data stream. Use `@timestamp` for the OTLP time filter; for Bulk, use `received` if mapped as a date, or create the data view without a time filter.

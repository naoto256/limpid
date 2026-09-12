# Send syslog to Datadog

Keep the original syslog line and add searchable context before sending it directly to Datadog. Choose JSON fields and Datadog tags, or an OpenTelemetry log body with resource and log attributes. Neither route needs a Datadog Agent or an intermediate collector.

## Configure the Datadog destination

Use an API key from the Datadog organization that should receive the logs. Both examples use **AP1**; choose the endpoint for your organization's Datadog site, not the web application's URL. JSON and OTLP have different intake hosts. See the [Send logs API](https://docs.datadoghq.com/api/latest/logs/#send-logs) and [OTLP logs intake](https://docs.datadoghq.com/opentelemetry/setup/otlp_ingest/logs/) for the corresponding endpoints and payload requirements.

Use the static header objects shown below. `DD_API_KEY` is not the same header as `DD-API-KEY`; the lowercase `dd-api-key` spelling in the OTLP example is equivalent because HTTP header names are case-insensitive.

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

Save it as `event.json`. For the parsed variations, keep the [snippet library](https://github.com/naoto256/limpid/tree/v0.9.0/packaging/snippets) from the same release as your binary under `packaging/snippets/` beside the configuration. Keep its directory structure for helper includes. The example declares the device timezone as `UTC`; replace that with the device's actual IANA timezone or fixed offset. RFC 3164 has no year, so the parser supplies the runtime year. This sample is FortiGate **CEF**, not FortiGate's other syslog formats.

## Option A: choose the JSON fields

```limpid
def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output datadog {
    type http
    peer { url "https://http-intake.logs.ap1.datadoghq.com/api/v2/logs" }
    content_type "application/json"
    batch_size 1
    headers {
        "DD-API-KEY": "<DATADOG_API_KEY>"
    }
}

def process datadog_document {
    egress = to_json({
        message: ingress,
        service: "syslog-forwarder",
        ddsource: "syslog",
        hostname: "host01",
        ddtags: "env:production,route:syslog"
    })
}

def pipeline syslog_to_datadog {
    input syslog_local
    process datadog_document
    output datadog
}
```

Replace the API-key placeholder only in a private configuration readable by the service account. Do not commit the real key, paste it into command history, or include it in shared diagnostics. Keep HTTPS certificate verification enabled.

`message: ingress` sends the complete received line, including its syslog header. `to_json` handles quotes, backslashes, and non-ASCII text; do not construct JSON by concatenating unescaped log text. Each event is one JSON object, so keep `batch_size 1` for this configuration rather than joining objects into an invalid JSON document.

`service`, `ddsource`, and `ddtags` provide searchable context. The fixed `hostname` is illustrative: replace it with the appropriate host identity for your logs. A collector receiving multiple devices should derive that value from validated event data rather than assigning every sender the collector's identity.

### Parse FortiGate fields before sending JSON

Keep Option A's input and output definitions. Replace its pipeline with the following includes, processes, and pipeline; do not run both pipelines on the same input. The original forwarding process may remain unused.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def process fortigate_document {
    egress = to_json({
        message: ingress,
        event_time_unix_nano: workspace.lsis.parsed.time,
        severity_number: workspace.lsis.parsed.severity_number,
        source: { ip: workspace.lsis.parsed.src_endpoint.ip, port: workspace.lsis.parsed.src_endpoint.port },
        destination: { ip: workspace.lsis.parsed.dst_endpoint.ip, port: workspace.lsis.parsed.dst_endpoint.port },
        rule: { name: workspace.lsis.parsed.finding_info.title },
        ddsource: "fortigate"
    })
}

def pipeline syslog_to_datadog {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef | fortigate_document
    output datadog
}
```

The sample produces `source.ip = 192.0.2.10`, `destination.port = 9100`, `rule.name = Example.Signature`, and `severity_number = 19`. This severity comes from the FortiGate CEF priority, not the outer syslog PRI. `message` still contains the complete original line. `event_time_unix_nano` is a numeric field containing the parsed device time, not an instruction to remap the destination's timestamp.

Expand the received JSON event and inspect its nested `source`, `destination`, and `rule` fields. Configure destination parsing/facets as needed. This variation does not set the raw example's service or route tags; search for `Example.Signature` in the message instead.

## Option B: preserve OTLP structure

Use this configuration instead of Option A. Place the matching [compose_otlp.limpid snippet](https://github.com/naoto256/limpid/blob/v0.9.0/packaging/snippets/composers/compose_otlp.limpid) beside the configuration file. Datadog's direct OTLP logs intake accepts HTTP Protobuf at `/v1/logs`.

```limpid
include "compose_otlp.limpid"

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output datadog {
    type otlp_http
    protocol http_protobuf
    peer { endpoint "https://otlp.ap1.datadoghq.com/v1/logs" }
    batch_size 1
    headers {
        "dd-api-key": "<DATADOG_API_KEY>"
    }
}

def process datadog_log {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "route", value: { string_value: "otlp" } }
    ]
}

def pipeline syslog_to_datadog {
    input syslog_local
    process datadog_log | compose_otlp | otlp_to_egress
    output datadog
}
```

The original line goes into the OTLP body. `service.name` identifies the service on the resource, while `route` is a log attribute. `compose_otlp` builds the protobuf payload and `otlp_to_egress` passes it to the output. The timestamp is limpid's receive time; this example does not parse the timestamp inside the syslog text. Header values are static strings, so replace the API-key placeholder in the private configuration rather than using an environment-variable template.

### Parse FortiGate fields before composing OTLP

Keep Option B's input, output, and composer include. Replace its pipeline with this fragment. Do not call the original body-only process: it would overwrite the parsed event time or adapter fields.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def pipeline syslog_to_datadog {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef
          | fortigate_cef_to_otlp | compose_otlp | otlp_to_egress
    output datadog
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

In Log Explorer, locate `Example.Signature` in the body, then expand resource and log attributes. Do not reuse the raw example's service/route filters: the FortiGate adapter does not set them. Receiver pipelines may normalize field names or remap severity and timestamps; inspect the stored event before making saved searches.

### Inspect the parsed variation locally

Save one assembled variation as `fortigate.conf` and use the sample `event.json` above:

```sh
limpid --check --config fortigate.conf
limpid --test-pipeline syslog_to_datadog --config fortigate.conf --input "$(cat event.json)"
```

Test mode processes the event without starting the listener or sending to the destination. JSON egress can be read directly; OTLP egress is protobuf bytes, not a readable JSON trace. To inspect it locally, temporarily append a process with `egress = to_json(otlp.decode_resourcelog_protobuf(egress))` after `otlp_to_egress`, run test mode, then remove that inspection process before sending to an OTLP output. Confirm the destination's stored fields separately when you enable delivery. A different FortiGate category can populate different fields or be rejected by the parser.

## Find the logs in Datadog

Open **Log Explorer** in the same Datadog site and organization, select a recent time range, and filter by `service:syslog-forwarder`. For Option A, you can also use `source:syslog`; Option B does not set that Datadog source tag. Find a distinctive string from your input and expand the event to inspect the original line and context. Intake acceptance and a searchable log are separate observations; an onboarding screen still waiting for logs does not by itself establish that delivery failed.

Check the organization's ingestion pipelines, exclusion filters, and index routing if accepted logs do not appear in the expected search. Request retries are not an exactly-once guarantee. This example does not establish outage recovery, throughput, or retention guarantees.

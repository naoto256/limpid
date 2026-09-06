# Send syslog to Better Stack

Send the original syslog line directly to Better Stack over HTTPS. Use JSON for a message with a few searchable fields, or OTLP for an OpenTelemetry log body, resource, and attributes. Neither route needs an intermediate collector.

## Configure the Better Stack source

In Better Stack, open **Sources** and choose **Connect source**. Give the source a name and select the platform matching the data you will send: HTTP for the JSON route, or follow the OpenTelemetry setup for OTLP. Platform selection controls automatic parsing; do not select a device-specific format merely because a syslog line is carried inside the message. See [source setup](https://betterstack.com/docs/logs/logging-start/) and [OpenTelemetry ingestion](https://betterstack.com/docs/logs/open-telemetry/).

Copy the source's **Ingesting host** and **Source token** from its configuration. Both examples use `ingesting-host.example` as a placeholder: replace it with that exact host, not the dashboard URL or a host copied from another source. Authenticate with `Authorization: Bearer <SOURCE_TOKEN>`. This is the source's ingestion token, not a management API token; the display name and numeric source ID are not needed in the request.

Replace the token only in a private configuration readable by the service account. Header values are literal strings, not environment-variable templates. Keep the real token out of Git, shell history, and shared diagnostics, and leave TLS certificate verification enabled.

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

## Option A: choose the JSON fields

The [HTTP ingestion endpoint](https://betterstack.com/docs/logs/ingesting-data/http/logs/) accepts a JSON object at `/`. Put the original line in `message` and add fields you want to search by.

```limpid
def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output betterstack {
    type http
    peer { url "https://ingesting-host.example/" }
    content_type "application/json"
    batch_size 1
    headers {
        "Authorization": "Bearer <SOURCE_TOKEN>"
    }
}

def process betterstack_document {
    egress = to_json({
        message: ingress,
        service: "syslog-forwarder",
        route: "json"
    })
}

def pipeline syslog_to_betterstack {
    input syslog_local
    process betterstack_document
    output betterstack
}
```

`message: ingress` preserves the whole received line, including its syslog header. `to_json` escapes quotes, backslashes, and non-ASCII text. Keep `batch_size 1` here: this process produces one JSON object per event, not a JSON array for a multi-event batch. Without a `dt` field, Better Stack uses reception time for the event timestamp.

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
        rule: { name: workspace.lsis.parsed.finding_info.title }
    })
}

def pipeline syslog_to_betterstack {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef | fortigate_document
    output betterstack
}
```

The sample produces `source.ip = 192.0.2.10`, `destination.port = 9100`, `rule.name = Example.Signature`, and `severity_number = 19`. This severity comes from the FortiGate CEF priority, not the outer syslog PRI. `message` still contains the complete original line. `event_time_unix_nano` is a numeric field containing the parsed device time, not an instruction to remap the destination's timestamp.

Expand the received JSON event and inspect its nested `source`, `destination`, and `rule` fields. Configure destination parsing/facets as needed. This variation does not set the raw example's service or route tags; search for `Example.Signature` in the message instead.

## Option B: preserve OTLP structure

Use this configuration instead of the JSON configuration. Place the matching [compose_otlp.limpid snippet](https://github.com/naoto256/limpid/blob/v0.8.4/packaging/snippets/composers/compose_otlp.limpid) beside your configuration file. The endpoint is `/v1/logs` on the source's ingesting host, with the same Bearer authentication.

```limpid
include "compose_otlp.limpid"

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output betterstack {
    type otlp_http
    protocol http_protobuf
    peer { endpoint "https://ingesting-host.example/v1/logs" }
    batch_size 1
    headers {
        "Authorization": "Bearer <SOURCE_TOKEN>"
    }
}

def process betterstack_log {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "route", value: { string_value: "otlp" } }
    ]
}

def pipeline syslog_to_betterstack {
    input syslog_local
    process betterstack_log | compose_otlp | otlp_to_egress
    output betterstack
}
```

The adapter supplies the original line as the log body, identifies the service on the resource, and adds `route` as a log attribute. `compose_otlp` builds the protobuf payload; `otlp_to_egress` passes it to the output. This example uses limpid's receive time rather than parsing a timestamp from the syslog text. See the [OTLP/HTTP output reference](../outputs/otlp_http.md) for transport and partial-rejection handling.

### Parse FortiGate fields before composing OTLP

Keep Option B's input, output, and composer include. Replace its pipeline with this fragment. Do not call the original body-only process: it would overwrite the parsed event time or adapter fields.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def pipeline syslog_to_betterstack {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef
          | fortigate_cef_to_otlp | compose_otlp | otlp_to_egress
    output betterstack
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

In Live Tail, locate `Example.Signature` in the body, then expand resource and log attributes. Do not reuse the raw example's service/route filters: the FortiGate adapter does not set them. Receiver pipelines may normalize field names or remap severity and timestamps; inspect the stored event before making saved searches.

### Inspect the parsed variation locally

Save one assembled variation as `fortigate.conf` and use the sample `event.json` above:

```sh
limpid --check --config fortigate.conf
limpid --test-pipeline syslog_to_betterstack --config fortigate.conf --input "$(cat event.json)"
```

Test mode processes the event without starting the listener or sending to the destination. JSON egress can be read directly; OTLP egress is protobuf bytes, not a readable JSON trace. To inspect it locally, temporarily append a process with `egress = to_json(otlp.decode_resourcelog_protobuf(egress))` after `otlp_to_egress`, run test mode, then remove that inspection process before sending to an OTLP output. Confirm the destination's stored fields separately when you enable delivery. A different FortiGate category can populate different fields or be rejected by the parser.

## Inspect the received logs

Open the source's Live Tail, select a recent time range, and find a distinctive string from your log. Expand the event to inspect the full JSON `message` or OTLP body, service context, and `route`. Check quotes, backslashes, and non-ASCII text in the event itself, not just the abbreviated list display or an AI explanation.

If the displayed fields differ from your payload, check the source platform's parser and any configured transformations. An accepted request and the final stored event are different checkpoints. For missing JSON events, check the host/token pair, ingestion pause state, and quota; HTTP `403` indicates an invalid source token and `402` indicates a quota or spending limit. For OTLP, also check partial rejection rather than treating every HTTP success as acceptance of every record. Retries can create duplicates after an ambiguous response.

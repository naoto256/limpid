# Send syslog to New Relic

Send logs directly from limpid to New Relic over HTTPS. Choose JSON for a message with searchable fields, or OTLP for an OpenTelemetry body, resource, and attributes. Neither route needs an intermediate collector.

## Configure New Relic

These examples use a **US account** and its **license (ingest) key**, not a User API key. JSON uses `https://log-api.newrelic.com/log/v1`; OTLP uses `https://otlp.nr-data.net/v1/logs`. For another region, use the corresponding endpoint from the [Log API documentation](https://docs.newrelic.com/docs/logs/log-api/introduction-log-api/) or [OTLP documentation](https://docs.newrelic.com/docs/opentelemetry/best-practices/opentelemetry-otlp/).

Replace the placeholder only in a private configuration readable by the service account. limpid header values are literal strings, not environment-variable templates or an encrypted secret store. Protect configuration includes, backups, and diagnostics as well as the primary file. Never put the real key in Git, command arguments, or a shared transcript. Leave TLS verification enabled.

## Choose raw forwarding or parsed fields

Use the same FortiGate CEF fixture as the Datadog and Better Stack recipes:

<!-- prettier-ignore -->
```json
{"ingress":"<134>Sep  6 10:00:00 fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|deviceExternalId=FG-EXAMPLE cat=utm:ips FTNTFGTsubtype=ips FTNTFGTseverity=high src=192.0.2.10 spt=36208 dst=198.51.100.5 dpt=9100 proto=6 act=detected FTNTFGTattack=Example.Signature FTNTFGTattackid=12345 msg=Example attack detected","source":{"ip":"192.0.2.1","port":514},"received_at":1789285800000000000}
```

Save it as `event.json`. Keep the matching [snippet library](https://github.com/naoto256/limpid/tree/v0.9.0/packaging/snippets) under `packaging/snippets/` beside the configuration, including helper directories. The parsed variations declare `UTC`; replace it with the device's actual timezone. RFC 3164 has no year, so the parser supplies the runtime year.

For a live test, update the sample date and time to current UTC, set `received_at` to the corresponding Unix nanoseconds, and add a unique marker to `msg`. Keep each Event on one physical line: live injection reads JSONL, not pretty-printed JSON. Live `inject --json` requires `received_at`, unlike the smaller fixture accepted by pipeline test mode. Do not replay an old fixture unchanged and expect it in a recent-time search. This is FortiGate **CEF**, not a parser for every FortiGate log format.

## Option A: choose the JSON fields

```limpid
def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output newrelic {
    type http
    peer { url "https://log-api.newrelic.com/log/v1" }
    content_type "application/json"
    batch_size 1
    headers { "Api-Key": "<NEW_RELIC_LICENSE_KEY>" }
}

def process newrelic_document {
    egress = to_json({
        message: ingress,
        service: "syslog-forwarder",
        route: "json"
    })
}

def pipeline syslog_to_newrelic {
    input syslog_local
    process newrelic_document
    output newrelic
}
```

`message: ingress` carries the whole received line, including its syslog header. `to_json` handles quoting and escaping. Keep `batch_size 1`: this process produces one JSON object, not a multi-event JSON array. With no `timestamp`, New Relic uses ingestion time.

### Parse FortiGate fields before sending JSON

Keep Option A's input and output. Replace its pipeline with the following fragment; do not attach both pipelines to the same input.

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

def pipeline syslog_to_newrelic {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef | fortigate_document
    output newrelic
}
```

The fixture produces `source.ip = 192.0.2.10`, `destination.port = 9100`, `rule.name = Example.Signature`, and `severity_number = 19`. That severity comes from CEF priority `7`, not the outer syslog PRI. `event_time_unix_nano` is an ordinary numeric attribute, not a remapping of New Relic's timestamp.

New Relic flattens nested JSON attributes, so inspect `source.ip`, `destination.port`, and `rule.name`. The parsed variation does not set the raw example's `service` or `route`; search for your marker or `Example.Signature` instead.

## Option B: preserve OTLP structure

Use this configuration instead of Option A. It sends protobuf through limpid's `otlp_http` output, not a separate HTTP client.

```limpid
include "packaging/snippets/composers/compose_otlp.limpid"

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output newrelic {
    type otlp_http
    protocol http_protobuf
    peer { endpoint "https://otlp.nr-data.net/v1/logs" }
    batch_size 1
    headers { "api-key": "<NEW_RELIC_LICENSE_KEY>" }
}

def process newrelic_log {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "route", value: { string_value: "otlp" } }
    ]
}

def pipeline syslog_to_newrelic {
    input syslog_local
    process newrelic_log | compose_otlp | otlp_to_egress
    output newrelic
}
```

The process chooses the body and attributes; the shared composer creates protobuf bytes. `otlp_to_egress` passes those bytes to the output. This raw variant uses receive time rather than the date inside the syslog line.

### Parse FortiGate fields before composing OTLP

Keep Option B's input, output, and composer include. Replace its pipeline with this fragment. Do not call `newrelic_log` after the adapter: that would overwrite its event time, body, and attributes.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"

def process fortigate_timezone {
    workspace.fortigate_cef.timezone = "UTC"
}

def pipeline syslog_to_newrelic {
    input syslog_local
    process parse_syslog | parse_cef | fortigate_timezone | parse_fortigate_cef
          | fortigate_cef_to_otlp | compose_otlp | otlp_to_egress
    output newrelic
}
```

| Location              | Fixture result                                                                                                                                              |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Resource              | `observer.vendor=Fortinet`, `observer.product=Fortigate`, `observer.type=firewall`                                                                          |
| Log attributes        | `source.ip=192.0.2.10`, `source.port=36208`, `destination.ip=198.51.100.5`, `destination.port=9100`, `rule.name=Example.Signature`, `event.action=detected` |
| Body                  | CEF text beginning `CEF:0\|Fortinet\|Fortigate\|`, without the syslog wrapper                                                                               |
| Event / observed time | Parsed device time / limpid receipt time                                                                                                                    |
| SeverityNumber        | `19`                                                                                                                                                        |

The adapter does not invent `service.name` or an instrumentation scope. Here, `source.ip` is the endpoint in the firewall event, not the transport sender. To retain the full wire line in the OTLP body, add a separate process assigning `workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }` after the adapter and before the composer.

## Test with a fixture

Save one assembled variation as `newrelic.conf`. Inspect it without opening a listener or sending anything:

```sh
limpid --check --config newrelic.conf
limpid --test-pipeline syslog_to_newrelic --config newrelic.conf --input "$(cat event.json)"
```

For readable OTLP inspection, temporarily append a process with `egress = to_json(otlp.decode_resourcelog_protobuf(egress))` after `otlp_to_egress`. Remove that inspection process before live delivery: the OTLP output expects protobuf, not decoded JSON.

For live delivery, use an isolated test configuration with its own control socket and error log. Start that instance, then inject the fixture into its named input:

```sh
limpidctl --socket /path/to/test-control.sock inject input syslog_local --json < event.json
```

On Windows, use the test instance's named pipe instead of the Unix socket path. Inject once, inspect that instance's output counters and error log, and stop it normally. No firewall or Event Log subscription is needed to exercise the parser, composer, and output. Do not inject into a production input by accident.

## Inspect the received logs

Open **Logs**, select a recent time range, and search for the unique marker in your fixture. Expand the event and compare its body, time, and attributes. You do not need NRQL for the initial text search. In a query interface, an equivalent optional check is:

```sql
SELECT * FROM Log WHERE message LIKE '%YOUR_UNIQUE_MARKER%' SINCE 30 minutes ago
```

Inspect the stored result rather than assuming it has the same field layout as the request. JSON text inside `message` can be parsed automatically. The raw examples' service/route filters do not apply to the parsed FortiGate variations.

Keep requests below 1,000,000 bytes; `batch_size` counts events, not bytes. Check New Relic's current limits before forwarding large records. Before continuous operation, configure [failure recovery](../operations/error-log.md) and review retry, queue, and shutdown behavior. An ambiguous response can lead to duplicate delivery. A successful output disposition is not proof that a record is searchable, nor does it certify every receiver-side transformation.

### Validation scope

With limpid 0.9.0, executable checks read the configuration and single-line Event directly from this article. Raw and parsed JSON, raw and parsed OTLP, and the optional OTLP wire-body override passed through running local senders and receivers with expected contents, counts, and normal exit. These local checks replace only the destination, key placeholder, ports, and private paths; they do not establish cloud acceptance.

The four main configurations were also run on Windows against the US New Relic endpoints using the documented `inject --json` form, current UTC timestamps and unique synthetic markers. Each reported one successful write, zero failures, and normal exit. The existing service and configuration were unchanged. The account owner retrieved all four stored records from New Relic; their unique markers, message bodies, fields and timestamps were compared with the injected fixtures:

| Configuration | Confirmed stored result                                                                                                                                                                                                                                      |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Raw JSON      | Full syslog line, `route=json`, `service=syslog-forwarder`, and `newrelic.source=api.logs`. The stored timestamp is ingestion time, not the date in the message.                                                                                             |
| Parsed JSON   | Full syslog line, dotted `source.ip`/`source.port` and `destination.ip`/`destination.port`, `rule.name=Example.Signature`, and `severity_number=19`. The event time remains a separate `event_time_unix_nano` field; the stored timestamp is ingestion time. |
| Raw OTLP      | Full syslog line, `route=otlp`, `service.name=syslog-forwarder`, and `newrelic.source=api.logs.otlp`. The stored timestamp was `08:07:42.258 UTC`; this configuration uses receipt time rather than parsing the date in the message.                         |
| Parsed OTLP   | CEF body without the syslog wrapper, source/destination fields, Fortinet observer attributes, `rule.name=Example.Signature`, and `severity.number=19`. The stored timestamp matches the parsed device time.                                                  |

For example, the parsed JSON record retained device time `08:07:31.000 UTC` in `event_time_unix_nano`, while New Relic stored `08:07:32.064 UTC` as its timestamp. Do not treat that custom field as an instruction to replace New Relic's timestamp. The optional NRQL query was not used for this comparison. These one-event checks do not establish production volume, long-term recovery, or coverage of every FortiGate category.

# Run the homepage example: FortiGate CEF to OCSF

The homepage shows a pipeline, not a complete configuration. Here are the input, output, snippets, and sample event needed to run that same chain locally. A FortiGate IPS event becomes an OCSF Detection Finding, with its attack name, severity, and network endpoints in structured fields.

## Use one release for the binary and snippets

Use limpid **0.8.4** and the snippet library from the [v0.8.4 source tree](https://github.com/naoto256/limpid/tree/v0.8.4). From the root of that source tree, save the following as `fortigate.conf`. The relative includes also load the snippets' own helper dependencies; keep the library directory structure intact.

```limpid
include "packaging/snippets/parsers/parse_syslog.limpid"
include "packaging/snippets/parsers/parse_cef.limpid"
include "packaging/snippets/parsers/parse_fortigate_cef.limpid"
include "packaging/snippets/composers/compose_ocsf.limpid"

def input fortigate_syslog {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output security_lake {
    type stdout
}

def pipeline fortigate_to_security_lake {
    input   fortigate_syslog
    process parse_syslog | parse_cef | parse_fortigate_cef | compose_ocsf | ocsf_to_egress
    output  security_lake
}
```

`security_lake` is just the output's name here: its type is `stdout`. This example neither connects to AWS Security Lake nor configures a cloud destination. It lets you inspect the composed bytes before choosing a delivery path.

## Supply a FortiGate IPS event

Save this test input as `event.json`. The addresses and device identifier are documentation placeholders. The syslog wrapper is RFC 3164; this FortiGate snippet uses its timestamp as an RFC 3164 value, so do not substitute an RFC 5424 wrapper without adapting that timestamp handling.

```json
{
  "ingress": "<134>Sep  6 10:00:00 fw01 CEF:0|Fortinet|Fortigate|v7.4.11|16384|utm:ips signature|7|deviceExternalId=FG-EXAMPLE cat=utm:ips FTNTFGTsubtype=ips FTNTFGTseverity=high src=192.0.2.10 spt=36208 dst=198.51.100.5 dpt=9100 proto=6 act=detected FTNTFGTattack=Example.Signature FTNTFGTattackid=12345 msg=Example attack detected",
  "source": { "ip": "192.0.2.1", "port": 514 }
}
```

```sh
limpid --check --config fortigate.conf
limpid --test-pipeline fortigate_to_security_lake --config fortigate.conf --input "$(cat event.json)"
```

Test mode traces processing without starting the UDP listener or sending to a destination. Each of the five named processes should report `ok`, followed by `security_lake` and its `egress` JSON.

## Read the result

The output contains these fields (an excerpt, not the entire record):

```json
{
  "class_uid": 2004,
  "category_uid": 2,
  "activity_id": 1,
  "type_uid": 200401,
  "severity_id": 4,
  "finding_info": {
    "title": "Example.Signature",
    "uid": "12345",
    "types": ["IDS/IPS"]
  },
  "src_endpoint": { "ip": "192.0.2.10", "port": 36208 },
  "dst_endpoint": { "ip": "198.51.100.5", "port": 9100 },
  "message": "Example attack detected"
}
```

The composer also supplies `time` and metadata identifying OCSF 1.3.0 and the Fortinet product. RFC 3164 omits the year and timezone; timestamp interpretation therefore depends on the parser's runtime context. Do not use a fixed millisecond timestamp from this example as an assertion for a different runtime date or timezone.

`parse_syslog` unwraps the transport, `parse_cef` reads the CEF header and extension, and `parse_fortigate_cef` places vendor-specific facts in `workspace.lsis.parsed`. `compose_ocsf` chooses the OCSF class from those facts, and `ocsf_to_egress` moves the composed JSON into the outgoing payload.

Change the fixture to representative logs before adapting the pipeline. Other FortiGate categories can map to different classes; this IPS example is not a promise that arbitrary vendor text becomes a valid finding. Choosing Loki, Elasticsearch, or another receiver requires that destination's own payload and transport configuration, not simply renaming this output.

# Map CEF fields to Log Analytics columns via AMP

A CEF event can reach the receiver and still leave the columns you need empty. Sending a message is not the same as making its fields available to the destination's mapping.

Azure Monitor pipeline (AMP) can receive OTLP and export records to Log Analytics. In this recipe, limpid parses CEF and puts selected fields into **OTLP log-record attributes** with the names the AMP exporter reads. AMP's `recordMap` then maps those attributes to CommonSecurityLog columns. The original CEF message remains available in the body.

## Make the field contract explicit

| CEF / syslog field | OTLP log-record attribute | AMP `recordMap.from`       | Log Analytics column |
| ------------------ | ------------------------- | -------------------------- | -------------------- |
| Syslog hostname    | `Computer`                | `attributes.Computer`      | `Computer`           |
| Device vendor      | `DeviceVendor`            | `attributes.DeviceVendor`  | `DeviceVendor`       |
| Event name         | `Activity`                | `attributes.Activity`      | `Activity`           |
| `src`              | `SourceIP`                | `attributes.SourceIP`      | `SourceIP`           |
| `dst`              | `DestinationIP`           | `attributes.DestinationIP` | `DestinationIP`      |

`attributes.SourceIP` does not read a JSON field called `SourceIP` inside the body, and it does not read a resource attribute. With this mapping, putting the value only in the body is insufficient. The name, location, and value type on the sending side must agree with the receiving side.

This is an AMP exporter mapping—not the facility filtering in the AMA recipe. The destination DCR and table still have to accept the exported columns; a `recordMap` does not create or configure either of them.

## Parse, project, and send

The packaged `parse_cef` and `cef_to_otlp` processes handle generic CEF. The small process between the adapter and `compose_otlp` adds the destination-specific column projection. Vendor-specific interpretation belongs in a vendor parser/adapter, not in the generic composer.

```limpid
include "/usr/share/limpid/snippets/parsers/parse_cef.limpid"
include "/usr/share/limpid/snippets/composers/compose_otlp.limpid"

def input cef_tcp {
    type syslog_tcp
    bind "0.0.0.0:5514"
}

def output amp {
    type otlp_grpc
    peer {
        endpoint "https://amp.example.com:4317"
        tls { ca "/etc/limpid/amp-ca.pem" }
    }
    queue {
        type disk
        path "/var/lib/limpid/queues/amp"
        max_size "1GB"
    }
}

def process project_common_security_log {
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.attributes = concat(
        workspace.lsis.shed.otlp.log_record.attributes,
        [
            { key: "Computer", value: { string_value: coalesce(workspace.syslog.hostname, "") } },
            { key: "DeviceVendor", value: { string_value: workspace.cef.device_vendor } },
            { key: "DeviceProduct", value: { string_value: workspace.cef.device_product } },
            { key: "DeviceVersion", value: { string_value: workspace.cef.device_version } },
            { key: "DeviceEventClassID", value: { string_value: workspace.cef.signature_id } },
            { key: "Activity", value: { string_value: workspace.cef.name } },
            { key: "LogSeverity", value: { string_value: "${workspace.cef.severity}" } },
            { key: "Message", value: { string_value: workspace.syslog.msg } },
            { key: "SourceIP", value: { string_value: coalesce(workspace.cef.extension.src, "") } },
            { key: "DestinationIP", value: { string_value: coalesce(workspace.cef.extension.dst, "") } }
        ]
    )
}

def pipeline cef_to_log_analytics {
    input cef_tcp
    process { workspace.syslog = syslog.parse(ingress) }
    if not starts_with(workspace.syslog.msg, "CEF:") { drop }
    process parse_cef | cef_to_otlp | project_common_security_log
          | compose_otlp | otlp_to_egress
    output amp
}
```

The endpoint and certificate path are placeholders. This listener expects syslog-wrapped CEF and deliberately drops non-CEF messages. Missing hostname or source/destination IP fields become empty strings. Reserve the port, restrict access to intended senders, and give the service access to the CA file and queue directory. Configure a writable `control.error_log` for durable failed-event recovery; without it, the checker warns that failed output payloads will not be persisted. A bounded disk queue helps buffer an outage; it does not prove that the destination accepted a record.

## Match the AMP exporter schema

The following is a **schema fragment**, not a complete AMP deployment template. It illustrates the corresponding `azureMonitorWorkspaceLogs.api.schema.recordMap`. Configure the receiver, pipeline, processors, authenticated destination, DCR, and full table schema separately.

```json
{
  "recordMap": [
    { "from": "time_unix_nano", "to": "TimeGenerated" },
    { "from": "attributes.Computer", "to": "Computer" },
    { "from": "attributes.DeviceVendor", "to": "DeviceVendor" },
    { "from": "attributes.DeviceProduct", "to": "DeviceProduct" },
    { "from": "attributes.DeviceVersion", "to": "DeviceVersion" },
    { "from": "attributes.DeviceEventClassID", "to": "DeviceEventClassID" },
    { "from": "attributes.Activity", "to": "Activity" },
    { "from": "attributes.LogSeverity", "to": "LogSeverity" },
    { "from": "attributes.Message", "to": "Message" },
    { "from": "attributes.SourceIP", "to": "SourceIP" },
    { "from": "attributes.DestinationIP", "to": "DestinationIP" }
  ]
}
```

Microsoft's [record-map troubleshooting guide](https://learn.microsoft.com/en-us/azure/azure-monitor/data-collection/pipeline-troubleshoot#check-the-record-map-schema-mapping) documents `body`, `time_unix_nano`, and `attributes.{fieldName}` as mapping sources, and requires a mapping for `TimeGenerated`. This example explicitly uses limpid's receipt time, not the device's event time. If device event time is required, use a source adapter that parses it and retain that timestamp instead.

For standard CommonSecurityLog ingestion, Microsoft's [AMP configuration guide](https://learn.microsoft.com/en-us/azure/azure-monitor/data-collection/pipeline-configure-cli) documents the `Microsoft-CommonSecurityLog-FullyFormed` stream and standard-table configuration. Its fully formed path can omit explicit record mapping when the required schema is already supplied. This recipe makes a selected field contract visible; it does not claim that every AMP configuration requires an explicit map or that these selected columns alone constitute a complete standard-table schema.

## Connect the receiving side

The complete path is **limpid → TLS gateway → AMP OTLP receiver → MicrosoftCommonSecurityLog → Batch → exporter → DCR → CommonSecurityLog**. An OTLP listener alone does not establish this path.

The examples below use invented resource names, `example.com`, and explicit `<placeholders>`. They are configuration fragments for your own deployment, not an export of a running environment. Start with an Arc-enabled Kubernetes cluster (k3s can host it), the AMP extension and custom location, a DCE, and a Log Analytics workspace with CommonSecurityLog available. Follow Microsoft's [resource setup procedure](https://learn.microsoft.com/en-us/azure/azure-monitor/data-collection/pipeline-configure-cli) for the resource envelopes, supported API version, region, and prerequisites.

### AMP: receive, normalize, and export

Use this `properties` fragment in the pipeline group resource. The named receiver, processors, and exporter are joined explicitly in `service.pipelines`.

```json
{
  "properties": {
    "tlsConfigurations": [{ "name": "receiver-tls", "mode": "serverOnly" }],
    "receivers": [
      {
        "name": "cef-otlp",
        "type": "OTLP",
        "otlp": { "endpoint": "0.0.0.0:4317" },
        "tlsConfiguration": "receiver-tls"
      }
    ],
    "processors": [
      { "name": "normalize-cef", "type": "MicrosoftCommonSecurityLog" },
      { "name": "batch-cef", "type": "Batch", "batch": { "timeout": 1000 } }
    ],
    "exporters": [
      {
        "name": "log-analytics-cef",
        "type": "AzureMonitorWorkspaceLogs",
        "azureMonitorWorkspaceLogs": {
          "api": {
            "dataCollectionEndpointUrl": "<DCE logs-ingestion URL>",
            "dataCollectionRule": "<DCR immutable ID>",
            "stream": "Microsoft-CommonSecurityLog-FullyFormed"
          }
        }
      }
    ],
    "service": {
      "pipelines": [
        {
          "name": "cef-to-log-analytics",
          "type": "Logs",
          "receivers": ["cef-otlp"],
          "processors": ["normalize-cef", "batch-cef"],
          "exporters": ["log-analytics-cef"]
        }
      ]
    }
  }
}
```

The standard processor handles attribute normalization, type conversion, and derived CommonSecurityLog fields before export. It is not a replacement for the sender's CEF parsing and projection: do not assume an OTLP body will be reparsed as raw syslog/CEF. Avoid emitting conflicting aliases for the same field, and check the final values after processing.

This fully formed example intentionally omits `api.schema`: Microsoft documents the built-in standard-table mapping for this stream. The earlier `recordMap` explains the selected field correspondence; it is not an additional processor and should not be blindly substituted as a complete table schema. If you maintain an explicit map, include every column your configuration needs and validate it against the processor output and DCR.

### Kubernetes: expose the receiver without breaking TLS

AMP's generated Service is `ClusterIP`. External clients need a gateway or another deliberately configured exposure mechanism. This Traefik example passes the original TLS connection through to the receiver:

```yaml
apiVersion: traefik.io/v1alpha1
kind: IngressRouteTCP
metadata:
  name: cef-otlp-route
  namespace: "<pipeline-namespace>"
spec:
  entryPoints:
    - otlp-cef
  routes:
    - match: HostSNI(`amp.example.com`)
      services:
        - name: "<actual-generated-AMP-service-name>"
          port: 4317
  tls:
    passthrough: true
```

The route assumes Traefik and its CRDs already exist, watch this namespace, and have an `otlp-cef` entrypoint listening on `:4317` with that TCP port exposed. It does not create the entrypoint, LoadBalancer, firewall rules, DNS, or certificates. Point `amp.example.com` at your chosen gateway address; discover the generated Service name rather than guessing it. If your Traefik installation uses label selectors, add its matching routing labels.

For this **server-only TLS** example, provision an AMP receiver certificate valid for `amp.example.com` and put its issuing CA chain in limpid's `amp-ca.pem`. The gateway neither terminates TLS nor supplies a client certificate. Restrict access to trusted senders: server-only TLS authenticates the server, not the client. For mTLS, configure receiver-side client trust and limpid's client certificate/key as well; changing only the mode is insufficient. Follow the [AMP TLS setup](https://learn.microsoft.com/en-us/azure/azure-monitor/data-collection/pipeline-tls) and certificate-management instructions for your chosen approach. Microsoft's [gateway guide](https://learn.microsoft.com/en-us/azure/azure-monitor/data-collection/pipeline-kubernetes-gateway) also covers a gateway that establishes its own backend TLS connection; do not mix that design with this passthrough route.

Configure AMP through its supported Azure resource interface. Do not edit the operator-generated collector ConfigMap as a second source of truth.

### DCR: connect the fully formed stream to the table

Use this DCR `properties` fragment with your own DCE and workspace resource IDs:

```json
{
  "properties": {
    "dataCollectionEndpointId": "<DCE Azure resource ID>",
    "destinations": {
      "logAnalytics": [
        {
          "name": "security-workspace",
          "workspaceResourceId": "<Log Analytics workspace Azure resource ID>"
        }
      ]
    },
    "dataFlows": [
      {
        "streams": ["Microsoft-CommonSecurityLog-FullyFormed"],
        "destinations": ["security-workspace"],
        "transformKql": "source",
        "outputStream": "Microsoft-CommonSecurityLog"
      }
    ]
  }
}
```

`source` preserves the incoming records; it does not parse CEF out of the body or repair missing attributes. The exporter stream and DCR input stream must match exactly. Use the DCR's **immutable ID** in the exporter, not its name or Azure resource ID, and use the DCE's **logs-ingestion URL**, not its resource ID, for `dataCollectionEndpointUrl`.

Grant the AMP extension's managed identity the **Monitoring Metrics Publisher** role at the DCR scope, as described in the setup guide. That authorization is separate from sender-to-receiver TLS. Confirm the destination table is available and that network policy permits AMP to reach the DCE. No tokens or credentials belong in these public examples.

## Verify the columns, not just delivery

Before sending test traffic, check that the collector Pod is Ready, its mounted generated configuration contains the intended OTLP-to-CEF processor chain, and the Service's target port reaches the receiver. Then confirm gateway routing, certificate hostname/trust, exporter stream, and DCR role assignment. A Ready Pod alone is not evidence of ingestion.

Use a representative synthetic event with known vendor, event name, and source/destination IP values. Decode the generated OTLP and check that those values are in **log-record attributes**, then inspect the matching Log Analytics row. Check `TimeGenerated`, the projected columns, duplicate ingestion, and AMP/DCR errors—not just a successful transport connection.

Extend the projection and map together for ports, actions, or vendor-specific extensions. Use the destination's required types, define what happens when a field is missing or malformed, and keep attribute names unique. This example demonstrates the mapping pattern; it is not a new end-to-end certification of your AMP deployment.

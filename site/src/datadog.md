# Send syslog directly to Datadog

Keep the original syslog line, add the service and source tags you want to search by, and send JSON directly to Datadog's HTTPS log intake. No Datadog Agent or Fluent Bit hop is needed for this route.

## Configure the Datadog destination

Use an API key from the Datadog organization that should receive the logs. The example uses the **AP1** intake endpoint; choose the endpoint for your organization's Datadog site, not the URL of the web application. See Datadog's [Send logs API](https://docs.datadoghq.com/api/latest/logs/#send-logs) for site-specific endpoints and payload limits.

This configuration requires **limpid 0.8.4 or later**. Use the static header object shown below; `DD_API_KEY` is not the same header as `DD-API-KEY`.

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

## Find the logs in Datadog

Open **Log Explorer** in the same Datadog site and organization, select a recent time range, and filter by `service:syslog-forwarder source:syslog`. Inspect `message` to see the original line. Intake acceptance and a searchable log are separate observations; an onboarding screen still waiting for logs does not by itself establish that delivery failed.

Check the organization's ingestion pipelines, exclusion filters, and index routing if accepted logs do not appear in the expected search. Request retries are not an exactly-once guarantee. This example does not establish outage recovery, throughput, or retention guarantees.

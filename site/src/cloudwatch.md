# Send syslog to Amazon CloudWatch Logs with JSON or OTLP

Send logs directly to CloudWatch Logs over HTTPS. Choose a JSON document when you want to define the stored fields, or OTLP when you want to preserve resource and log-record attributes.

|                             | Structured JSON           | OTLP/HTTP     |
| --------------------------- | ------------------------- | ------------- |
| limpid output               | `http`                    | `otlp_http`   |
| Endpoint path               | `/ingest/json`            | `/v1/logs`    |
| Payload                     | One JSON object per event | OTLP protobuf |
| Original line in CloudWatch | `message`                 | `body`        |

## Prepare CloudWatch

In the destination AWS account and region, create a log group such as `/applications/syslog` and a log stream such as `incoming`. Choose an appropriate retention period.

Enable **bearer token authentication** on that log group and generate a CloudWatch Logs service-specific API key. Its IAM identity needs `logs:PutLogEvents` and `logs:CallWithBearerToken` permissions for the destination. Restrict permissions to the intended logs, use an expiring key, and rotate it before expiry. See AWS's [bearer token setup](https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/CWL_HTTP_Endpoints_BearerTokenAuth.html).

This route uses a bearer token, not an AWS access-key ID and secret. AWS recommends SigV4 with short-lived credentials where the sender supports it; the configurations below use the bearer-token alternative supported by these HTTP endpoints.

Both examples use Tokyo (`ap-northeast-1`). Change the endpoint region, log group, and log stream together to match your destination. Replace `<CLOUDWATCH_LOGS_TOKEN>` only in a private configuration readable by the service account. Never commit the token or place it in a shared URL or diagnostic output. Keep TLS certificate verification enabled.

Use **limpid 0.8.4 or later** and the static header objects shown below.

## Option A: choose the JSON fields

```limpid
def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output cloudwatch {
    type http
    peer { url "https://logs.ap-northeast-1.amazonaws.com/ingest/json" }
    content_type "application/json"
    batch_size 1
    headers {
        "Authorization": "Bearer <CLOUDWATCH_LOGS_TOKEN>",
        "x-aws-log-group": "/applications/syslog",
        "x-aws-log-stream": "incoming"
    }
}

def process make_document {
    egress = to_json({
        message: ingress,
        service: "syslog-forwarder",
        route: "json"
    })
}

def pipeline syslog_to_cloudwatch {
    input syslog_local
    process make_document
    output cloudwatch
}
```

The `message` field contains the complete syslog line. `to_json` escapes quotes, backslashes, and non-ASCII text. Keep `batch_size 1`: this process creates one complete JSON object per event, not a batch array.

Without a numeric `timestamp` field, CloudWatch assigns server current time. If you add that field, it must be epoch **milliseconds**; do not put a nanosecond timestamp there. See the [Structured JSON endpoint contract](https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/CWL_HTTP_Endpoints_StructuredJSON.html).

## Option B: preserve OTLP structure

Use this instead of Option A for the same events. The installed composer builds the ResourceLogs payload; the output wraps it in an OTLP export request.

```limpid
include "/usr/share/limpid/snippets/composers/compose_otlp.limpid"

def input syslog_local {
    type syslog_udp
    bind "127.0.0.1:5514"
}

def output cloudwatch {
    type otlp_http
    peer { endpoint "https://logs.ap-northeast-1.amazonaws.com/v1/logs" }
    protocol http_protobuf
    batch_size 1
    headers {
        "Authorization": "Bearer <CLOUDWATCH_LOGS_TOKEN>",
        "x-aws-log-group": "/applications/syslog",
        "x-aws-log-stream": "incoming"
    }
}

def process syslog_for_cloudwatch {
    workspace.lsis.shed.otlp.resource.attributes = [
        { key: "service.name", value: { string_value: "syslog-forwarder" } }
    ]
    workspace.lsis.shed.otlp.log_record.time_unix_nano = received_at
    workspace.lsis.shed.otlp.log_record.body = { string_value: ingress }
    workspace.lsis.shed.otlp.log_record.attributes = [
        { key: "route", value: { string_value: "otlp" } }
    ]
}

def pipeline syslog_to_cloudwatch {
    input syslog_local
    process syslog_for_cloudwatch | compose_otlp | otlp_to_egress
    output cloudwatch
}
```

The timestamp here is limpid's receipt time, not parsed device event time. CloudWatch stores the line as `body`, `service.name` under `resource.attributes`, and `route` under log-record `attributes`. These are different locations from Option A's hand-built fields.

Supply the complete `/v1/logs` endpoint; limpid does not append it. CloudWatch requires the log group in an HTTP header for OTLP, not a query parameter. See the [OTLP endpoint contract](https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/CWL_HTTP_Endpoints_OTLP.html).

## Read the logs

In CloudWatch Logs, select the configured region, log group, and stream. For JSON, inspect `message`; for OTLP, inspect `body` and the resource/log attributes. Select a time range that includes arrival time and allow for ingestion delay.

HTTP success is not always acceptance of every record: these endpoints can report partial rejection. limpid's generic HTTP output does not interpret CloudWatch's JSON `partialSuccess` payload, and OTLP partial success is not selectively retried. Monitor rejected records and output errors rather than treating an HTTP status or a disk queue as an end-to-end delivery guarantee. Retries can also create duplicates.

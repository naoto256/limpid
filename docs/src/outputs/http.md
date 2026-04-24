# http

Sends events to an HTTP/HTTPS endpoint. Supports batching, gzip compression, custom headers, and TLS certificate configuration.

Works with Elasticsearch Bulk API, Splunk HEC, Datadog, Grafana Loki, and any generic HTTP endpoint.

## Configuration

```
def output elasticsearch {
    type http
    url "https://es:9200/_bulk"
    content_type "application/x-ndjson"
    batch_size 100
    batch_timeout "5s"
    compress gzip
    headers {
        Authorization "Basic dXNlcjpwYXNz"
    }
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `url` | yes | — | Target URL (http:// or https://) |
| `method` | no | `POST` | HTTP method (`POST` or `PUT`) |
| `content_type` | no | `application/json` | Content-Type header |
| `batch_size` | no | `1` | Events per HTTP request (1 = no batching) |
| `batch_timeout` | no | `5s` | Max time before flushing a partial batch |
| `compress` | no | none | `gzip` to compress request body |
| `verify` | no | `true` | `false` to skip TLS certificate validation |

### headers block

```
headers {
    Authorization "Bearer your-token"
    X-Custom-Header "value"
}
```

### tls block

```
tls {
    ca "/etc/limpid/certs/corp-ca.crt"
}
```

| Property | Description |
|----------|-------------|
| `ca` | Path to PEM-encoded CA certificate for private PKI |

## Status

> **Experimental**: This module has not been tested against live Elasticsearch/Splunk/Datadog endpoints. The core HTTP functionality works but edge cases in batching and error handling may exist. Please report any issues.

## Batching

When `batch_size > 1`, events are buffered and sent in a single HTTP request body (newline-delimited). The batch is flushed when:

- `batch_size` events have accumulated, or
- `batch_timeout` has elapsed since the last event (debounce timer)

On flush failure, events are returned to the buffer for retry by the queue.

## TLS behavior

| Setting | Effect |
|---------|--------|
| Default | Validate server cert against system CA store |
| `tls { ca "..." }` | Add custom CA for private PKI |
| `verify false` | Skip all certificate validation |

> **Warning**: `verify false` disables TLS certificate validation entirely — the
> connection is vulnerable to MITM. limpid emits a loud `WARN` log at startup
> when this is set. This setting is for debugging against self-signed test
> endpoints only; **never use it in production**. For private PKI, use
> `tls { ca "..." }` to trust an internal CA instead.

## Examples

### Splunk HEC

```
def output splunk {
    type http
    url "https://splunk:8088/services/collector/event"
    headers {
        Authorization "Splunk your-hec-token"
    }
}
```

### Datadog Logs

```
def output datadog {
    type http
    url "https://http-intake.logs.datadoghq.com/api/v2/logs"
    batch_size 50
    compress gzip
    headers {
        DD-API-KEY "your-api-key"
    }
}
```

### Grafana Loki

```
def output loki {
    type http
    url "http://loki:3100/loki/api/v1/push"
    content_type "application/json"
}
```

### Self-signed certificates (debugging only)

```
def output internal {
    type http
    url "https://es.internal:9200/_bulk"
    verify false
}
```

`verify false` disables certificate validation entirely. Prefer pointing to an
internal CA via `tls { ca "..." }` for private PKI — that keeps the connection
authenticated.

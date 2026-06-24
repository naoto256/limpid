# http

Sends events to one or more HTTP/HTTPS endpoints. Supports batching, gzip compression, custom headers, per-peer TLS / mTLS, and round-robin across multiple peers with per-peer cooldown on failure.

Works with Elasticsearch Bulk API, Splunk HEC, Datadog, Grafana Loki, and any generic HTTP endpoint.

## Configuration

```
def output es_cluster {
    type http
    peers {
        peer {
            url "https://es01.example.com:9200/_bulk"
            tls { ca "/etc/limpid/ca.crt" }
        }
        peer {
            url "https://es02.example.com:9200/_bulk"
            tls {
                ca   "/etc/limpid/ca.crt"
                cert "/etc/limpid/client.crt"   # mTLS
                key  "/etc/limpid/client.key"
            }
        }
    }
    content_type "application/x-ndjson"
    batch_size 100
    batch_timeout "5s"
    compress gzip
    headers {
        Authorization "Basic <base64(user:password)>"
    }
}
```

Single-peer setups use the `peer { ... }` shorthand (same shape `output syslog_tcp` / `output otlp_http` accept):

```
def output es {
    type http
    peer {
        url "https://es:9200/_bulk"
        tls { ca "/etc/limpid/ca.crt" }
    }
}
```

`peer { ... }` and `peers { peer { ... } ... }` are mutually exclusive — exactly one of the two must be present.

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `peer { url tls{...} }` or `peers { peer { ... } ... }` | yes (one of) | — | See [§ peers](#peers) below. |
| `method` | no | `POST` | HTTP method — any RFC-compliant verb (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, …). Invalid methods fail at config-load time. |
| `content_type` | no | `application/json` | Content-Type header |
| `batch_size` | no | `1` | Events per HTTP request (1 = no batching) |
| `batch_timeout` | no | `5s` | Max time before flushing a partial batch |
| `compress` | no | none | `gzip` to compress request body |
| `verify` | no | `true` | `false` to skip TLS certificate validation (applies to every peer) |
| `headers` | no | — | Extra HTTP request headers (see below) |

### peers

Each `peer` block configures one target endpoint:

| Per-peer property | Required | Description |
|-------------------|----------|-------------|
| `url` | yes | Target URL (`http://` or `https://`) |
| `tls.ca` | no | Custom CA certificate file (PEM) for this peer. Falls back to the system root store if omitted. |
| `tls.cert`, `tls.key` | no (paired) | Client certificate and private key for mTLS, as separate PEM files (chmod 600 the key). Both must be present together. |

On each send the rotation picks the next available peer (cooldown expired) and tries it. On failure that peer is marked cooled-down for ~5 s and subsequent sends rotate past it until the cooldown expires. When every peer is currently cooled the rotation falls back to the cursor start — the queue layer's per-event retry then handles re-delivery.

### headers block

```
headers {
    Authorization "Bearer your-token"
    X-Custom-Header "value"
}
```

## Status

> **Experimental**: This module has not been tested against live Elasticsearch/Splunk/Datadog endpoints. The core HTTP functionality works but edge cases in batching and error handling may exist. Please report any issues.

## Batching

When `batch_size > 1`, events are buffered and sent in a single HTTP request body (newline-delimited). The batch is flushed when:

- `batch_size` events have accumulated, or
- `batch_timeout` has elapsed since the last event (debounce timer)

On flush failure, events are returned to the buffer for retry by the queue.

### Shutdown

When the daemon stops, a final flush is attempted for any partial batch. If that flush fails unrecoverably, the buffered request body is drained to `control { error_log "..." }` (PR-P), one DLQ record per rendered body. Without `error_log` configured, behaviour matches 0.7.7 (warn + drop). Because the in-memory queue drops the source `Event` envelope as soon as `write()` returns `Ok`, the original metadata cannot be reconstructed at shutdown — DLQ records emitted on this path carry a synthetic source and the shutdown time as `received_at`.

See [Queue and retry → Recovery (error_log)](./README.md#recovery-error_log).

## TLS behavior

Per peer:

| Setting | Effect |
|---------|--------|
| `https://` URL, no `tls` block | Validate server cert against system CA store |
| `https://` URL, `tls { ca "..." }` | Add custom CA for private PKI |
| `https://` URL, `tls { ca cert key }` | mTLS — present `cert`/`key` as client identity |
| `verify false` (top-level) | Skip all certificate validation for every peer |

> **Warning**: `verify false` disables TLS certificate validation entirely — the
> connection is vulnerable to MITM. limpid emits a loud `WARN` log at startup
> when this is set. This setting is for debugging against self-signed test
> endpoints only; **never use it in production**. For private PKI, use
> per-peer `tls { ca "..." }` to trust an internal CA instead. For mTLS, set
> `cert` + `key` together.

`verify false` only relaxes **server-cert validation**. A `tls { cert key }`
pair on the same peer continues to be applied as the client identity — mTLS
remains intact even with `verify false`. `tls { ca "..." }` is ignored when
`verify false` is set (and a warning surfaces) because trusting a custom CA
is moot when no chain is checked at all.

## Examples

### Splunk HEC

```
def output splunk {
    type http
    peer { url "https://splunk:8088/services/collector/event" }
    headers {
        Authorization "Splunk your-hec-token"
    }
}
```

### Datadog Logs

```
def output datadog {
    type http
    peer { url "https://http-intake.logs.datadoghq.com/api/v2/logs" }
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
    peer { url "http://loki:3100/loki/api/v1/push" }
    content_type "application/json"
}
```

### Elasticsearch cluster with mTLS (round-robin)

```
def output es {
    type http
    peers {
        peer {
            url "https://es01.example.com:9200/_bulk"
            tls {
                ca   "/etc/limpid/ca.crt"
                cert "/etc/limpid/client.crt"
                key  "/etc/limpid/client.key"
            }
        }
        peer {
            url "https://es02.example.com:9200/_bulk"
            tls {
                ca   "/etc/limpid/ca.crt"
                cert "/etc/limpid/client.crt"
                key  "/etc/limpid/client.key"
            }
        }
    }
    content_type "application/x-ndjson"
    batch_size 200
    compress gzip
}
```

### Self-signed certificates (debugging only)

```
def output internal {
    type http
    peer { url "https://es.example.com:9200/_bulk" }
    verify false
}
```

`verify false` disables certificate validation entirely. Prefer pointing to an
internal CA via per-peer `tls { ca "..." }` for private PKI — that keeps the
connection authenticated.

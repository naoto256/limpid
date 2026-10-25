# http

Sends events to one or more HTTP/HTTPS endpoints. Supports batching, gzip compression, custom headers, per-peer TLS / mTLS, and round-robin across multiple peers with per-peer cooldown on failure.

Works with Elasticsearch Bulk API, Splunk HEC, Datadog, Grafana Loki, and any generic HTTP endpoint.

Redirects are not followed. All 3xx responses are failures and use the normal retry and DLQ path; errors include a sanitized `Location` destination when present (userinfo, query and fragment removed; capped at 200 characters plus a truncation marker). Configure the final endpoint directly.

## Configuration

```limpid
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
                cert "/etc/limpid/client.crt"   // mTLS
                key  "/etc/limpid/client.key"
            }
        }
    }
    content_type "application/x-ndjson"
    batch_size 100
    batch_timeout "5s"
    compress gzip
    headers {
        "Authorization": "Basic <base64(user:password)>"
    }
}
```

Single-peer setups use the `peer { ... }` shorthand (same shape `output syslog_tcp` / `output otlp_http` accept):

```limpid
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
| `url` | yes | Target URL (`http://` or `https://`). A `tls { ... }` sub-block requires the `https://` scheme; a `tls` block paired with an `http://` URL is rejected at config-load time (see [TLS behavior](#tls-behavior)). |
| `tls.ca` | no | Custom CA certificate file (PEM) for this peer. Falls back to the system root store if omitted. |
| `tls.cert`, `tls.key` | no (paired) | Client certificate and private key for mTLS, as separate PEM files (chmod 600 the key). Both must be present together. |

On each send the rotation picks the next available peer (cooldown expired) and tries it. On failure that peer is marked cooled-down for ~5 s and subsequent sends rotate past it until the cooldown expires. When every peer is currently cooled the rotation falls back to the cursor start — the output's per-flush retry loop then handles re-delivery without dropping the drained batch.

### headers block

Headers use a static string object, for example
`"DD-API-KEY": "your-api-key"`. Keys and values are literal strings;
interpolation, variables, function calls, and non-string values are rejected.
Literal escapes follow string-value decoding (`\"`,
`\\`, `\n`, `\t`, and `\$`; unknown escapes retain the backslash).
This object syntax is supported for string maps such as `headers`,
not fixed configuration names such as `type` or `peer`. HTTP header-name
restrictions still apply to the decoded name.

```limpid
headers {
    "Authorization": "Bearer your-token",
    "X-Custom-Header": "value"
}
```

## Status

> **Experimental**: This module has not been tested against live Elasticsearch/Splunk/Datadog endpoints. The core HTTP functionality works but edge cases in batching and error handling may exist. Please report any issues.

## Batching

Events are combined into one newline-delimited HTTP request body. Reaching
`batch_size` wakes the flusher immediately; it sends after any current send
or retry backoff completes. Otherwise, the flusher sends a partial batch when
its current `batch_timeout` wait ends. That wait starts at the beginning of
each flusher loop; arrivals during the wait do not restart it.

A failed send retries the prepared batch within the configured retry budget.
See [Queue and retry](./README.md#queue-and-retry) for the policy and the risk
of duplicate delivery when replaying an error-log record.

### Shutdown

A final bounded drain runs on graceful shutdown; see the common
[disposition contract](./README.md#disposition-contract) and
[shutdown guarantees and recovery limits](../operations/error-log.md#graceful-shutdown-guarantees-and-known-limitations).

## TLS behavior

Per peer:

| Setting | Effect |
|---------|--------|
| `https://` URL, no `tls` block | Validate server cert against system CA store |
| `https://` URL, `tls { ca "..." }` | Add custom CA for private PKI |
| `https://` URL, `tls { ca cert key }` | mTLS — present `cert`/`key` as client identity |
| `http://` URL, `tls { ... }` block | **Rejected at load time.** reqwest only engages TLS for `https://`, so the tls settings would be silently dropped and the daemon would ship plaintext. Switch the url to `https://` or drop the tls block. |
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

```limpid
def output splunk {
    type http
    peer { url "https://splunk:8088/services/collector/event" }
    headers {
        "Authorization": "Splunk your-hec-token"
    }
}
```

### Datadog Logs

```limpid
def output datadog {
    type http
    peer { url "https://http-intake.logs.datadoghq.com/api/v2/logs" }
    batch_size 50
    compress gzip
    headers {
        "DD-API-KEY": "your-api-key"
    }
}
```

### Grafana Loki

```limpid
def output loki {
    type http
    peer { url "http://loki:3100/loki/api/v1/push" }
    content_type "application/json"
}
```

### Elasticsearch cluster with mTLS (round-robin)

```limpid
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

```limpid
def output internal {
    type http
    peer { url "https://es.example.com:9200/_bulk" }
    verify false
}
```

`verify false` disables certificate validation entirely. Prefer pointing to an
internal CA via per-peer `tls { ca "..." }` for private PKI — that keeps the
connection authenticated.

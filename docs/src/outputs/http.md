# http

Sends events to one or more HTTP/HTTPS endpoints. Supports batching, gzip compression, custom headers, per-peer TLS / mTLS, and round-robin across multiple peers with per-peer cooldown on failure.

Works with Elasticsearch Bulk API, Splunk HEC, Datadog, Grafana Loki, and any generic HTTP endpoint.

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

On each send the rotation picks the next available peer (cooldown expired) and tries it. On failure that peer is marked cooled-down for ~5 s and subsequent sends rotate past it until the cooldown expires. When every peer is currently cooled the rotation falls back to the cursor start — the output's per-flush retry loop (driven inside the flush actor's `flush_events`) then handles re-delivery without dropping the drained batch.

### headers block

From 0.8.4, headers use a static string object, for example
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

When `batch_size > 1`, events are buffered and sent in a single HTTP request body (newline-delimited). The batch is flushed when:

- `batch_size` events have accumulated, or
- `batch_timeout` has elapsed since the last event (debounce timer)

On flush failure the per-flush retry loop inside `flush_events` retries the same batch in-place with exponential backoff between attempts; the events are *not* returned to the buffer for the next `batch_timeout` tick. When the retry budget is exhausted, the batched events are routed to `control { error_log }` as Output-flavor DLQ records with a `Recovered` marker (the failure was proved *before* any request bytes crossed the wire boundary, so replay is safe). When shutdown cancels an in-flight `flush_events` mid-send, the wire state is ambiguous — the batch may have partially reached the peer — so the events route through the ambiguous DLQ path instead, which forces `Dropped` on a disk queue (the fail-stop wedge holds the cursor for next-start reconciliation against the DLQ record) and folds to `Recovered` on a memory queue (no replay path exists). `consume()` itself only pushes the new event into the actor's buffer and signals the flush actor — it never awaits transport, so steady-state ingress is not coupled to peer latency. The queue layer's cursor advances when each event's ack handle resolves, which happens after the flush actor finishes its retry cycle for that batch (success, `Recovered` DLQ, or `Dropped`-with-wedge on disk).

### Shutdown

On **graceful shutdown** (`SIGTERM`, `SIGHUP` reload, `systemctl stop`, or an explicit `shutdown()` API call), a bounded final drain runs — one flush attempt per parked payload bounded by `SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT` (3 s) plus the per-actor in-flight cancel. The disposition of each drained event depends on where its failure fired: a per-event render failure (proved pre-boundary) or a retry-exhausted terminal drop routes as `Recovered`, while an in-flight cancel or the 3-second attempt timeout leaves the wire state ambiguous and routes through the ambiguous DLQ path — `Dropped` on a disk queue (fail-stop wedge holds the cursor for next-start reconciliation against the DLQ record) and folded to `Recovered` on a memory queue (no replay path exists). In every case `event.source`, `event.received_at`, `event.ingress`, and `event.egress` reflect the original per-event provenance because the batched output parks the source `Event` alongside its `QueueAckHandle` until the flush resolves. Without `error_log` configured, the daemon emits a one-line `tracing::error!` summary per drain-failure event (site + reason, no payload) — the payload is not persisted anywhere by default. To attach metadata or the full JSONL to the tracing line, set `control { error_log_fallback "meta" | "full" }` alongside `error_log` (see the [tracing fallback ladder](../operations/error-log.md#tracing-fallback-ladder-error_log_fallback)). **`SIGKILL` (`kill -9`) does not run this path** — actor tasks are aborted and the stack-local buffer is lost. Production deployments should not send `SIGKILL` directly to the daemon (systemd's default `KillSignal=SIGTERM` is the contract).

- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).
- Recovery / DLQ behaviour for shutdown-flush leftovers — see [Queue and retry → Recovery (error_log)](./README.md#recovery-error_log).

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

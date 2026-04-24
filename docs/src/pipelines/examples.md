# Pipeline Examples

Real-world configuration examples from production deployments.

## Firewall log archival with source-based routing

Receives syslog from multiple firewall vendors, strips PRI, and routes to per-device log files.

```
def input syslog_udp {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output fw01 { type file  path "/var/log/fw/fw01.log" }
def output fw02 { type file  path "/var/log/fw/fw02.log" }
def output fw03 { type file  path "/var/log/fw/fw03.log" }

def process filter_noise {
    if source == "192.0.2.2" and contains(ingress, "CHARGEN") {
        drop
    }
}

def pipeline archive {
    input syslog_udp
    process strip_pri | filter_noise

    switch source {
        "192.0.2.1" {
            output fw01
        }
        "192.0.2.2" {
            output fw02
        }
        "192.0.2.3" {
            if contains(ingress, "type=\"traffic\"") {
                drop
            }
            process prepend_source | prepend_timestamp
            output fw03
        }
        default {
            drop
        }
    }
}
```

## Azure Monitor Agent (AMA) forwarding

Receives CEF logs over TCP, filters Fortinet traffic, rewrites PRI for Azure Log Analytics facility routing, and forwards via disk-backed queue.

```
def input ama_tcp {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output ama {
    type tcp
    address "127.0.0.1:28330"
    framing non_transparent
    queue {
        type disk
        path "/var/lib/limpid/queues/ama"
        max_size "1GB"
    }
}

def process filter_fortinet_traffic {
    if contains(ingress, "Fortinet") and contains(ingress, "cat=traffic:") {
        drop
    }
}

def process ama_rewrite {
    if contains(ingress, "CEF:") {
        facility = 16
    } else {
        facility = 17
    }
    severity = 6
}

def pipeline ama_forward {
    input ama_tcp
    process filter_chargen | filter_fortinet_traffic | ama_rewrite
    output ama
}
```

## SIEM ingest with enrichment

Parses CEF, enriches with GeoIP, serializes to JSON, and sends to Elasticsearch with batching.

```
def input fw {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output archive {
    type file
    path "/var/log/limpid/${source}/${strftime(timestamp, "%Y-%m-%d", "local")}.log"
}

def output elasticsearch {
    type http
    url "https://es:9200/firewall-logs/_bulk"
    content_type "application/x-ndjson"
    batch_size 100
    batch_timeout "5s"
    headers {
        Authorization "Basic dXNlcjpwYXNz"
    }
}

def pipeline siem {
    input fw

    // Archive raw log first
    output archive

    // Parse and enrich
    process parse_cef | {
        if workspace.src != null {
            workspace.geo = geoip(workspace.src)
        }
        egress = to_json()
    }

    output elasticsearch
}
```

## FortiGate KV parsing with format

```
def process enrich_fortigate {
    process parse_kv

    if workspace.srcip != null {
        workspace.geo = geoip(workspace.srcip)
    }

    egress = format("%{workspace.devname} %{workspace.srcip} -> %{workspace.dstip} %{workspace.action}")
}
```

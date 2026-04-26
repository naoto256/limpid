# Pipeline Examples

Real-world configuration examples from production deployments.

## Firewall log archival with source-based routing

Receives syslog from multiple firewall vendors, strips PRI, and routes to per-device log files.

```
def input syslog_udp {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output fw01 { type file; path "/var/log/fw/fw01.log" }
def output fw02 { type file; path "/var/log/fw/fw02.log" }
def output fw03 { type file; path "/var/log/fw/fw03.log" }

def process strip_headers {
    egress = syslog.strip_pri(egress)
}

def process filter_noise {
    if source == "192.0.2.2" and contains(ingress, "CHARGEN") {
        drop
    }
}

def pipeline archive {
    input syslog_udp
    process strip_headers | filter_noise

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
            process { egress = source + " " + strftime(received_at, "%b %e %H:%M:%S") + " " + egress }
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
        egress = syslog.set_pri(egress, 16, 6)   // local0.info for CEF → CommonSecurityLog
    } else {
        egress = syslog.set_pri(egress, 17, 6)   // local1.info for everything else → Syslog
    }
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
    path "/var/log/limpid/${source}/${strftime(received_at, "%Y-%m-%d", "local")}.log"
}

def output elasticsearch {
    type http
    url "https://es:9200/firewall-logs/_bulk"
    content_type "application/x-ndjson"
    batch_size 100
    batch_timeout "5s"
    headers {
        Authorization "Basic <base64(user:password)>"
    }
}

def pipeline siem {
    input fw

    // Archive raw log first
    output archive

    // Parse and enrich
    process {
        workspace.cef = cef.parse(ingress)
        if workspace.cef.src != null {
            workspace.geo = geoip(workspace.cef.src)
        }
        egress = to_json(workspace)
    }

    output elasticsearch
}
```


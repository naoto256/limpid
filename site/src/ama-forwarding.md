# Route CEF and Syslog to Log Analytics via AMA

Receive mixed syslog traffic on one TCP listener, assign separate facilities to CEF and non-CEF messages, and forward both to Azure Monitor Agent (AMA). The facility split gives the two data collection rules (DCRs) distinct filters: CEF goes to CommonSecurityLog and other syslog messages go to Syslog.

## Configure the two DCRs

AMA collection rules select syslog facilities and severity levels, both encoded in PRI. Create a DCR through each Microsoft Sentinel connector and associate both with the machine running AMA:

| Connector / DCR                   | Facility to collect | Minimum severity | Destination table |
| --------------------------------- | ------------------- | ---------------- | ----------------- |
| Common Event Format (CEF) via AMA | local0 only         | LOG_INFO         | CommonSecurityLog |
| Syslog via AMA                    | local1 only         | LOG_INFO         | Syslog            |

The pipeline below assigns local0.info to messages containing `CEF:` and local1.info to everything else. Rewriting PRI alone does not select a table: the connector and matching DCR complete the routing. Do not also collect local0 in the Syslog DCR, or another overlapping rule, if you want to avoid duplicate collection.

Microsoft documents the [facility separation and duplication risk](https://learn.microsoft.com/en-us/azure/sentinel/cef-syslog-ama-overview#data-ingestion-duplication-avoidance) and the [connector and DCR setup procedure](https://learn.microsoft.com/en-us/azure/sentinel/connect-cef-syslog-ama). In the portal, selecting LOG_INFO also includes higher-severity messages; this example deliberately rewrites every message to info.

## Forward through a disk-backed queue

```limpid
def input tcp_514 {
    type syslog_tcp
    bind "0.0.0.0:514"
}

def output ama {
    type syslog_tcp
    peer { host "127.0.0.1" port 28330 }
    framing non_transparent
    queue {
        type disk
        path "/var/lib/limpid/queues/ama"
        max_size "1GB"
    }
}

def process ama_rewrite {
    if contains(ingress, "CEF:") {
        // local0.info for CEF → CommonSecurityLog
        egress = syslog.set_pri(egress, 16, 6)
    } else {
        // local1.info for everything else → Syslog
        egress = syslog.set_pri(egress, 17, 6)
    }
}

def pipeline ama_forward {
    input tcp_514
    process ama_rewrite
    output ama
}
```

## Before using this configuration

- AMA must already be installed and listening locally on TCP 28330. Microsoft documents this listener for AMA 1.28.11 and later; verify the actual agent configuration.
- Reserve TCP 514 for limpid, grant the service permission to bind it, and restrict network access to intended senders. Do not leave another syslog daemon bound to the same address and port.
- Give the service write access to the queue directory and provision disk space. The 1 GB queue is bounded; it is not an unlimited outage buffer or proof of downstream ingestion.
- `contains(ingress, "CEF:")` is a classification convention, not a CEF validator. Confirm that your sources use it reliably. Non-CEF data is not converted into CEF, and the original facility and severity are replaced.
- Validate the configuration and test representative CEF and non-CEF events in your own environment. Confirm their arrival in CommonSecurityLog and Syslog respectively and check for duplication. This published example is not a new end-to-end integration certification.

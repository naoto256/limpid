# Archive firewall logs in per-device files

Several firewalls send syslog to the same listener. Keep each device's logs in its own file, with a fallback archive for other senders.

This example uses the observed sender IP address to choose the destination. It removes the syslog PRI prefix, but does not parse or normalize the vendor's message body.

## Choose an archive

| Sender           | Archive               |
| ---------------- | --------------------- |
| 192.0.2.1        | /var/log/fw/fw01.log  |
| 192.0.2.2        | /var/log/fw/fw02.log  |
| 192.0.2.3        | /var/log/fw/fw03.log  |
| Any other sender | /var/log/fw/other.log |

## Write the pipeline

`strip_headers` removes only the PRI prefix, not the timestamp or the rest of the header. The switch then selects a file; every branch has an output.

```limpid
def input syslog_udp {
    type syslog_udp
    bind "0.0.0.0:514"
}

def output fw01 { type file; path "/var/log/fw/fw01.log" }
def output fw02 { type file; path "/var/log/fw/fw02.log" }
def output fw03 { type file; path "/var/log/fw/fw03.log" }
def output other { type file; path "/var/log/fw/other.log" }

def process strip_headers {
    egress = syslog.strip_pri(egress)
}

def pipeline archive {
    input syslog_udp
    process strip_headers
    switch source.ip {
        "192.0.2.1" { output fw01 }
        "192.0.2.2" { output fw02 }
        "192.0.2.3" { output fw03 }
        default { output other }
    }
}
```

## Before using this configuration

- UDP does not guarantee delivery or provide backpressure. This is not a lossless collection guarantee.
- `source.ip` is the observed sender address. A relay or NAT can change it; it is not an authenticated device identity.
- Reserve UDP 514 for limpid, restrict incoming traffic to intended senders, and grant the service the necessary bind and file permissions. Create the archive directory and plan rotation and disk capacity.
- Replace the documentation-only addresses with your own routes. Filtering and repeated-message suppression are covered in the next recipe.

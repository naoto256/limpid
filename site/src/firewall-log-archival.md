# Keep the firewall logs you need, in per-device files

Several firewalls send syslog to the same listener, but they do not need the same retention rules. Keep each device's logs in its own file, discard selected noise, and add source context where it is useful—all in one pipeline.

This example uses the sender IP address to choose the destination. It does not parse every vendor's log format or normalize the records into a common schema.

## Decide what to keep

| Sender           | Keep / discard rule                                                                           | Archive              |
| ---------------- | --------------------------------------------------------------------------------------------- | -------------------- |
| 192.0.2.1        | Keep all received messages                                                                    | /var/log/fw/fw01.log |
| 192.0.2.2        | Discard messages containing `CHARGEN`; keep the rest                                          | /var/log/fw/fw02.log |
| 192.0.2.3        | Discard messages containing `type="traffic"`; prefix the rest with sender IP and receipt time | /var/log/fw/fw03.log |
| Any other sender | Discard                                                                                       | None                 |

The explicit default drop makes this an allowlist: adding a firewall requires adding its route. Replace these documentation-only IP addresses and the example filtering rules with your own policy.

## Write the pipeline

First, `filter_noise` rejects the selected CHARGEN messages. Then, `strip_headers` removes only the syslog PRI prefix from the remaining messages; despite its name, it does not remove the timestamp or the rest of the header. Finally, the switch applies the per-device rules and selects a file.

<!-- archival-configuration -->

## Before using this configuration

- UDP does not guarantee delivery or provide backpressure. This is a selective file archive, not a lossless collection guarantee.
- `source.ip` is the observed sender address. A relay or NAT can change it; it is not an authenticated device identity.
- The substring filters are deliberately simple and case-sensitive. Test representative messages before adopting them as retention policy.
- Reserve UDP 514 for limpid, grant the service permission to bind it, and restrict incoming traffic to intended senders. Create the archive directory with appropriate write permissions and plan file rotation and disk capacity.
- Test all three sender branches and the default drop. The code adapts the existing documented example to filter before stripping PRI; this page does not claim a newly verified deployment.

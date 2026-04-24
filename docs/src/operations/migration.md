# Migrating from rsyslog

A guide for replacing rsyslog with limpid.

## Before you start

1. Document your current rsyslog configuration: `rsyslogd -N 1` validates, `grep -r '' /etc/rsyslog.d/` shows all config.
2. Identify: what inputs (ports, protocols), what outputs (files, remote servers), what filters.
3. limpid and rsyslog cannot share the same ports simultaneously.

## Step 1: Install limpid

```bash
sudo dpkg -i limpid_*.deb
# limpid is installed but not started yet
```

## Step 2: Translate your configuration

### Inputs

| rsyslog | limpid |
|---------|--------|
| `input(type="imudp" port="514")` | `def input fw { type syslog_udp bind "0.0.0.0:514" }` |
| `input(type="imtcp" port="514")` | `def input fw { type syslog_tcp bind "0.0.0.0:514" }` |
| `input(type="imuxsock")` | `def input local { type unix_socket path "/dev/log" }` |
| `input(type="imfile" File="/var/log/app.log")` | `def input app { type tail path "/var/log/app.log" }` |

### Outputs

| rsyslog | limpid |
|---------|--------|
| `action(type="omfile" file="/var/log/msg.log")` | `def output msg { type file path "/var/log/msg.log" }` |
| `action(type="omfwd" target="10.0.0.1" port="514" protocol="tcp")` | `def output remote { type tcp address "10.0.0.1:514" }` |
| `action(type="omfwd" target="10.0.0.1" port="514" protocol="udp")` | `def output remote { type udp address "10.0.0.1:514" }` |
| `action(type="omelasticsearch" server="es:9200")` | `def output es { type http url "https://es:9200/_bulk" }` |

### Filters

| rsyslog | limpid |
|---------|--------|
| `if $msg contains 'error' then ...` | `if contains(egress, "error") { ... }` |
| `if $syslogfacility-text == 'local0' then ...` | `let pri = syslog.extract_pri(ingress)`<br>`if pri != null and pri / 8 == 16 { ... }` |
| `if $fromhost-ip == '10.0.0.1' then ...` | `if source == "10.0.0.1" { ... }` |
| `:msg, contains, "DISCARD" stop` | `if contains(ingress, "DISCARD") { drop }` |

### Templates

| rsyslog | limpid |
|---------|--------|
| `template(name="t" type="string" string="%HOSTNAME% %msg%")` | `egress = format("%{workspace.syslog_hostname} %{egress}")` |

## Step 3: Stop rsyslog, start limpid

```bash
# Stop rsyslog (limpid's Conflicts= will do this automatically, but being explicit is safer)
sudo systemctl stop rsyslog
sudo systemctl stop syslog.socket
sudo systemctl disable rsyslog

# Start limpid
sudo systemctl start limpid
sudo systemctl status limpid

# Verify
sudo limpidctl stats
```

## Step 4: Verify

```bash
# Send a test message
logger "migration test from limpid"

# Check output
tail /var/log/limpid/syslog.log
sudo limpidctl stats
```

## Rollback

If something goes wrong:

```bash
sudo systemctl stop limpid
sudo systemctl start rsyslog
```

rsyslog's configuration is untouched — you can always go back.

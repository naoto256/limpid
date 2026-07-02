# systemd

## Service unit

limpid ships with a systemd unit file:

```ini
[Unit]
Description=limpid log pipeline daemon
Documentation=https://github.com/naoto256/limpid
After=network.target
Conflicts=rsyslog.service syslog-ng.service syslog.socket

[Service]
Type=simple
ExecStart=/usr/bin/limpid --config /etc/limpid/limpid.conf
ExecReload=/bin/kill -HUP $MAINPID
TimeoutStopSec=15
Restart=on-failure
RestartSec=5

User=syslog
Group=syslog
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

RuntimeDirectory=limpid
StateDirectory=limpid
ConfigurationDirectory=limpid

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallFilter=@system-service
UMask=0027
ReadWritePaths=/var/lib/limpid /var/run/limpid /var/log/limpid

[Install]
WantedBy=multi-user.target
```

The full unit (`packaging/limpid.service`) carries a comment above each
directive explaining why it's safe for limpid's inputs/outputs; this
page shows the trimmed shape. Notably `PrivateDevices=yes` gives the
unit a private `/dev` without the host's `/dev/log` — see [Adding write
paths](#adding-write-paths) below for the `/dev/log` and extra
`ReadWritePaths` drop-ins.

## Key features

### Conflicts

`Conflicts=rsyslog.service syslog-ng.service syslog.socket` ensures that starting limpid automatically stops conflicting syslog daemons. No manual `systemctl stop rsyslog` needed.

### Non-root operation

limpid runs as the `syslog` user with `CAP_NET_BIND_SERVICE` for binding to privileged ports (514). No root required at runtime.

### Hot reload

```bash
sudo systemctl reload limpid    # sends SIGHUP
```

limpid validates the new configuration before applying it. If validation fails, the old configuration is kept. If the new runtime fails to start, it rolls back to the previous configuration automatically.

### Graceful shutdown

```bash
sudo systemctl stop limpid
```

Stops within seconds:
1. Input listeners stop accepting
2. Pipeline workers drain remaining events
3. Output queues flush
4. All tasks terminate

`TimeoutStopSec=15` provides a safety net. If shutdown takes longer than 15 seconds, systemd forcibly kills the process.

## Common operations

```bash
# Start
sudo systemctl start limpid

# Stop
sudo systemctl stop limpid

# Restart (brief downtime)
sudo systemctl restart limpid

# Reload configuration (brief downtime for new connections;
# existing connections drain via the old runtime, disk queues persist)
sudo systemctl reload limpid

# Status
sudo systemctl status limpid

# Logs
sudo journalctl -u limpid -f

# Enable on boot
sudo systemctl enable limpid
```

## limpid-prometheus

```bash
# Start the Prometheus exporter
sudo systemctl start limpid-prometheus

# Logs
sudo journalctl -u limpid-prometheus -f
```

The unit depends on `limpid.service` — starting it will also start limpid if not already running.

Settings are in `/etc/default/limpid-prometheus`:

```bash
LIMPID_PROMETHEUS_BIND=127.0.0.1:9100
LIMPID_PROMETHEUS_SOCKET=/var/run/limpid/control.sock
```

Edit and restart to apply changes:

```bash
sudo systemctl restart limpid-prometheus
```

## Adding write paths

If your file outputs write to directories outside the defaults, add them to `ReadWritePaths` via a drop-in rather than editing the shipped unit:

```ini
# /etc/systemd/system/limpid.service.d/override.conf
[Service]
ReadWritePaths=/var/log/limpid /path/to/other/output/dir
```

If you configure a `unix_socket` input at `/dev/log` as a syslog(3) replacement, `PrivateDevices=yes` keeps it out of reach unless you also bind it in:

```ini
# /etc/systemd/system/limpid.service.d/override.conf
[Service]
BindPaths=/dev/log
```

Then reload systemd:

```bash
sudo systemctl daemon-reload
sudo systemctl restart limpid
```

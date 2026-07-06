# Packaging

limpid uses [cargo-deb](https://github.com/kornelski/cargo-deb) to build `.deb` packages. Each crate produces its own package.

## Building packages

```bash
cargo install cargo-deb

# Main daemon (includes limpidctl)
cargo deb -p limpid

# Prometheus exporter
cargo deb -p limpid-prometheus
```

Packages are written to `target/debian/`.

### Building with optional features

```bash
# With systemd journal support
cargo deb -p limpid -- --features journal

# With Kafka output
cargo deb -p limpid -- --features kafka

# Both
cargo deb -p limpid -- --features journal,kafka
```

## Package contents

### limpid

| Path | Description |
|------|-------------|
| `/usr/bin/limpid` | Daemon binary |
| `/usr/bin/limpidctl` | Control and debug CLI |
| `/usr/share/limpid/limpid.conf.example` | Example configuration |
| `/usr/share/doc/limpid/README.md` | Documentation |
| `/usr/share/limpid/snippets/` | Shipped snippet library (`composers/`, `filters/`, `functions/`, `parsers/`) — resolved by absolute-`include` under `SYSTEM_SNIPPET_DIR`. See [Snippet Library](../snippets/README.md). |
| `/etc/systemd/system/limpid.service` | systemd unit file |

The post-install script (`packaging/postinst`) runs on first install:

1. Creates `syslog` user and group (if not present)
2. Creates directory structure:
   - `/etc/limpid/{inputs,outputs,processes,pipelines}/`
   - `/var/lib/limpid/` (state: disk queues, cursor files)
   - `/var/log/limpid/` (file output default location)
3. Copies example config to `/etc/limpid/limpid.conf` (only if no config exists)
4. Warns if rsyslog, syslog-ng, td-agent, or fluentd is running
5. Enables the systemd unit (but does not start it)

### limpid-prometheus

| Path | Description |
|------|-------------|
| `/usr/bin/limpid-prometheus` | Prometheus exporter |
| `/etc/default/limpid-prometheus` | Environment variables (`LIMPID_PROMETHEUS_BIND`, `LIMPID_PROMETHEUS_SOCKET`) |
| `/etc/systemd/system/limpid-prometheus.service` | systemd unit file |

## systemd unit

The included unit file (`packaging/limpid.service`) runs limpid as the `syslog` user with security hardening:

```ini
[Service]
User=syslog
Group=syslog
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
ReadWritePaths=/var/log/limpid
```

- `CAP_NET_BIND_SERVICE` allows binding to privileged ports (514) without root
- `ProtectSystem=strict` makes the filesystem read-only except for explicitly allowed paths
- `RuntimeDirectory=limpid` ensures `/var/run/limpid/` exists for the control socket. The packaged unit explicitly sets `RuntimeDirectoryMode=0750` — this is **not** systemd's default (systemd.exec picks `0755`), so operators who take a drop-in path must keep the `0750` value. Daemon startup refuses to run when the control socket's parent is group-writable, world-writable, or world-traversable, so a laxer mode fails-closed and breaks the unit
- `ExecReload=/bin/kill -HUP $MAINPID` triggers hot reload via SIGHUP
- `StateDirectory=limpid` and `RuntimeDirectory=limpid` provide `/var/lib/limpid` and `/var/run/limpid` writable; `ReadWritePaths=/var/log/limpid` covers the log directory. Operators using the `file` output to write elsewhere add a drop-in with the extra path.
- `PrivateDevices=yes` means a `/dev/log` `unix_socket` input needs a `PrivateDevices=no` drop-in (see [systemd](./systemd.md#adding-write-paths))

See [systemd](./systemd.md) for operational details.

### limpid-prometheus

The unit (`packaging/limpid-prometheus.service`) depends on `limpid.service` and reads settings from `/etc/default/limpid-prometheus`:

```ini
[Service]
EnvironmentFile=/etc/default/limpid-prometheus
ExecStart=/usr/bin/limpid-prometheus --bind ${LIMPID_PROMETHEUS_BIND} --socket ${LIMPID_PROMETHEUS_SOCKET}
```

To change the bind address or socket path, edit `/etc/default/limpid-prometheus` and restart:

```bash
sudo systemctl restart limpid-prometheus
```

## Directory layout after installation

```
/etc/limpid/
├── limpid.conf
├── inputs/
├── outputs/
├── processes/
└── pipelines/

/usr/share/limpid/snippets/  # Shipped snippet library (composers/, filters/, functions/, parsers/)
/var/lib/limpid/             # Disk queue data
/var/log/limpid/             # Default file output location
/var/run/limpid/
└── control.sock             # Control socket (created at runtime)
```

## Upgrading

```bash
# Build new package
cargo deb -p limpid

# Install over existing
sudo dpkg -i target/debian/limpid_*.deb

# Reload (brief downtime for new connections; established
# connections drain via the old runtime, disk queues persist)
sudo systemctl reload limpid
```

`systemctl reload` sends SIGHUP. limpid validates the new config first and keeps the existing runtime untouched on failure. On a successful validate, the old runtime is shut down and the new one rebinds — expect brief downtime for new connections while the old runtime drains established TCP/HTTP/gRPC connections. Persistent disk queue data survives the cycle; memory queues and in-flight events are best-effort drained.

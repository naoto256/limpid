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
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/limpid /var/run/limpid /var/log
```

- `CAP_NET_BIND_SERVICE` allows binding to privileged ports (514) without root
- `ProtectSystem=strict` makes the filesystem read-only except for explicitly allowed paths
- `RuntimeDirectory=limpid` ensures `/var/run/limpid/` exists for the control socket
- `ExecReload=/bin/kill -HUP $MAINPID` triggers hot reload via SIGHUP

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

/var/lib/limpid/          # Disk queue data
/var/log/limpid/          # Default file output location
/var/run/limpid/
└── control.sock          # Control socket (created at runtime)
```

## Upgrading

```bash
# Build new package
cargo deb -p limpid

# Install over existing
sudo dpkg -i target/debian/limpid_*.deb

# Reload (no downtime)
sudo systemctl reload limpid
```

`systemctl reload` sends SIGHUP, which triggers a hot reload with automatic rollback on failure. Existing queue data and in-flight events are preserved.

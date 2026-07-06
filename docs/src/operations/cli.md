# CLI

## limpid

```bash
# Run as daemon
limpid --config /etc/limpid/limpid.conf

# Validate configuration and exit
limpid --check --config /etc/limpid/limpid.conf

# Test a pipeline with sample data
limpid --test-pipeline <name> --config /etc/limpid/limpid.conf \
  --input '{"ingress": "<134>test message"}'

# Enable debug trace logging
limpid --debug --config /etc/limpid/limpid.conf
```

### Options

| Flag | Description |
|------|-------------|
| `--config <path>` | Main configuration file (default: `/etc/limpid/limpid.conf`) |
| `--check` | Validate configuration and exit. Exit codes: `0` clean, `1` errors, `2` warnings present with `--strict-warnings`. |
| `--strict-warnings` | When combined with `--check`, treat any warning (recovery readiness, etc.) as a non-zero exit (`2`). Useful in CI to gate on warnings without failing on the absence of warnings. |
| `--ultra-strict` | When combined with `--check`, promote unknown-identifier warnings to errors. This is the only opt-in lint upgrade today and is *not* a generic style-level fail-on-warn; for "any warning fails CI" use `--strict-warnings`. |
| `--graph[=mermaid\|dot\|ascii]` | Print the configured pipeline graph (nodes + edges) and exit. Format defaults to `mermaid`; `dot` (Graphviz) and `ascii` are also accepted. Composes with `--check`: validation runs first, the graph is printed on success. |
| `--test-pipeline <name>` | Test a named pipeline with sample data |
| `--input <json>` | Sample event for test mode (JSON) |
| `--debug` | Enable trace-level logging |

`--check` prints a per-file header line and a final status footer:

```
$ limpid --check --config /etc/limpid/limpid.conf
checking /etc/limpid/limpid.conf: 5 file(s), 3 input(s), 2 output(s), 2 process(es), 2 pipeline(s)
/etc/limpid/limpid.conf: Configuration OK (2 pipeline(s), 2 process(es); dataflow check passed)
```

CI integration: combine `--check --strict-warnings` and treat exit `2` as a gate failure distinct from exit `1` (hard errors).

### Test mode input format

```json
{
  "ingress": "<134>Apr 15 10:30:00 myhost sshd: test",
  "source": {"ip": "192.0.2.3", "port": 514},
  "workspace": {
    "custom_field": "value"
  }
}
```

All keys except `ingress` are optional. `source` is the canonical `{ip, port}` object — same shape `tap --json` emits and the DSL `source` ident exposes. Field-level defaults make smoke testing terse:

| Input shape | Resulting `source` |
|---|---|
| `source` omitted entirely | `{ip: "127.0.0.1", port: 0}` |
| `source: {}` | `{ip: "127.0.0.1", port: 0}` |
| `source: {ip: "10.0.0.1"}` | `{ip: "10.0.0.1", port: 0}` |
| `source: {port: 5140}` | `{ip: "127.0.0.1", port: 5140}` |
| `source: {ip, port}` | exact |

When `source` is present but malformed (legacy `"ip:port"` string, wrong types, port out of range), `--test-pipeline` errors out loudly — operators migrating from the 0.5.5 form should see the failure, not silently get a wrong default. The Event has no facility / severity fields — the `<PRI>` byte lives inside `ingress` / `egress`, and pipelines that need its numeric value call `syslog.extract_pri(...)`.

`received_at` in the input JSON is **ignored** in test mode — `--test-pipeline` constructs the Event with `Event::new`, which stamps `received_at` to the current wall-clock. Only `ingress`, `source`, and `workspace` are honoured. When piping `tap --json` output into `--test-pipeline`, be aware that the captured timestamp will not be reproduced.

## limpidctl

```bash
# Stream events from an output
limpidctl tap output ama

# Stream events entering an input
limpidctl tap input fw_syslog

# Stream events after a process
limpidctl tap process enrich_fortigate

# Stream full Event JSON (one per line) — useful for piping to jq
limpidctl tap output ama --json

# Inject raw lines into an input (each stdin line becomes one event)
limpidctl inject input fw_syslog < raw.log

# Inject full-Event JSON (as emitted by `tap --json`) into an input
limpidctl inject input fw_syslog --json < events.jsonl

# Inject directly into an output queue, bypassing pipelines
limpidctl inject output ama < messages.log
limpidctl inject output ama --json < events.jsonl

# Replay with original cadence (or 10x faster, 0.2x slower, ...)
limpidctl inject input fw_syslog --json --replay-timing < events.jsonl
limpidctl inject input fw_syslog --json --replay-timing=10x < events.jsonl

# List available tap points
limpidctl list
limpidctl list --json

# Show metrics
limpidctl stats
limpidctl stats --json

# Health check
limpidctl health
limpidctl health --json
```

### Global options

| Flag | Description |
|------|-------------|
| `--socket <path>` | Control socket path (default: `/var/run/limpid/control.sock`) |

### Control socket parent safety

The control socket is a root-equivalent trust boundary, and its `bind → chmod 0o660` window relies on the parent directory being a real, trusted-owner directory that keeps non-group traffic out. Daemon startup **refuses to start** when the configured `control { socket "..." }`'s parent fails any of:

- **Final component symlink** — the final parent component is checked with `symlink_metadata`. A symlink parent lets an attacker redirect the bind target between this preflight and the daemon's actual `bind`, so a symlink final parent is rejected up front. **Ancestor** components may still be symlinks — modern Linux ships `/var/run` as a symlink to `/run`, and the packaged default path (`/var/run/limpid/control.sock` — final parent `limpid` is a real directory) relies on that compat. Ancestor path identity is a **deployment contract**: the daemon does not follow every ancestor to its root and refuse an ancestor-symlink chain, but it does trust that the ancestor chain resolves under a directory the operator controls.
- **Owner** — the parent must be owned by the daemon's own effective uid. An untrusted owner retains rename/unlink rights inside the directory regardless of mode bits and can replace the socket inode between bind and chmod. A root-owned parent is trusted **only when the daemon itself runs as root**; for a non-root daemon a root-owned parent at the packaged `0o750` is not writable anyway, so bind would fail post-validation and the fire-and-forget control task would die silently — the exact failure this preflight prevents.
- **Mode** — the parent must not be group-writable, world-writable, or world-traversable (predicate `mode & 0o023 != 0`).

If the parent does not exist yet, the preflight itself creates it at `0o750` under the daemon's own uid, but only after checking that the deepest existing ancestor is trusted. "Trusted" means both properties hold: (a) the ancestor is owned by the daemon's own uid or by root, AND (b) its mode is not group- or world-writable (`mode & 0o022 == 0`). Ownership alone is not enough — an ancestor at `0o777` still lets any process with write permission plant a node under the target name before the daemon's `chmod` runs. Both attacker-writable ancestors (e.g. `/tmp` at `0o1777`) and daemon-owned-but-world-writable ancestors refuse the create. After create, `symlink_metadata` on the created path re-verifies real-directory shape, daemon ownership, and the requested mode.

Under packaged systemd units (`RuntimeDirectory=limpid` combined with `User=<daemon-user>` — the packaged `limpid.service` ships with `User=syslog` — and `RuntimeDirectoryMode=0750` explicitly set in the unit file) every property is satisfied by construction — systemd creates the runtime dir at the daemon's uid with the requested mode — and this check is a no-op.

For custom deploys, ensure the parent is owned by the daemon's own uid (not root) and tightened to `0o750` (or `0o700` for owner-only) before starting the daemon. Substitute your daemon user for `<daemon-user>` (the packaged `limpid.service` uses `syslog`):

```sh
chown <daemon-user>:<daemon-group> /path/to/parent   # e.g. `chown syslog:syslog ...` for the packaged unit
chmod 0750 /path/to/parent                           # owner + group only
```

The failure diagnostic names whether the symlink shape, owner, or mode failed, prints the observed values alongside the daemon's own effective uid, and gives a remediation hint.

At shutdown, the control task records the `(dev, ino)` of the socket it bound and refuses to unlink the path if the on-disk inode has been swapped since bind. Under the safe-parent contract above no outside-the-group writer can produce the swap; this is defense-in-depth, not the load-bearing guard.

### Control socket limits

The control socket is a local root-equivalent trust boundary (mode `0o660` in
a daemon-owned runtime directory — the packaged unit provisions
`/var/run/limpid/` at `User=syslog` via `RuntimeDirectory=limpid` +
`RuntimeDirectoryMode=0750`), but limpid still enforces these limits as
defense in depth:

| Limit | Value | Behaviour on breach |
|-------|-------|---------------------|
| Concurrent connections | 8 | New connections receive `error: control socket busy (too many concurrent connections)` and are closed immediately. |
| Inject payload size (per connection) | 16 MiB | Stream is cut off; response includes `"error"` field alongside the partial `"injected"` count. |
| First-line command length | 4 KiB | Command is rejected with `error: command too long`. |

For larger replay jobs, split the input into multiple `limpidctl inject`
invocations.

See [Debug Tap](./tap.md) for details.

## limpid-prometheus

Prometheus exporter — converts limpid's JSON stats to Prometheus text exposition format.

```bash
limpid-prometheus --bind 127.0.0.1:9100 --socket /var/run/limpid/control.sock
```

| Flag | Description |
|------|-------------|
| `--bind <addr>` | HTTP bind address (default: `127.0.0.1:9100`) |
| `--socket <path>` | Control socket path (default: `/var/run/limpid/control.sock`) |

| Endpoint | Response |
|----------|----------|
| `GET /health` | `OK` (plain text) |
| `GET /metrics` | Prometheus text format (`text/plain; version=0.0.4`) |

See [Metrics](./metrics.md) for the full list of exported metrics.

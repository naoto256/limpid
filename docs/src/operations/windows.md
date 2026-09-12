# Windows service

limpid 0.9.0 uses a ZIP package with PowerShell installation scripts. MSI packaging is not part of this release.

## Platform status

0.9.0 distributes Windows x64 and ARM64 ZIPs; availability of an architecture build is not a full OS/service support certification. Saved results from earlier Windows 11 ARM64 integration builds cover package installation, the virtual service account, Event Log collection and bookmark resume, the control pipe, Prometheus scraping, configuration reload, upgrade, and automatic start after reboot. These are not a full-matrix rerun on every subsequent candidate. The integrated candidate has also been exercised through SCM for a long-running shutdown with observable progress.

Windows 11 x64 and Windows Server 2022 or later x64 are release targets, but they must complete the same native acceptance matrix before being advertised as tested combinations. The package requires the Microsoft Visual C++ runtime for its architecture (`VCRUNTIME140.dll`); the ZIP does not install it.

## Install

Extract the ZIP and run its installer from elevated 64-bit PowerShell:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\install.ps1
```

The installer:

- copies `limpid.exe`, `limpidctl.exe`, and `limpid-prometheus.exe` to `%ProgramFiles%\limpid`;
- creates `%ProgramData%\limpid\config`, `state`, and `log` with protected ACLs;
- registers the `limpid` service as `NT SERVICE\limpid` and adds that identity to Event Log Readers;
- sets the service to Automatic for later boots;
- preserves existing configuration, checkpoints, logs, and sibling directories during an upgrade.

The installer deliberately leaves the service stopped on first installation. Review and validate the generated configuration before the first start:

```powershell
& "$env:ProgramFiles\limpid\limpid.exe" --check `
  --config "$env:ProgramData\limpid\config\limpid.conf"
Start-Service limpid
Get-Service limpid
```

The default configuration reads new `System` channel events, stores its bookmark under `%ProgramData%\limpid\state`, and writes JSONL under `%ProgramData%\limpid\log`.

## Operate

Run control commands from an elevated shell. The daemon, `limpidctl`, and `limpid-prometheus` use the local named pipe `\\.\pipe\limpid-control` by default.

```powershell
limpidctl health
limpidctl stats
limpidctl tap input system_events --json
limpid-prometheus --bind 127.0.0.1:9100
```

The pipe accepts local names only. Its ACL grants access to the executing service identity, SYSTEM, and Administrators. First-instance creation fails if the name is already owned; it does not take over an endpoint pre-created by another process.

Use Service Control Manager operations for lifecycle changes:

```powershell
# Validate first, then request the same configuration reload used on Unix.
& "$env:ProgramFiles\limpid\limpid.exe" --check `
  --config "$env:ProgramData\limpid\config\limpid.conf"
sc.exe control limpid 6

# Graceful shutdown.
Stop-Service limpid
```

SCM control 6 (`PARAMCHANGE`) validates the candidate configuration and keeps the existing runtime running when validation fails. A successful reload drains the old runtime and rebinds inputs, so new connections can see brief downtime.

Shutdown retains tasks that must finish. Completed processing, successful delivery/recovery notifications, consumer bookkeeping, and completed task joins contribute to coalesced SCM checkpoints while stopping. Merely waiting does not advance a checkpoint; a notification does not prove that a bookmark has been durably persisted. The wait hint remains 30 seconds. An owner blocked without observable progress can exceed that hint; there is no periodic heartbeat or forced-abort guarantee.

The integrated candidate was observed in `STOP_PENDING` for about 95.95 seconds with checkpoints advancing from 1 to 71, then reached `STOPPED` with both SCM exit codes zero. This was mixed delivery/recovery, not an all-delivery result. A no-progress stall and higher-level control-client timeout behavior were not tested. `sc.exe stop` request acceptance, used in that run, is distinct from waiting for completion in `Stop-Service` or another client.

Diagnostic logs are written to `%ProgramData%\limpid\log\daemon.log`. In foreground mode, Ctrl-C and Ctrl-Break request graceful shutdown.

## Upgrade and uninstall

Stop the service explicitly before upgrading, extract the new ZIP, and run its `install.ps1`:

```powershell
Stop-Service limpid
.\install.ps1
Start-Service limpid
```

The installer refuses to replace a running installation. It retains the existing configuration and Event Log bookmark files.

To unregister the service:

```powershell
Stop-Service limpid
.\uninstall.ps1
```

Uninstall removes the service registration and Event Log Readers membership. It preserves the binaries, configuration, state, and logs so removal never recursively deletes operator data.

## Security and current limits

- `NT SERVICE\limpid` receives read access to configuration and modify access to state and logs. SYSTEM and Administrators retain management access. The installer refuses reparse points and untrusted write grants in managed paths.
- `limpidctl ltp keygen` creates keys with protected Windows ACLs. A key generated by an administrator must be reprovisioned with `NT SERVICE\limpid` as owner and grants limited to that identity, SYSTEM, and Administrators before the service can use it.
- `file` output supports Windows paths. Its Unix `mode`, `owner`, and `group` properties are rejected on Windows; manage file ACLs outside the DSL.
- Unix socket input/output and the Linux `journal` input are unavailable on Windows. `windows_event_log` is the native host-log input.
- The package configures Automatic boot start. It does not currently configure SCM recovery actions, so SCM will not automatically restart limpid after an unexpected process termination.
- The ZIP includes snippets, but the installer does not copy them into the configuration tree. Copy the needed files under `%ProgramData%\limpid\config\snippets` and reference them with relative `include` paths.

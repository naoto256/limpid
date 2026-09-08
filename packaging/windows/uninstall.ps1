[CmdletBinding(SupportsShouldProcess)]
param()
$ErrorActionPreference = 'Stop'
$service = Get-Service -Name limpid -ErrorAction SilentlyContinue
if (-not $service) { Write-Output 'limpid service is not registered.'; return }
$registration = Get-CimInstance Win32_Service -Filter "Name='limpid'"
$expected = '"' + (Join-Path $env:ProgramFiles 'limpid/limpid.exe') + '"'
if (-not $registration.PathName.StartsWith($expected, [StringComparison]::OrdinalIgnoreCase) -or $registration.StartName -ne 'NT SERVICE\limpid') { throw 'Existing service does not match this package; refusing to remove it.' }
if ($service.Status -ne 'Stopped') { throw 'Stop limpid explicitly before uninstalling.' }
if (-not $PSCmdlet.ShouldProcess('limpid', 'Remove service registration; preserve binaries, configuration, state and logs')) { return }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run from an elevated PowerShell session.' }
$sid = ([Security.Principal.NTAccount]::new('NT SERVICE\limpid')).Translate([Security.Principal.SecurityIdentifier])
$readers = Get-LocalGroup -SID 'S-1-5-32-573'
if (Get-LocalGroupMember -Group $readers | Where-Object { $_.SID -eq $sid }) { Remove-LocalGroupMember -Group $readers -Member $sid.Value }
& "$env:SystemRoot/System32/sc.exe" delete limpid
if ($LASTEXITCODE -ne 0) { throw "Service removal failed: $LASTEXITCODE" }
Write-Output 'Service registration removed. Binaries, configuration, state and logs were preserved.'

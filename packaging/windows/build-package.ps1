# Purpose: Assemble a ZIP distribution that contains the three limpid binaries,
# the installer and uninstaller scripts, the example configuration, this
# directory's README, and the bundled snippet library, plus a SHA256SUMS
# manifest for the binaries.
# Preconditions: BinaryDirectory holds a fresh build of limpid.exe,
# limpidctl.exe, and limpid-prometheus.exe; OutputPath does not yet exist so a
# stale archive cannot be republished by accident.
# Modifies: Creates a per-invocation staging directory next to OutputPath,
# copies the payload into it, writes SHA256SUMS, and produces the archive.
# Preserves: The BinaryDirectory sources are copied, not moved; the staging
# directory is deliberately retained after the archive is written so the exact
# contents can be inspected before distribution.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryDirectory,
    [Parameter(Mandatory)][string]$OutputPath
)
$ErrorActionPreference = 'Stop'
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $OutputPath) { throw 'Output archive already exists; select a new candidate path.' }
$binaryNames = @('limpid.exe', 'limpidctl.exe', 'limpid-prometheus.exe')
foreach ($name in $binaryNames) {
    if (-not (Test-Path -LiteralPath (Join-Path $BinaryDirectory $name) -PathType Leaf)) {
        throw "Missing built binary: $name"
    }
}
$parent = [IO.Path]::GetDirectoryName($OutputPath)
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$staging = Join-Path $parent ('limpid-package-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
foreach ($name in $binaryNames) {
    Copy-Item -LiteralPath (Join-Path $BinaryDirectory $name) -Destination (Join-Path $staging $name)
}
$scriptPayload = @('install.ps1', 'uninstall.ps1', 'limpid.conf.example', 'README.md')
foreach ($name in $scriptPayload) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $staging $name)
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../snippets') -Destination (Join-Path $staging 'snippets') -Recurse
$hashes = foreach ($name in $binaryNames) {
    $hash = Get-FileHash -LiteralPath (Join-Path $staging $name) -Algorithm SHA256
    "{0}  {1}" -f $hash.Hash, $name
}
[IO.File]::WriteAllLines((Join-Path $staging 'SHA256SUMS'), $hashes, [Text.UTF8Encoding]::new($false))
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($staging, $OutputPath)
Write-Output "Created $OutputPath. Reviewable staging files retained at $staging."

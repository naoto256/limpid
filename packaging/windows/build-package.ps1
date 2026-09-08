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
    if (-not (Test-Path -LiteralPath (Join-Path $BinaryDirectory $name) -PathType Leaf)) { throw "Missing built binary: $name" }
}
$parent = [IO.Path]::GetDirectoryName($OutputPath)
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$staging = Join-Path $parent ('limpid-package-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
foreach ($name in $binaryNames) { Copy-Item -LiteralPath (Join-Path $BinaryDirectory $name) -Destination (Join-Path $staging $name) }
foreach ($name in @('install.ps1', 'uninstall.ps1', 'limpid.conf.example', 'README.md')) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $staging $name) }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../snippets') -Destination (Join-Path $staging 'snippets') -Recurse
$hashes = foreach ($name in $binaryNames) { $hash = Get-FileHash -LiteralPath (Join-Path $staging $name) -Algorithm SHA256; "{0}  {1}" -f $hash.Hash, $name }
[IO.File]::WriteAllLines((Join-Path $staging 'SHA256SUMS'), $hashes, [Text.UTF8Encoding]::new($false))
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($staging, $OutputPath)
Write-Output "Created $OutputPath. Reviewable staging files retained at $staging."

#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PackageDirectory,
    [Parameter(Mandatory)][string]$ResultsDirectory
)
$ErrorActionPreference = 'Stop'
$fixture = [IO.Path]::GetFullPath($ResultsDirectory)
if (Test-Path -LiteralPath $fixture) { throw 'Choose a new fixture directory.' }
# Load production helpers through the read-only installer preflight.
. "$PSScriptRoot/install.ps1" -PackageDirectory $PackageDirectory -WhatIf
New-Item -ItemType Directory -Path $fixture | Out-Null
$sibling = Join-Path $fixture 'unmanaged'
New-Item -ItemType Directory -Path $sibling | Out-Null
$payload = Join-Path $sibling 'fixture.txt'
[IO.File]::WriteAllText($payload, 'synthetic sibling fixture')
$everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$original = Get-Acl -LiteralPath $fixture
$parentAcl = Get-Acl -LiteralPath $fixture
$parentAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'Write', 'ContainerInherit, ObjectInherit', 'None', 'Allow'))
try {
    Set-Acl -LiteralPath $fixture -AclObject $parentAcl
    $parentBefore = (Get-Acl -LiteralPath $fixture).Sddl
    $siblingBefore = (Get-Acl -LiteralPath $sibling).Sddl
    Assert-TrustedWrites $fixture -SharedParent
    # Test the native ACL-at-creation path without elevation. Production uses
    # Administrators as owner and the virtual service SID; those assignments
    # still require the separate elevated installation test.
    $admins = $identity.User
    $script:serviceSid = $system
    $managed = Join-Path $fixture 'config'
    Set-ManagedDirectory $managed 'ReadAndExecute'
    $createdAcl = Get-Acl -LiteralPath $managed
    if (-not $createdAcl.AreAccessRulesProtected) { throw 'Child ACL must be protected at creation.' }
    $rules = $createdAcl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    if (@($rules | Where-Object { $_.IdentityReference.Value -eq $everyone.Value }).Count) { throw 'Broad parent grant leaked into child.' }
    if ((Get-Acl -LiteralPath $fixture).Sddl -ne $parentBefore -or (Get-Acl -LiteralPath $sibling).Sddl -ne $siblingBefore) { throw 'Parent or sibling ACL changed.' }
    if ([IO.File]::ReadAllText($payload) -cne 'synthetic sibling fixture') { throw 'Sibling contents changed.' }
    $parentAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'DeleteSubdirectoriesAndFiles', 'Allow'))
    Set-Acl -LiteralPath $fixture -AclObject $parentAcl
    $rejected = $false
    try { Assert-TrustedWrites $fixture -SharedParent }
    catch { if ($_.Exception.Message -like 'Untrusted write grant*') { $rejected = $true } else { throw } }
    if (-not $rejected) { throw 'Unsafe parent deletion grant accepted.' }
} finally {
    Set-Acl -LiteralPath $fixture -AclObject $original
}
Write-Output 'PASS: shared parent accepted; child ACL protected at creation; sibling preserved; parent DeleteChild rejected. Elevated service identity not tested.'

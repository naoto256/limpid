#requires -Version 7.0
# Purpose: Exercise install.ps1's ACL and shared-parent helpers against a
# throwaway fixture, covering the native ACL-at-creation path and the
# DeleteChild rejection contract.
# Preconditions: PowerShell 7.0+; PackageDirectory is usable by install.ps1's
# WhatIf preflight (which is dot-sourced to reuse the production helpers);
# ResultsDirectory does not yet exist so no unrelated content is co-mingled.
# Modifies: Creates the fixture, an unmanaged sibling with a payload file,
# and a managed child directory, and grants Everyone Write on the fixture
# parent so the acceptance and rejection paths can both be exercised. The
# finally block restores the fixture's original ACL before returning.
# Preserves: Nothing outside the fixture directory is touched. Sibling
# payload bytes and both the parent and sibling SDDLs are asserted unchanged,
# and the local $admins substitution used to bypass elevation is scoped to a
# nested block so parent-trust checks stay honest.
# Test boundary: Helper ACL verification only; this does not validate the
# production Administrators owner or virtual service SID assignments, which
# require the separate elevated installation test.
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
$parentOwner = (Get-Acl -LiteralPath ([IO.Path]::GetDirectoryName($fixture))).GetOwner([Security.Principal.SecurityIdentifier]).Value
Write-Output "ACL fixture: user=$($identity.User.Value); owner=$($original.GetOwner([Security.Principal.SecurityIdentifier]).Value); parentOwner=$parentOwner; administratorRole=$([Security.Principal.WindowsPrincipal]::new($identity).IsInRole($admins))"
$parentAcl = Get-Acl -LiteralPath $fixture
$parentAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'Write', 'ContainerInherit, ObjectInherit', 'None', 'Allow'))
try {
    Set-Acl -LiteralPath $fixture -AclObject $parentAcl
    $parentBefore = (Get-Acl -LiteralPath $fixture).Sddl
    $siblingBefore = (Get-Acl -LiteralPath $sibling).Sddl
    Write-Output 'Checking shared-parent acceptance before child creation.'
    Assert-TrustedWrites $fixture -SharedParent
    # Test the native ACL-at-creation path without elevation. Production uses
    # Administrators as owner and the virtual service SID; those assignments
    # still require the separate elevated installation test.
    $script:serviceSid = $system
    $managed = Join-Path $fixture 'config'
    $trustedOwnersBefore = @($admins.Value, $system.Value, $identity.User.Value)
    & {
        # Keep the test owner substitution out of later parent trust checks.
        $admins = $identity.User
        Set-ManagedDirectory $managed 'ReadAndExecute'
    }
    $trustedOwnersAfter = @($admins.Value, $system.Value, $identity.User.Value)
    if (($trustedOwnersAfter -join ',') -cne ($trustedOwnersBefore -join ',')) { throw 'Test owner substitution changed parent trust.' }
    $createdAcl = Get-Acl -LiteralPath $managed
    if (-not $createdAcl.AreAccessRulesProtected) { throw 'Child ACL must be protected at creation.' }
    $rules = $createdAcl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    if (@($rules | Where-Object { $_.IdentityReference.Value -eq $everyone.Value }).Count) { throw 'Broad parent grant leaked into child.' }
    if ((Get-Acl -LiteralPath $fixture).Sddl -ne $parentBefore -or (Get-Acl -LiteralPath $sibling).Sddl -ne $siblingBefore) { throw 'Parent or sibling ACL changed.' }
    if ([IO.File]::ReadAllText($payload) -cne 'synthetic sibling fixture') { throw 'Sibling contents changed.' }
    $parentAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'DeleteSubdirectoriesAndFiles', 'Allow'))
    Set-Acl -LiteralPath $fixture -AclObject $parentAcl
    $rejected = $false
    Write-Output 'Checking shared-parent DeleteChild rejection.'
    try { Assert-TrustedWrites $fixture -SharedParent }
    catch { if ($_.Exception.Message -like 'Untrusted write grant*') { $rejected = $true } else { throw } }
    if (-not $rejected) { throw 'Unsafe parent deletion grant accepted.' }
} finally {
    Set-Acl -LiteralPath $fixture -AclObject $original
}
Write-Output 'PASS: shared parent accepted; child ACL protected at creation; sibling preserved; parent DeleteChild rejected. Elevated service identity not tested.'

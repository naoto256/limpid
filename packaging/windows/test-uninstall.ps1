#requires -Version 5.1
# Purpose: Verify uninstall.ps1's Event Log Readers membership-removal block
# against every observed error shape (member/absent/orphan/denied/
# missing-group/other/wrong-error-id), and optionally probe the native
# Microsoft.PowerShell.LocalAccounts absence semantics on a hosted runner.
# Preconditions: PowerShell 5.1+; on Windows the LocalAccounts module is
# imported for a real MemberNotFoundException type, on other platforms a
# stub type is added so the mock still parses. The native probe requires
# -NativeLocalAccounts plus GITHUB_ACTIONS=true on a github-hosted Windows
# runner and refuses to run elsewhere.
# Modifies: Reads uninstall.ps1 as text, extracts the membership block via
# regex, and evaluates it against shim Get-LocalGroup /
# Remove-LocalGroupMember functions for each case. The native probe creates
# a disposable local group (unique GUID name) and deletes it in a finally
# block that revalidates the probe's SID before removal.
# Preserves: uninstall.ps1 is only read, never rewritten; no machine group
# other than the disposable probe is touched. Get-LocalGroupMember is
# intentionally never called even in the shim so the mock cannot encourage
# enumerating unrelated (possibly orphaned) SIDs.
# Test boundary: Membership-removal contract only. SCM deletion, elevation,
# and service identity checks belong to the elevated installation and
# uninstallation tests.
[CmdletBinding()]
param([switch]$NativeLocalAccounts)
$ErrorActionPreference = 'Stop'

# === Mock membership block extraction ===
$source = Get-Content -Raw (Join-Path $PSScriptRoot 'uninstall.ps1')
$match = [regex]::Match($source, '(?ms)^\$readers =.*?(?=^& )')
if (-not $match.Success) { throw 'Uninstall membership block not found.' }
$membership = [scriptblock]::Create($match.Value)
$absentId = 'MemberNotFound,Microsoft.PowerShell.Commands.RemoveLocalGroupMemberCommand'
if ($env:OS -eq 'Windows_NT') { Import-Module Microsoft.PowerShell.LocalAccounts }
if (-not ('Microsoft.PowerShell.Commands.MemberNotFoundException' -as [type])) {
    # Non-Windows mock only; native runs use the installed module's real type.
    Add-Type 'namespace Microsoft.PowerShell.Commands { public class MemberNotFoundException : System.Exception {} }'
}

# === Mock membership test harness ===
function Test-MembershipCase([string]$Case, [bool]$ExpectedContinue) {
    $sid = [pscustomobject]@{ Value = 'S-1-5-21-1-2-3-1001' }
    function Get-LocalGroup {
        param($SID)
        if ($SID -ne 'S-1-5-32-573') { throw 'Unexpected group target.' }
        if ($Case -eq 'missing-group') { throw 'Group missing.' }
        'owned-mock-group'
    }
    function Get-LocalGroupMember { throw 'Unrelated (possibly orphaned) members must not be enumerated.' }
    function Remove-LocalGroupMember {
        param($Group, $Member, $ErrorAction)
        if ($Group -ne 'owned-mock-group' -or
            $Member -ne $sid.Value -or
            $ErrorAction -ne 'Stop') {
            throw 'Unexpected removal target/options.'
        }
        if ($Case -in @('absent', 'wrong-error-id')) {
            $errorId = if ($Case -eq 'absent') { $absentId } else { 'UnexpectedError' }
            throw [Management.Automation.ErrorRecord]::new(
                [Microsoft.PowerShell.Commands.MemberNotFoundException]::new(),
                $errorId, [Management.Automation.ErrorCategory]::ObjectNotFound, $Member)
        }
        if ($Case -eq 'denied') { throw [UnauthorizedAccessException]::new('Denied') }
        if ($Case -eq 'other') { throw 'Other failure.' }
    }
    $continued = $false
    try {
        & $membership
        $continued = $true
    } catch { }
    if ($continued -ne $ExpectedContinue) {
        throw "Unexpected service-delete eligibility: $Case"
    }
    Write-Output "Mock $Case PASS"
}
foreach ($case in @('member', 'absent', 'unrelated-orphan')) {
    Test-MembershipCase $case $true
}
foreach ($case in @('denied', 'missing-group', 'other', 'wrong-error-id')) {
    Test-MembershipCase $case $false
}

# === Native LocalAccounts probe (opt-in) ===
if (-not $NativeLocalAccounts) { return }
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:OS -ne 'Windows_NT') {
    throw 'Native group probe is restricted to GitHub-hosted Windows runners.'
}
$module = Get-Module Microsoft.PowerShell.LocalAccounts
Write-Output "PowerShell=$($PSVersionTable.PSVersion) LocalAccounts=$($module.Version)"
$probeName = 'limpid-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$probe = $null
try {
    $probe = New-LocalGroup -Name $probeName -Description 'Disposable limpid CI uninstall probe'
    $memberSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    Add-LocalGroupMember -Group $probe -Member $memberSid -ErrorAction Stop
    Remove-LocalGroupMember -Group $probe -Member $memberSid -ErrorAction Stop
    $absent = $false
    try {
        Remove-LocalGroupMember -Group $probe -Member $memberSid -ErrorAction Stop
    } catch {
        Write-Output "Native absence type=$($_.Exception.GetType().FullName) FQID=$($_.FullyQualifiedErrorId)"
        if ($_.Exception.GetType().FullName -ne 'Microsoft.PowerShell.Commands.MemberNotFoundException' -or
            $_.FullyQualifiedErrorId -ne $absentId) {
            throw
        }
        $absent = $true
    }
    if (-not $absent) { throw 'Expected the already-absent membership to be reported.' }
    Write-Output 'Native SID removal and absence discrimination PASS (orphan case is mock-only).'
} finally {
    if ($null -ne $probe) {
        $current = Get-LocalGroup -Name $probeName -ErrorAction Stop
        if ($current.SID -ne $probe.SID) { throw 'Probe group identity changed; refusing cleanup.' }
        Remove-LocalGroup -SID $probe.SID -ErrorAction Stop
    }
}

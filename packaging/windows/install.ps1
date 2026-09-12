# Purpose: Install or upgrade the limpid Windows service, its binaries, and its
# protected data directories. The service is registered but never started here.
# Preconditions: Elevated PowerShell; the fixed InstallDirectory
# %ProgramFiles%\limpid and DataDirectory %ProgramData%\limpid; the package
# supplies limpid.exe, limpidctl.exe, limpid-prometheus.exe, and
# limpid.conf.example alongside this script.
# Modifies: Copies the three binaries into InstallDirectory and applies
# protected ACLs (owner Administrators; SYSTEM and Administrators FullControl;
# NT SERVICE\limpid ReadAndExecute or Modify per role). Creates
# DataDirectory\{config,state,log} the same way, registers the SCM service via
# Win32_Service Create (StartName NT SERVICE\limpid), switches StartupType to
# Automatic, and adds the service SID to the built-in Event Log Readers group.
# Preserves: An existing limpid.conf is left untouched; sibling directories
# under DataDirectory keep their inherited ACLs (shared-parent rules only guard
# against write grants that could delete or reparent the managed children);
# an existing service registration that does not match this package is refused
# rather than replaced, and the service is left stopped for operator review.
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$PackageDirectory = $PSScriptRoot,
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles 'limpid'),
    [string]$DataDirectory = (Join-Path $env:ProgramData 'limpid')
)
$ErrorActionPreference = 'Stop'
$serviceName = 'limpid'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')

function Assert-PlainPath([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Refusing reparse point: $current" }
        }
        $current = [IO.Path]::GetDirectoryName($current)
    }
}
function Set-ManagedFile([string]$Path) {
    Assert-PlainPath $Path
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetOwner($admins)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($admins, $system)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow'))
    }
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($script:serviceSid, 'ReadAndExecute', 'Allow'))
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Assert-TrustedDirectory([string]$Path) {
    Assert-PlainPath $Path
    if (Test-Path -LiteralPath $Path) {
        if (-not (Get-Item -LiteralPath $Path).PSIsContainer) { throw "Not a directory: $Path" }
        $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($owner -notin @($admins.Value, $system.Value, $identity.User.Value)) { throw "Untrusted existing directory owner: $Path" }
    }
}
function Assert-TrustedWrites([string]$Path, [string[]]$AdditionalTrusted = @(), [switch]$SharedParent) {
    Assert-PlainPath $Path
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $acl = Get-Acl -LiteralPath $Path
    $trusted = @($admins.Value, $system.Value, $identity.User.Value) + $AdditionalTrusted
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $trusted) { throw "Untrusted existing owner: $Path" }
    if ($SharedParent) {
        # Creating unrelated siblings is compatible with protected children.
        # Deleting/replacing those children or changing parent ownership is not.
        $namedWriteRights = 'Delete, DeleteSubdirectoriesAndFiles, ChangePermissions, TakeOwnership'
        $genericWriteMask = 0x10000000 # GENERIC_ALL
    } else {
        $namedWriteRights = 'Write, Delete, DeleteSubdirectoriesAndFiles, ChangePermissions, TakeOwnership'
        $genericWriteMask = 0x50000000 # GENERIC_WRITE and GENERIC_ALL
    }
    $writeMask = ([int64][Security.AccessControl.FileSystemRights]$namedWriteRights) -bor $genericWriteMask
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) { continue }
        if ($rule.AccessControlType -eq 'Allow' -and ([int64]$rule.FileSystemRights -band $writeMask) -and $rule.IdentityReference.Value -notin $trusted) {
            throw "Untrusted write grant on existing managed content: $Path"
        }
    }
}
function Assert-ExistingBinaries([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    Assert-TrustedDirectory $Path
    Assert-TrustedWrites $Path
    # Check every existing entry, including DLLs that Windows could load next
    # to the executable. Do not recurse through a junction or symbolic link.
    foreach ($entry in Get-ChildItem -LiteralPath $Path -Force) {
        Assert-TrustedWrites $entry.FullName
        if ($entry.PSIsContainer) { Assert-ExistingBinaries $entry.FullName }
    }
}
function Set-ManagedDirectory([string]$Path, [Security.AccessControl.FileSystemRights]$ServiceRights) {
    Assert-TrustedDirectory $Path
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($admins)
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($sid in @($admins, $system)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', $inherit, 'None', 'Allow'))
    }
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($script:serviceSid, $ServiceRights, $inherit, 'None', 'Allow'))
    if (Test-Path -LiteralPath $Path) {
        Assert-TrustedWrites $Path @($script:serviceSid.Value)
        Set-Acl -LiteralPath $Path -AclObject $acl
    } else {
        # Apply the protected descriptor during creation, without a window
        # where the parent's broad inherited write grants reach this directory.
        $directory = [IO.DirectoryInfo]::new($Path)
        if ($PSVersionTable.PSEdition -eq 'Core') {
            [IO.FileSystemAclExtensions]::Create($directory, $acl)
        } else {
            $directory.Create($acl)
        }
        # A concurrent creator may have won the name; validate that result.
        Assert-TrustedDirectory $Path
        Assert-TrustedWrites $Path @($script:serviceSid.Value)
    }
}

# === Argument and payload validation ===
$InstallDirectory = [IO.Path]::GetFullPath($InstallDirectory)
$DataDirectory = [IO.Path]::GetFullPath($DataDirectory)
if ($InstallDirectory -ne [IO.Path]::GetFullPath((Join-Path $env:ProgramFiles 'limpid'))) {
    throw 'Custom InstallDirectory is not supported by this package.'
}
# The service's diagnostic log and default paths use the platform ProgramData.
if ($DataDirectory -ne [IO.Path]::GetFullPath((Join-Path $env:ProgramData 'limpid'))) {
    throw 'Custom DataDirectory is not supported by this package.'
}
foreach ($binary in @('limpid.exe', 'limpidctl.exe', 'limpid-prometheus.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $PackageDirectory $binary) -PathType Leaf)) { throw "Package is missing $binary" }
    Assert-PlainPath (Join-Path $PackageDirectory $binary)
}
if (-not (Test-Path -LiteralPath (Join-Path $PackageDirectory 'limpid.conf.example') -PathType Leaf)) { throw 'Package is missing limpid.conf.example' }
Assert-PlainPath (Join-Path $PackageDirectory 'limpid.conf.example')

# === Existing filesystem and service state ===
Assert-TrustedDirectory $InstallDirectory
Assert-TrustedDirectory $DataDirectory
Assert-TrustedWrites $DataDirectory -SharedParent
Assert-ExistingBinaries $InstallDirectory
Assert-TrustedDirectory (Join-Path $DataDirectory 'config')
Assert-TrustedWrites (Join-Path $DataDirectory 'config')
Assert-TrustedWrites (Join-Path $DataDirectory 'config/limpid.conf')
$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing -and $existing.Status -ne 'Stopped') {
    throw 'Stop limpid explicitly before upgrading; the installer does not stop an existing service.'
}
$executable = Join-Path $InstallDirectory 'limpid.exe'
$config = Join-Path $DataDirectory 'config/limpid.conf'
if ($existing) {
    $registration = Get-CimInstance Win32_Service -Filter "Name='limpid'"
    $expectedPathName = '"' + $executable + '"'
    if (-not $registration.PathName.StartsWith($expectedPathName, [StringComparison]::OrdinalIgnoreCase) -or
        $registration.StartName -ne 'NT SERVICE\limpid') {
        throw 'Existing limpid service does not match this installation and identity.'
    }
}

# === Elevation gates and SCM registration ===
if (-not $PSCmdlet.ShouldProcess($InstallDirectory, 'Install/update limpid, register a stopped service and configure its directories')) { return }
if (-not ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole($admins)) { throw 'Run the installer from an elevated PowerShell session.' }

if (-not $existing) {
    # Virtual accounts require a NULL password, not the empty password that
    # New-Service marshals from PSCredential (CreateService error 1057).
    $created = Invoke-CimMethod -ClassName Win32_Service -MethodName Create -Arguments @{
        Name = $serviceName
        DisplayName = 'limpid'
        PathName = ('"' + $executable + '" --service --config "' + $config + '"')
        ServiceType = [byte]16
        ErrorControl = [byte]1
        DesktopInteract = $false
        StartMode = 'Manual'
        StartName = 'NT SERVICE\limpid'
    }
    if ($created.ReturnValue -ne 0) { throw "Service creation failed (Win32_Service result $($created.ReturnValue))." }
}
# Registration creates the virtual service identity before assigning its ACLs.
$script:serviceSid = ([Security.Principal.NTAccount]::new('NT SERVICE\limpid')).Translate([Security.Principal.SecurityIdentifier])

# === Protected ACL application ===
Set-ManagedDirectory $InstallDirectory 'ReadAndExecute'
if (-not (Test-Path -LiteralPath $DataDirectory)) {
    Set-ManagedDirectory $DataDirectory 'ReadAndExecute'
} else {
    # Existing sibling trees (for example parser-development fixtures) and
    # their inherited ACLs are outside the installer-managed directories.
    Assert-TrustedDirectory $DataDirectory
    Assert-TrustedWrites $DataDirectory -SharedParent
}
Set-ManagedDirectory (Join-Path $DataDirectory 'config') 'ReadAndExecute'
Set-ManagedDirectory (Join-Path $DataDirectory 'state') 'Modify'
Set-ManagedDirectory (Join-Path $DataDirectory 'log') 'Modify'

# === Binary and config placement ===
foreach ($binary in @('limpid.exe', 'limpidctl.exe', 'limpid-prometheus.exe')) {
    $destination = Join-Path $InstallDirectory $binary
    Assert-PlainPath $destination
    Copy-Item -LiteralPath (Join-Path $PackageDirectory $binary) -Destination $destination -Force
    Set-ManagedFile $destination
}
Assert-PlainPath $config
if (-not (Test-Path -LiteralPath $config)) {
    $template = [IO.File]::ReadAllText((Join-Path $PackageDirectory 'limpid.conf.example'))
    [IO.File]::WriteAllText($config, $template.Replace('@LIMPID_DATA@', $DataDirectory.Replace('\', '/')), [Text.UTF8Encoding]::new($false))
}
Set-ManagedFile $config

# === Event Log Readers membership and service startup ===
# Event Log Readers is localized by name; address the built-in group by SID.
$readers = Get-LocalGroup -SID 'S-1-5-32-573'
$alreadyMember = Get-LocalGroupMember -Group $readers | Where-Object { $_.SID -eq $script:serviceSid }
if (-not $alreadyMember) {
    Add-LocalGroupMember -Group $readers -Member $script:serviceSid.Value
}
Set-Service -Name $serviceName -StartupType Automatic
Write-Output "Installed. Review $config, run limpid.exe --check --config with that path, then Start-Service limpid. The service has not been started."

#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryDirectory,
    [Parameter(Mandatory)][string]$ResultsDirectory
)
$ErrorActionPreference = 'Stop'
$BinaryDirectory = [IO.Path]::GetFullPath($BinaryDirectory)
$ResultsDirectory = [IO.Path]::GetFullPath($ResultsDirectory)
if (Test-Path -LiteralPath $ResultsDirectory) { throw 'Choose a new results directory.' }
New-Item -ItemType Directory -Path $ResultsDirectory | Out-Null
$utf8 = [Text.UTF8Encoding]::new($false)
$daemonExe = Join-Path $BinaryDirectory 'limpid.exe'
$ctlExe = Join-Path $BinaryDirectory 'limpidctl.exe'
$script:invocation = 0
function Invoke-Bounded([string]$Executable, [string[]]$Arguments, [string]$InputFile = '') {
    $script:invocation++
    $prefix = Join-Path $ResultsDirectory "command-$script:invocation"
    $options = @{
        FilePath = $Executable
        ArgumentList = $Arguments
        WindowStyle = 'Hidden'
        PassThru = $true
        RedirectStandardOutput = "$prefix.stdout"
        RedirectStandardError = "$prefix.stderr"
    }
    if ($InputFile) { $options.RedirectStandardInput = $InputFile }
    $process = Start-Process @options
    if (-not $process.WaitForExit(5000)) {
        Stop-Process -Id $process.Id -Force
        $process.WaitForExit()
        throw "Command exceeded 5s: $Executable"
    }
    if ($process.ExitCode -ne 0) { throw "Command failed (see $prefix.stderr): $Executable" }
    [IO.File]::ReadAllText("$prefix.stdout")
}
function Run-Candidate([string]$Query, [string]$Bookmark, [scriptblock]$Verify) {
    $pipe = '\\.\pipe\limpid-native-' + [Guid]::NewGuid().ToString('N')
    $run = Join-Path $ResultsDirectory ([Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $run | Out-Null
    $state = Join-Path $run 'bookmark'
    $output = Join-Path $run 'events.jsonl'
    if ($Bookmark) { [IO.File]::WriteAllText($state, $Bookmark, $utf8) }
    $config = Join-Path $run 'limpid.conf'
    $text = @"
control { socket "$($pipe.Replace('\', '\\'))" }
def input source { type windows_event_log channel "System" query "$Query" state_file "$($state.Replace('\', '/'))" }
def output captured { type file path "$($output.Replace('\', '/'))" }
def pipeline collect { input source output captured }
"@
    [IO.File]::WriteAllText($config, $text, $utf8)
    Invoke-Bounded $daemonExe @('--check', '--config', ('"' + $config + '"')) | Out-Null
    $daemon = Start-Process -FilePath $daemonExe -ArgumentList @('--config', ('"' + $config + '"')) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $run 'stdout.log') -RedirectStandardError (Join-Path $run 'stderr.log')
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        $ready = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($daemon.HasExited) { throw "Daemon exited; inspect $run" }
            try { Invoke-Bounded $ctlExe @('--socket', $pipe, 'health') | Out-Null; $ready = $true; break }
            catch { Start-Sleep -Milliseconds 100 }
        }
        if (-not $ready) { throw 'Daemon did not become ready within 10s.' }
        & $Verify $pipe $output $state $run
    } finally {
        # Cleanup only our foreground test process. This is NOT an SCM or
        # graceful-shutdown assertion; native service tests are separate.
        if (-not $daemon.HasExited) { Stop-Process -Id $daemon.Id -Force; $daemon.WaitForExit() }
    }
}
Run-Candidate '*[System[EventID=999999]]' '' {
    param($pipe, $output, $state, $run)
    $payload = Join-Path $run 'payload'
    [IO.File]::WriteAllText($payload, "native-pipe-to-file`n", $utf8)
    $reply = Invoke-Bounded $ctlExe @('--socket', $pipe, 'inject', 'input', 'source') $payload
    if (($reply | ConvertFrom-Json).injected -ne 1) { throw 'Injection count mismatch.' }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    $written = ''
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $output) { $written = [IO.File]::ReadAllText($output) }
        if ($written -ceq "native-pipe-to-file`n") { break }
        Start-Sleep -Milliseconds 20
    }
    if ($written -cne "native-pipe-to-file`n") { throw 'File output bytes differ.' }
    $portReservation = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $portReservation.Start()
    $port = $portReservation.LocalEndpoint.Port
    $portReservation.Stop()
    $exporter = Start-Process -FilePath (Join-Path $BinaryDirectory 'limpid-prometheus.exe') -ArgumentList @('--socket', $pipe, '--bind', "127.0.0.1:$port") -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $run 'exporter.stdout') -RedirectStandardError (Join-Path $run 'exporter.stderr')
    try {
        $response = $null
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        do {
            if ($exporter.HasExited) { throw 'Exporter exited before a successful scrape.' }
            try { $response = Invoke-WebRequest "http://127.0.0.1:$port/metrics" -TimeoutSec 6; break }
            catch { Start-Sleep -Milliseconds 50 }
        } while ([DateTime]::UtcNow -lt $deadline)
        if (-not $response -or $response.StatusCode -ne 200 -or -not $response.Content.Contains('captured')) { throw 'Exporter did not expose native daemon metrics.' }
    } finally {
        if (-not $exporter.HasExited) { Stop-Process -Id $exporter.Id -Force; $exporter.WaitForExit() }
    }
}
# Read existing events only. Results may contain local event data: keep this
# directory private and do not turn its contents into repository fixtures.
$xml = Invoke-Bounded "$env:SystemRoot/System32/wevtutil.exe" @('qe', 'System', '/c:2', '/rd:false', '/f:xml')
$events = ([xml]("<Events>$xml</Events>")).Events.Event
if ($events.Count -lt 2) { throw 'Native resume test requires two existing System events.' }
$first = [string]$events[0].System.EventRecordID
$second = [string]$events[1].System.EventRecordID
$bookmark = "<BookmarkList><Bookmark Channel='System' RecordId='$first' IsCurrent='true'/></BookmarkList>"
Run-Candidate "*[System[EventRecordID=$second]]" $bookmark {
    param($pipe, $output, $state, $run)
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 20
        $saved = ([xml][IO.File]::ReadAllText($state)).BookmarkList.Bookmark.RecordId
    } while ($saved -ne $second -and [DateTime]::UtcNow -lt $deadline)
    if ($saved -ne $second) { throw 'Pipeline ACK did not advance the bookmark.' }
    $records = @([IO.File]::ReadAllLines($output))
    if ($records.Count -ne 1) { throw 'Resume must emit exactly one matching event.' }
    if ([string](($records[0] | ConvertFrom-Json).EventRecordID) -ne $second) { throw 'Resumed record identity differs.' }
}
[IO.File]::WriteAllText((Join-Path $ResultsDirectory 'result.txt'), 'PASS: native control-to-file, exporter scrape, and Event Log resume/ACK checkpoint. SCM shutdown not tested.', $utf8)
Write-Output 'PASS: native control-to-file, exporter scrape, and Event Log resume/ACK checkpoint.'

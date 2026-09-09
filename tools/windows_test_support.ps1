# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
# Process/clock adapters let the runner's failure paths be tested without a desktop.

function Get-NativeTime {
    [Diagnostics.Stopwatch]::GetTimestamp() / [double][Diagnostics.Stopwatch]::Frequency
}

function Throw-NativeFailure([string]$Reason) {
    $failure = [InvalidOperationException]::new('Native test operation failed.')
    $failure.Data['StillusReason'] = $Reason
    throw $failure
}

function Get-NativeFailureReason($Failure) {
    $allowed = @('test/timeout', 'test/failed', 'window/timeout', 'state/timeout',
        'process/early/exit', 'process/exit/code', 'process/close/rejected', 'process/exit/timeout', 'process/cleanup',
        'state/mismatch', 'content/changed')
    $reason = $Failure.Exception.Data['StillusReason']
    if ($reason -in $allowed) { return $reason }
    return 'runner/error'
}

function Update-NativeProcessState($Process, $Record) {
    $Process.Refresh()
    $Record.processState = if ($Process.HasExited) { 'exited' } else { 'running' }
    $Record.windowState = 'unknown'
    $Record.responding = 'unknown'
    if (-not $Process.HasExited) {
        $Record.windowState = if ($Process.MainWindowHandle -eq [IntPtr]::Zero) { 'absent' } else { 'present' }
        if ($Record.windowState -eq 'present') {
            $Record.responding = $Process.Responding.ToString().ToLowerInvariant()
        }
    }
}

function Wait-NativeReady {
    param($Process, [scriptblock]$State, $Record,
        [scriptblock]$Now = { Get-NativeTime },
        [scriptblock]$Pause = { param($Milliseconds) Start-Sleep -Milliseconds $Milliseconds })
    $deadline = (& $Now) + 60
    $stableSince = $null
    $stableWindow = [IntPtr]::Zero
    while ($true) {
        Update-NativeProcessState $Process $Record
        if ($Process.HasExited) { Throw-NativeFailure 'process/early/exit' }
        $Record.stage = 'window'
        $ready = $Process.MainWindowHandle -ne [IntPtr]::Zero -and $Process.Responding
        if ($ready) { $Record.stage = 'state' }
        if ((& $Now) -ge $deadline) { Throw-NativeFailure ($Record.stage + '/timeout') }
        if ($ready -and (& $State)) {
            $nowSeconds = & $Now
            if ($nowSeconds -ge $deadline) { Throw-NativeFailure 'state/timeout' }
            # Require the same responsive window and saved state continuously;
            # the first settings write can precede the end of UI initialization.
            if ($null -eq $stableSince -or $stableWindow -ne $Process.MainWindowHandle) {
                $stableSince = $nowSeconds
                $stableWindow = $Process.MainWindowHandle
            }
            $Process.Refresh()
            if ($Process.HasExited) { Throw-NativeFailure 'process/early/exit' }
            if ($Process.MainWindowHandle -ne $stableWindow -or -not $Process.Responding) {
                $stableSince = $null
            } elseif ($nowSeconds - $stableSince -ge 0.5) { return }
        } else {
            $stableSince = $null
        }
        & $Pause 100
    }
}

function Close-NativeProcess {
    param($Process, $Record = @{},
        [scriptblock]$Now = { Get-NativeTime },
        [scriptblock]$Pause = { param($Milliseconds) Start-Sleep -Milliseconds $Milliseconds })
    $started = & $Now
    $deadline = $started + 30
    $Record.stage = 'close/request'
    $Record.closeAccepted = $false
    $Record.closeAttempts = 0
    while (-not $Record.closeAccepted) {
        Update-NativeProcessState $Process $Record
        if ($Process.HasExited) { Throw-NativeFailure 'process/early/exit' }
        if ((& $Now) -ge $started + 5) { Throw-NativeFailure 'process/close/rejected' }
        if ($Record.windowState -eq 'present' -and $Record.responding -eq 'true') {
            $Record.closeAttempts++
            $Record.closeAccepted = $Process.CloseMainWindow()
        }
        # Retry only a request that Windows did not accept. Once accepted, a
        # second close could hide a shutdown defect or dismiss another window.
        if (-not $Record.closeAccepted) { & $Pause 100 }
    }
    $Record.stage = 'close/wait'
    $remaining = [int][Math]::Max(0, [Math]::Ceiling(($deadline - (& $Now)) * 1000))
    $exited = $remaining -gt 0 -and $Process.WaitForExit($remaining)
    Update-NativeProcessState $Process $Record
    if (-not $exited) {
        Throw-NativeFailure 'process/exit/timeout'
    }
    $Record.exitCode = $Process.ExitCode
    if ($Process.ExitCode -ne 0) { Throw-NativeFailure 'process/exit/code' }
}

function Stop-OwnedNativeProcess($Process) {
    if ($null -eq $Process) { return }
    try {
        $Process.Refresh()
        if (-not $Process.HasExited) {
            try { $Process.Kill() } catch [InvalidOperationException] {
                $Process.Refresh()
                if (-not $Process.HasExited) { throw }
            }
            if (-not $Process.WaitForExit(10000)) { Throw-NativeFailure 'process/cleanup' }
        }
    } finally { $Process.Dispose() }
}

function ConvertTo-NativeComparisonPath([string]$Path) {
    if ($Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}

function Test-NativeSettings {
    param([string]$Path, [string[]]$ExternalPaths = @(), [string]$SelectedNote = '',
        [scriptblock]$Read = { param($Name) [IO.File]::ReadAllText($Name) })
    try { $text = & $Read $Path } catch [IO.IOException] { return $false }
    try { $settings = $text | ConvertFrom-Json -ErrorAction Stop } catch [ArgumentException] { return $false }
    if ($null -eq $settings -or $null -eq $settings.PSObject.Properties['version'] -or
        $settings.version -ne 1 -or $null -eq $settings.PSObject.Properties['window'] -or
        $null -eq $settings.PSObject.Properties['sidebar'] -or
        $null -eq $settings.PSObject.Properties['external_files'] -or
        $null -eq $settings.PSObject.Properties['selected_external']) { return $false }
    if ($SelectedNote -ne '' -and ($null -eq $settings.PSObject.Properties['selected_note'] -or
        $settings.selected_note -cne $SelectedNote)) { return $false }
    $files = @($settings.external_files)
    if ($files.Count -ne $ExternalPaths.Count) { return $false }
    for ($index = 0; $index -lt $files.Count; $index++) {
        $file = $files[$index]
        if ($null -eq $file -or $null -eq $file.PSObject.Properties['engine_id'] -or
            $null -eq $file.PSObject.Properties['absolute_path'] -or $file.engine_id -cne 'markdown' -or
            (ConvertTo-NativeComparisonPath $file.absolute_path) -ine $ExternalPaths[$index]) { return $false }
    }
    if ($ExternalPaths.Count -eq 0) { return $null -eq $settings.selected_external }
    return (ConvertTo-NativeComparisonPath $settings.selected_external) -ieq $ExternalPaths[0]
}

function Invoke-NativeSmoke {
    param([string]$Application, [string[]]$Arguments, [scriptblock]$State, $Record,
        [string]$Log,
        [scriptblock]$Start = { param($Executable, $Arguments, $OutputPath)
            Start-Process -FilePath $Executable -ArgumentList $Arguments -PassThru -RedirectStandardOutput $OutputPath -RedirectStandardError ($OutputPath + '.stderr')
        },
        [scriptblock]$CollectDiagnostics = { param($OutputPath, $Entry) },
        [scriptblock]$Now = { Get-NativeTime },
        [scriptblock]$Pause = { param($Milliseconds) Start-Sleep -Milliseconds $Milliseconds })
    $process = $null
    $started = & $Now
    $Record.stage = 'start'
    $Record.reason = 'none'
    $Record.closeAccepted = $false
    $Record.closeAttempts = 0
    $Record.processState = 'unknown'
    $Record.windowState = 'unknown'
    $Record.responding = 'unknown'
    try {
        $process = & $Start $Application $Arguments $Log
        # Cache the owned handle so ExitCode remains available after termination.
        $null = $process.Handle
        Wait-NativeReady -Process $process -State $State -Record $Record -Now $Now -Pause $Pause
        Close-NativeProcess -Process $process -Record $Record -Now $Now -Pause $Pause
        $Record.stage = 'verify'
        if (-not (& $State)) { Throw-NativeFailure 'state/mismatch' }
        $Record.stage = 'complete'
    } catch {
        $Record.reason = Get-NativeFailureReason $_
        throw
    } finally {
        try { Stop-OwnedNativeProcess $process } catch {
            $Record.cleanup = 'failed'
            if ($Record.reason -eq 'none') {
                $Record.reason = 'process/cleanup'
                throw
            }
        } finally {
            $Record.durationMs = [long](([Math]::Max(0, (& $Now) - $started)) * 1000)
            try { & $CollectDiagnostics $Log $Record } catch {
                $Record.diagnosticsError = 'collection/failed'
                if ($Record.reason -eq 'none') {
                    $Record.reason = 'runner/error'
                    throw
                }
            }
        }
    }
}

function Invoke-NativeTest {
    param([string]$Executable, [string]$Log,
        [scriptblock]$Start = { param($Name, $OutputPath)
            Start-Process -FilePath $Name -ArgumentList '--test-threads=1' -PassThru -NoNewWindow -RedirectStandardOutput $OutputPath -RedirectStandardError ($OutputPath + '.stderr')
        })
    $process = $null
    $started = Get-NativeTime
    $record = [ordered]@{ executable = [IO.Path]::GetFileName($Executable)
        exitCode = $null; stage = 'rust'; reason = 'none'; durationMs = 0 }
    try {
        $process = & $Start $Executable $Log
        $null = $process.Handle
        if (-not $process.WaitForExit(600000)) {
            $record.reason = 'test/timeout'
        } else {
            $record.exitCode = $process.ExitCode
            if ($process.ExitCode -ne 0) { $record.reason = 'test/failed' }
        }
    } catch {
        $record.reason = Get-NativeFailureReason $_
    } finally {
        try { Stop-OwnedNativeProcess $process } catch {
            $record.cleanup = 'failed'
            if ($record.reason -eq 'none') { $record.reason = 'process/cleanup' }
        }
        $record.durationMs = [long](([Math]::Max(0, (Get-NativeTime) - $started)) * 1000)
    }
    return $record
}

function Invoke-NativeTestSuite {
    param([string[]]$Executables, [string]$Directory, [string]$LogDirectory, $Report,
        [scriptblock]$AfterEach, [scriptblock]$Run = {
            param($Executable, $Log) Invoke-NativeTest -Executable $Executable -Log $Log
        })
    foreach ($name in $Executables) {
        if ([IO.Path]::GetFileName($name) -ne $name -or $name -notmatch '^[A-Za-z0-9_]+-[a-f0-9]+\.exe$') {
            throw 'Invalid test executable name.'
        }
        $log = Join-Path $LogDirectory ($name + '.log')
        $entry = & $Run (Join-Path $Directory $name) $log
        # Retain the result even if processing its diagnostics subsequently fails.
        $Report.tests += $entry
        & $AfterEach $entry $log
    }
}

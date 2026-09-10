# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
# Standalone, dependency-free tests. Also run before the Windows native test kit.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows_test_support.ps1')

function Assert-True($Condition, [string]$Name) {
    if (-not $Condition) { throw "Windows runner regression: $Name" }
}

function Assert-Failure([scriptblock]$Action, [string]$Reason) {
    $caught = $false
    try { & $Action } catch {
        $caught = $true
        Assert-True ((Get-NativeFailureReason $_) -eq $Reason) "expected $Reason"
    }
    Assert-True $caught "missing $Reason"
}

function New-FakeProcess {
    $fake = [pscustomobject]@{
        HasExited = $false; Handle = [IntPtr]1; MainWindowHandle = [IntPtr]1
        Responding = $true; ExitCode = 0; Hung = $false; CloseAccepted = $true
        Closed = 0; Killed = 0; Disposed = 0; Waits = @(); Clock = $null
    }
    $fake | Add-Member ScriptMethod Refresh {}
    $fake | Add-Member ScriptMethod CloseMainWindow { $this.Closed++; return $this.CloseAccepted }
    $fake | Add-Member ScriptMethod WaitForExit {
        param($Milliseconds)
        $this.Waits += $Milliseconds
        if ($this.Hung -and $this.Killed -eq 0) {
            if ($null -ne $this.Clock) { $this.Clock.seconds += $Milliseconds / 1000.0 }
            return $false
        }
        $this.HasExited = $true
        return $true
    }
    $fake | Add-Member ScriptMethod Kill { $this.Killed++ }
    $fake | Add-Member ScriptMethod Dispose { $this.Disposed++ }
    return $fake
}

# A delayed window, temporarily unavailable file, invalid JSON, then the right state.
$clock = @{ seconds = 0.0 }
$fake = New-FakeProcess
$fake.MainWindowHandle = [IntPtr]::Zero
$record = @{}
$valid = '{"version":1,"window":{},"sidebar":{},"external_files":[],"selected_external":null}'
$read = {
    param($Path)
    if ($clock.seconds -lt 15) { throw [IO.FileNotFoundException]::new('synthetic') }
    if ($clock.seconds -lt 20) { throw [IO.IOException]::new('sharing violation') }
    if ($clock.seconds -lt 25) { return '{' }
    return $valid
}
Wait-NativeReady -Process $fake -Record $record -Now { $clock.seconds } -Pause {
    param($Milliseconds)
    Assert-True ($Milliseconds -eq 100) 'poll interval'
    $clock.seconds += 0.1
    if ($clock.seconds -ge 10) { $fake.MainWindowHandle = [IntPtr]1 }
} -State { Test-NativeSettings -Path 'fixture' -Read $read }
Assert-True ($clock.seconds -ge 25 -and $clock.seconds -lt 26) 'delayed state accepted'
Assert-True ($record.stage -eq 'state') 'readiness phase'

# A briefly ready window/state must not start shutdown. Changing the main
# window handle also restarts the quiet period, without extending the deadline.
foreach ($interruption in @('window', 'responsive', 'state', 'handle')) {
    $clock.seconds = 0
    $fake = New-FakeProcess
    Wait-NativeReady -Process $fake -Record @{} -Now { $clock.seconds } -Pause {
        param($Milliseconds)
        $clock.seconds += 0.1
        $interrupted = $clock.seconds -ge 0.2 -and $clock.seconds -lt 0.4
        if ($interruption -eq 'window') { $fake.MainWindowHandle = if ($interrupted) { [IntPtr]::Zero } else { [IntPtr]1 } }
        if ($interruption -eq 'responsive') { $fake.Responding = -not $interrupted }
        if ($interruption -eq 'handle' -and $clock.seconds -ge 0.4) { $fake.MainWindowHandle = [IntPtr]2 }
    } -State { -not ($interruption -eq 'state' -and $clock.seconds -ge 0.2 -and $clock.seconds -lt 0.4) }
    Assert-True ($clock.seconds -ge 0.9 -and $clock.seconds -lt 1.1) 'continuous readiness required'
}

foreach ($kind in @('window', 'state')) {
    $clock.seconds = 0
    $fake = New-FakeProcess
    if ($kind -eq 'window') { $fake.Responding = $false }
    Assert-Failure {
        Wait-NativeReady -Process $fake -Record @{} -Now { $clock.seconds } -Pause {
            param($Milliseconds) $clock.seconds += 0.1
        } -State { $false }
    } ($kind + '/timeout')
    Assert-True ($clock.seconds -ge 60 -and $clock.seconds -lt 61) 'bounded timeout'
}

# Repeated short ready intervals never extend the 60-second deadline.
$clock.seconds = 0
$fake = New-FakeProcess
Assert-Failure {
    Wait-NativeReady -Process $fake -Record @{} -Now { $clock.seconds } -Pause {
        param($Milliseconds) $clock.seconds += 0.1
    } -State { $clock.seconds % 0.4 -lt 0.2 }
} 'state/timeout'
Assert-True ($clock.seconds -ge 60 -and $clock.seconds -lt 60.2) 'unstable readiness deadline'

# Readiness appearing after the deadline cannot turn a timeout into a pass.
$clock.seconds = 0
$fake = New-FakeProcess
Assert-Failure {
    Wait-NativeReady -Process $fake -Record @{} -Now { $clock.seconds } -State {
        $clock.seconds = 61
        return $true
    }
} 'state/timeout'

$clock.seconds = 0
$fake = New-FakeProcess
Assert-Failure {
    Wait-NativeReady -Process $fake -Record @{} -Now { $clock.seconds } -Pause {
        param($Milliseconds)
        $clock.seconds += 0.1
        $fake.HasExited = $true
    } -State { $false }
} 'process/early/exit'
Assert-True ($clock.seconds -lt 1) 'early exit is immediate'

$expected = @('C:\fixture 日本語\one.MD', 'C:\fixture 日本語\two.txt')
$settings = @{
    version = 1; window = @{}; sidebar = @{}; selected_external = '\\?\' + $expected[0]
    external_files = @(
        @{ engine_id = 'markdown'; absolute_path = '\\?\' + $expected[0] }
        @{ engine_id = 'markdown'; absolute_path = $expected[1] }
    )
}
$read = { param($Path) $settings | ConvertTo-Json -Depth 5 }
Assert-True (Test-NativeSettings -Path 'fixture' -ExternalPaths $expected -Read $read) 'ordered paths'
$settings.external_files = @($settings.external_files[1], $settings.external_files[0])
Assert-True (-not (Test-NativeSettings -Path 'fixture' -ExternalPaths $expected -Read $read)) 'wrong order'
$settings.external_files = @($settings.external_files[1], $settings.external_files[0])
$settings.selected_external = $expected[1]
Assert-True (-not (Test-NativeSettings -Path 'fixture' -ExternalPaths $expected -Read $read)) 'wrong selection'
Assert-True (-not (Test-NativeSettings -Path 'fixture' -Read { '{}' })) 'missing schema'
Assert-True (-not (Test-NativeSettings -Path 'fixture' -SelectedNote 'notes/Ready.md' -Read { $valid })) 'missing selected note'
$selected = '{"version":1,"window":{},"sidebar":{},"external_files":[],"selected_external":null,"selected_note":"notes/Ready.md"}'
Assert-True (Test-NativeSettings -Path 'fixture' -SelectedNote 'notes/Ready.md' -Read { $selected }) 'startup selection'
$observation = [ordered]@{ scenario = 'startup' }
Assert-True (Test-NativeSettings -Path 'fixture' -SelectedNote 'notes/Ready.md' -Read { $selected } -Record $observation) 'observed startup selection'
Assert-True ($observation.settingsState -eq 'ready') 'ready state observation'
foreach ($case in @(
    @{ read = { throw [IO.FileNotFoundException]::new('SYNTHETIC_SECRET') }; state = 'missing' },
    @{ read = { throw [IO.DirectoryNotFoundException]::new('SYNTHETIC_SECRET') }; state = 'missing' },
    @{ read = { throw [IO.IOException]::new('SYNTHETIC_SECRET') }; state = 'read' },
    @{ read = { '{' }; state = 'json' },
    @{ read = { '{}' }; state = 'schema' },
    @{ read = { $valid }; state = 'selected_note' }
)) {
    $observation = @{}
    Assert-True (-not (Test-NativeSettings -Path 'fixture' -SelectedNote 'notes/Ready.md' -Read $case.read -Record $observation)) 'failed settings observation'
    Assert-True ($observation.settingsState -eq $case.state) 'exact failed settings condition'
    Assert-True (($observation | ConvertTo-Json -Compress) -notmatch 'SYNTHETIC_SECRET|notes/|fixture') 'observation contains no paths or data'
}
Assert-Failure { Test-NativeSettings -Path 'fixture' -Read { throw 'SYNTHETIC_SECRET' } } 'runner/error'

$clock.seconds = 0
$fake = New-FakeProcess
$record = @{}
Invoke-NativeSmoke -Application 'fixture' -Arguments @('fixture') -Record $record -State { $true } -Start { $fake } -Now { $clock.seconds } -Pause {
    param($Milliseconds) $clock.seconds += $Milliseconds / 1000.0
}
Assert-True ($fake.Closed -eq 1 -and $fake.Killed -eq 0 -and $fake.Disposed -eq 1) 'graceful close'
Assert-True ($fake.Waits[0] -eq 30000 -and $record.reason -eq 'none') 'close deadline'
Assert-True ($record.closeAccepted -and $record.closeAttempts -eq 1 -and $record.processState -eq 'exited') 'successful close diagnostics'
Assert-True ($record.stage -eq 'complete' -and $record.durationMs -ge 0) 'smoke report'

$fake = New-FakeProcess
$fake.Hung = $true
$record = @{}
Assert-Failure {
    Invoke-NativeSmoke -Application 'fixture' -Arguments @('fixture') -Record $record -State { $true } -Start { $fake }
} 'process/exit/timeout'
Assert-True ($fake.Killed -eq 1 -and $fake.Disposed -eq 1) 'hung owned process killed and disposed'
Assert-True ($fake.Waits[-1] -eq 10000) 'wait after kill'
Assert-True ($record.reason -eq 'process/exit/timeout' -and $record.stage -eq 'close/wait') 'close failure retained'
Assert-True ($fake.Closed -eq 1 -and $record.processState -eq 'running') 'accepted close is never retried and pre-kill state is retained'

# Rejected requests may settle, but the exit wait uses only the remainder of
# the original 30 seconds. Once accepted, even a hang gets exactly one request.
$clock.seconds = 0
$fake = New-FakeProcess
$fake.Clock = $clock
$fake.Hung = $true
$fake | Add-Member -Force ScriptMethod CloseMainWindow { $this.Closed++; return $this.Closed -ge 4 }
$record = @{}
Assert-Failure {
    Close-NativeProcess -Process $fake -Record $record -Now { $clock.seconds } -Pause { param($Milliseconds) $clock.seconds += $Milliseconds / 1000.0 }
} 'process/exit/timeout'
Assert-True ($fake.Closed -eq 4 -and $record.closeAccepted) 'only rejected requests are retried'
Assert-True ($fake.Waits.Count -eq 1 -and $fake.Waits[0] -le 29700 -and $clock.seconds -le 30.001) 'one shared close deadline'

# An accepted request that itself overruns the deadline cannot start another wait.
$clock.seconds = 0
$fake = New-FakeProcess
$fake.Clock = $clock
$fake | Add-Member -Force ScriptMethod CloseMainWindow { $this.Closed++; $this.Clock.seconds = 31; return $true }
$record = @{}
Assert-Failure { Close-NativeProcess -Process $fake -Record $record -Now { $clock.seconds } } 'process/exit/timeout'
Assert-True ($record.closeAccepted -and $fake.Closed -eq 1 -and $fake.Waits.Count -eq 0) 'late accepted request cannot extend the deadline'

# A vanished window with a surviving process remains an exit failure.
$fake = New-FakeProcess
$fake | Add-Member -Force ScriptMethod WaitForExit { param($Milliseconds) $this.MainWindowHandle = [IntPtr]::Zero; return $false }
$record = @{}
Assert-Failure { Close-NativeProcess -Process $fake -Record $record } 'process/exit/timeout'
Assert-True ($record.windowState -eq 'absent' -and $record.responding -eq 'unknown' -and $record.processState -eq 'running') 'window disappearance is not process exit'

foreach ($interruption in @('window', 'responsive', 'rejected')) {
    $clock.seconds = 0
    $fake = New-FakeProcess
    if ($interruption -eq 'window') { $fake.MainWindowHandle = [IntPtr]::Zero }
    if ($interruption -eq 'responsive') { $fake.Responding = $false }
    if ($interruption -eq 'rejected') { $fake.CloseAccepted = $false }
    $record = @{}
    Close-NativeProcess -Process $fake -Record $record -Now { $clock.seconds } -Pause {
        param($Milliseconds)
        $clock.seconds += $Milliseconds / 1000.0
        if ($clock.seconds -ge 0.4) {
            $fake.MainWindowHandle = [IntPtr]1
            $fake.Responding = $true
            $fake.CloseAccepted = $true
        }
    }
    Assert-True ($record.closeAccepted -and $record.exitCode -eq 0 -and $clock.seconds -ge 0.4) 'transient close rejection settles'
}

$clock.seconds = 0
$fake = New-FakeProcess
$fake.CloseAccepted = $false
$record = @{}
Assert-Failure {
    Close-NativeProcess -Process $fake -Record $record -Now { $clock.seconds } -Pause { param($Milliseconds) $clock.seconds += $Milliseconds / 1000.0 }
} 'process/close/rejected'
Assert-True (-not $record.closeAccepted -and $record.stage -eq 'close/request') 'rejected close is distinct from exit timeout'
Assert-True ($clock.seconds -ge 5 -and $clock.seconds -lt 5.1 -and $fake.Waits.Count -eq 0) 'rejection wait is bounded'

$clock.seconds = 0
$fake = New-FakeProcess
$fake.CloseAccepted = $false
Assert-Failure {
    Close-NativeProcess -Process $fake -Now { $clock.seconds } -Pause {
        param($Milliseconds)
        $clock.seconds += $Milliseconds / 1000.0
        $fake.HasExited = $true
    }
} 'process/early/exit'

# Diagnostic/cleanup work cannot erase the primary failure or replace the
# pre-cleanup process state with the result of our forced termination.
$clock.seconds = 0
$fake = New-FakeProcess
$fake.Clock = $clock
$fake.Hung = $true
$record = @{}
Assert-Failure {
    Invoke-NativeSmoke -Application 'fixture' -Arguments @('fixture') -Record $record -Log 'fixture.log' -State { $true } -Start { $fake } -Now { $clock.seconds } -Pause {
        param($Milliseconds) $clock.seconds += $Milliseconds / 1000.0
    } -CollectDiagnostics {
        param($OutputPath, $Entry)
        Assert-True ($OutputPath -eq 'fixture.log' -and $fake.Killed -eq 1) 'collect after process cleanup'
        $Entry.diagnostics = @('NATIVE_LIFECYCLE stage=WindowClosed')
        throw 'SYNTHETIC_SECRET'
    }
} 'process/exit/timeout'
Assert-True ($record.diagnosticsError -eq 'collection/failed' -and $record.diagnostics.Count -eq 1) 'diagnostic collection failure is explicit'
Assert-True ($record.processState -eq 'running' -and $fake.HasExited) 'snapshot precedes forced cleanup'

$fake = New-FakeProcess
$record = @{}
Assert-Failure {
    Invoke-NativeSmoke -Application 'fixture' -Arguments @('fixture') -Record $record -State { $true } -Start { $fake } -CollectDiagnostics {
        param($OutputPath, $Entry) throw 'SYNTHETIC_SECRET'
    }
} 'runner/error'

$fake = New-FakeProcess
$record = @{}
Assert-Failure {
    Invoke-NativeSmoke -Application 'fixture' -Arguments @('fixture') -Record $record -State { -not $fake.HasExited } -Start { $fake }
} 'state/mismatch'

$fake = New-FakeProcess
$fake.ExitCode = 42
$record = @{}
Assert-Failure { Close-NativeProcess $fake $record } 'process/exit/code'
Assert-True ($record.exitCode -eq 42) 'nonzero close exit code is retained'

$fake = New-FakeProcess
$fake.HasExited = $true
Stop-OwnedNativeProcess $fake
Assert-True ($fake.Killed -eq 0 -and $fake.Disposed -eq 1) 'exited process is not killed'

foreach ($kind in @('passed', 'failed', 'timeout')) {
    $fake = New-FakeProcess
    if ($kind -eq 'failed') { $fake.ExitCode = 101 }
    if ($kind -eq 'timeout') { $fake.Hung = $true }
    $result = Invoke-NativeTest -Executable 'fixture-a.exe' -Log 'fixture.log' -Start { $fake }
    $reason = @{ passed = 'none'; failed = 'test/failed'; timeout = 'test/timeout' }[$kind]
    Assert-True ($result.reason -eq $reason) "test result $kind"
    Assert-True ($fake.Waits[0] -eq 600000 -and $fake.Disposed -eq 1) 'test deadline'
    if ($kind -eq 'timeout') {
        Assert-True ($fake.Killed -eq 1 -and $null -eq $result.exitCode) 'timeout has no invented exit code'
    }
}

$suite = @{ tests = @() }
$observed = [Collections.Generic.List[string]]::new()
Invoke-NativeTestSuite -Executables @('fixture-a.exe', 'fixture-b.exe', 'fixture-c.exe') -Directory $PSScriptRoot -LogDirectory $PSScriptRoot -Report $suite -Run {
    param($Executable, $Log)
    $name = [IO.Path]::GetFileName($Executable)
    $reason = @{ 'fixture-a.exe' = 'test/failed'; 'fixture-b.exe' = 'test/timeout'; 'fixture-c.exe' = 'none' }[$name]
    return @{ executable = $name; reason = $reason }
} -AfterEach {
    param($Entry, $Log)
    $observed.Add($Entry.executable)
}
Assert-True ($suite.tests.Count -eq 3 -and $observed.Count -eq 3) 'continue after failure and timeout'
Assert-True ($suite.tests[2].reason -eq 'none') 'later success is retained'

# Exercise real process handles and redirected streams as well as the fake clock.
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('stillus-runner-selftest-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    $program = Join-Path $temporary 'fixture.ps1'
    [IO.File]::WriteAllText($program, "[Console]::Out.WriteLine('fixture stdout'); [Console]::Error.WriteLine('fixture stderr'); exit 23")
    $shell = [Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    $output = Join-Path $temporary 'output.log'
    $result = Invoke-NativeTest -Executable 'fixture-a.exe' -Log $output -Start {
        param($Name, $OutputPath)
        Start-Process -FilePath $shell -ArgumentList @('-NoProfile', '-File', ('"' + $program + '"')) -PassThru -NoNewWindow -RedirectStandardOutput $OutputPath -RedirectStandardError ($OutputPath + '.stderr')
    }
    Assert-True ($result.exitCode -eq 23 -and $result.reason -eq 'test/failed') 'real exit code'
    Assert-True ([IO.File]::ReadAllText($output).Contains('fixture stdout')) 'stdout collected'
    Assert-True ([IO.File]::ReadAllText($output + '.stderr').Contains('fixture stderr')) 'stderr collected'
} finally { Remove-Item -LiteralPath $temporary -Recurse -Force }

try { throw 'SYNTHETIC_SECRET' } catch {
    Assert-True ((Get-NativeFailureReason $_) -eq 'runner/error') 'exception payload is excluded'
}
Write-Output 'Windows runner behavior tests passed.'

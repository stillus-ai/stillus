# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
# Run with: powershell -NoProfile -ExecutionPolicy Bypass -File .\Run-Tests.ps1
[CmdletBinding()]
param([switch]$Interactive, [switch]$CI, [string]$ReportDirectory = $PSScriptRoot)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'windows_test_support.ps1')
if (-not [Environment]::Is64BitOperatingSystem -or [Environment]::OSVersion.Version.Build -lt 10240) {
    throw 'Windows 10/11 x64 is required.'
}
$drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot([IO.Path]::GetTempPath()))
if ($drive.DriveType -ne [IO.DriveType]::Fixed -or $drive.DriveFormat -ne 'NTFS') {
    throw 'The test temporary directory must be on local NTFS.'
}
$root = Join-Path ([IO.Path]::GetTempPath()) ('Stillus Windows 日本語 ' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$previous = @{}
foreach ($name in @('TEMP', 'TMP', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'STILLUS_TEST_JUNCTION', 'STILLUS_NATIVE_DIAGNOSTICS')) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$report = [ordered]@{
    platform = 'Windows'; build = [Environment]::OSVersion.Version.ToString()
    architecture = $env:PROCESSOR_ARCHITECTURE; filesystem = $drive.DriveFormat
    started = [DateTime]::UtcNow.ToString('o'); tests = @(); status = 'running'
    interactive = 'not performed'; temporaryWorkspace = $root; smokeChecks = @(); reason = 'none'
}
if ($CI) {
    $report.Remove('temporaryWorkspace')
    $report.sourceRevision = $env:SOURCE_REVISION
}
function Save-WindowsReport {
    New-Item -ItemType Directory -Path $ReportDirectory -Force | Out-Null
    $report | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $ReportDirectory 'windows-results.json')
}
$collectSmokeDiagnostics = {
    param($OutputPath, $Entry)
    $logs = @($OutputPath, ($OutputPath + '.stderr')) | Where-Object { Test-Path -LiteralPath $_ }
    $Entry.diagnostics = @()
    if ($CI -and @($logs).Count -ne 0) {
        $safeReport = & python (Join-Path $PSScriptRoot 'ci_diagnostics.py') @logs
        if ($LASTEXITCODE -ne 0) { throw 'Could not sanitize native smoke diagnostics.' }
        $Entry.diagnostics = @(($safeReport | ConvertFrom-Json).diagnostics)
        $Entry.diagnostics | Write-Output
    } elseif (-not $CI) {
        $Entry.log = $OutputPath
        $logs | ForEach-Object { Get-Content -LiteralPath $_ }
    }
}
try {
    $report.phase = 'runner self tests'
    & (Join-Path $PSScriptRoot 'test_windows_support.ps1')
    $env:TEMP = $root
    $env:TMP = $root
    $env:USERPROFILE = $root
    $env:HOMEDRIVE = [IO.Path]::GetPathRoot($root).TrimEnd('\')
    $env:HOMEPATH = $root.Substring($env:HOMEDRIVE.Length)
    $junctionTarget = Join-Path $root 'junction target'
    New-Item -ItemType Directory -Path $junctionTarget | Out-Null
    $env:STILLUS_TEST_JUNCTION = Join-Path $root 'junction'
    New-Item -ItemType Junction -Path $env:STILLUS_TEST_JUNCTION -Target $junctionTarget | Out-Null
    $report.phase = 'rust tests'
    $executables = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'tests.json') | ConvertFrom-Json
    Invoke-NativeTestSuite -Executables $executables -Directory $PSScriptRoot -LogDirectory $root -Report $report -AfterEach {
        param($entry, $log)
        $logs = @($log, ($log + '.stderr')) | Where-Object { Test-Path -LiteralPath $_ }
        if ($CI) {
            $entry.failedTests = @()
            $entry.diagnostics = @()
            if (@($logs).Count -ne 0) {
                $safeReport = & python (Join-Path $PSScriptRoot 'ci_diagnostics.py') @logs
                if ($LASTEXITCODE -ne 0) { throw 'Could not sanitize Rust test diagnostics.' }
                $safeReport = $safeReport | ConvertFrom-Json
                $entry.failedTests = @($safeReport.failedTests)
                $entry.diagnostics = @($safeReport.diagnostics)
                $safeReport.diagnostics | Write-Output
            }
        } else {
            $entry.log = $log
            $logs | ForEach-Object { Get-Content -LiteralPath $_ }
        }
        Write-Output "NATIVE_RUNNER stage=rust reason=$($entry.reason) duration_ms=$($entry.durationMs)"
        Save-WindowsReport
    }
    if (@($report.tests | Where-Object { $_.reason -ne 'none' }).Count -ne 0) {
        Throw-NativeFailure 'test/failed'
    }
    $report.phase = 'native startup'
    $env:STILLUS_NATIVE_DIAGNOSTICS = '1'
    $application = Join-Path (Split-Path -Parent $PSScriptRoot) 'Stillus.exe'
    $report.applicationSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $application).Hash
    $report.applicationVersion = (Get-Item -LiteralPath $application).VersionInfo.FileVersion
    $workspace = Join-Path $root 'Workspace with spaces 日本語'
    New-Item -ItemType Directory -Path (Join-Path $workspace 'notes') -Force | Out-Null
    if (-not (Test-Path -LiteralPath $application)) { throw 'Stillus.exe is missing beside the test package.' }
    # A selection change guarantees settings are written: an empty workspace can
    # legitimately keep defaults in memory without creating settings.json.
    $startupNote = Join-Path $workspace 'notes/Ready.md'
    $startupBody = "---`ntitle: Ready`n---`n# Ready`n"
    [IO.File]::WriteAllText($startupNote, $startupBody, [Text.UTF8Encoding]::new($false))
    $settingsPath = Join-Path $workspace '.stillus/settings.json'
    $startupCheck = [ordered]@{ scenario = 'startup' }
    $report.smokeChecks += $startupCheck
    Invoke-NativeSmoke -Application $application -Arguments @(('"' + $workspace + '"')) -Record $startupCheck -Log (Join-Path $root 'startup.log') -CollectDiagnostics $collectSmokeDiagnostics -State {
        Test-NativeSettings -Path $settingsPath -SelectedNote 'notes/Ready.md'
    }
    if ([IO.File]::ReadAllText($startupNote) -cne $startupBody) {
        $startupCheck.stage = 'verify'
        $startupCheck.reason = 'content/changed'
        Throw-NativeFailure 'content/changed'
    }
    Save-WindowsReport
    $report.phase = 'external file launch'
    $external = Join-Path $root 'External 日本語 #1.MD'
    $second = Join-Path $root 'External two.txt'
    [IO.File]::WriteAllText($external, "External unchanged`n", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($second, "Second unchanged`n", [Text.UTF8Encoding]::new($false))
    $arguments = @('--workspace', ('"' + $workspace + '"'), '--open', ('"' + $external + '"'), ('"' + $second + '"'))
    $externalCheck = [ordered]@{ scenario = 'external' }
    $report.smokeChecks += $externalCheck
    Invoke-NativeSmoke -Application $application -Arguments $arguments -Record $externalCheck -Log (Join-Path $root 'external.log') -CollectDiagnostics $collectSmokeDiagnostics -State {
        Test-NativeSettings -Path $settingsPath -ExternalPaths @($external, $second)
    }
    if ([IO.File]::ReadAllText($external) -cne "External unchanged`n" -or
        [IO.File]::ReadAllText($second) -cne "Second unchanged`n") {
        $externalCheck.stage = 'verify'
        $externalCheck.reason = 'content/changed'
        Throw-NativeFailure 'content/changed'
    }
    $report.externalLaunch = 'passed'
    $report.phase = 'Open With registration'
    $registration = Join-Path (Split-Path -Parent $PSScriptRoot) 'Register.ps1'
    $testRegistry = 'Software\StillusTests\' + [Guid]::NewGuid().ToString('N')
    try {
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($testRegistry + '\.txt')
        $key.SetValue('', 'Other.TextEditor')
        $key.Dispose()
        & $registration -RegistryRoot $testRegistry
        & $registration -RegistryRoot $testRegistry
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($testRegistry + '\Stillus.Document\shell\open\command')
        if ($key.GetValue('') -ne ('"' + $application + '" --open "%1"')) { throw 'Invalid Open With command.' }
        $key.Dispose()
        & $registration -RegistryRoot $testRegistry -Remove
        & $registration -RegistryRoot $testRegistry -Remove
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($testRegistry + '\.txt')
        if ($key.GetValue('') -ne 'Other.TextEditor') { throw 'Registration changed a default association.' }
        $key.Dispose()
        $report.registration = 'passed in isolated registry subtree'
    } finally {
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($testRegistry, $false)
    }
    $report.nativeSmoke = 'passed'
    if ($Interactive) {
        Write-Host 'Complete the Windows UI checklist in docs/windows.md. This script does not mark manual checks as passed.'
        Start-Process -FilePath $application -ArgumentList ('"' + $workspace + '"') -Wait
        $report.interactive = 'opened; checklist results must be recorded separately'
    }
    $report.status = 'automated tests passed'
} catch {
    $report.status = 'failed'
    $report.reason = Get-NativeFailureReason $_
    $report.error = if ($CI) { "Native test kit failed: phase=$($report.phase), reason=$($report.reason)." } else { $_.Exception.Message }
    throw
} finally {
    $report.finished = [DateTime]::UtcNow.ToString('o')
    foreach ($check in $report.smokeChecks) {
        Write-Output "NATIVE_RUNNER stage=$($check.stage) reason=$($check.reason) duration_ms=$($check.durationMs)"
        Write-Output "NATIVE_WINDOW scenario=$($check.scenario) process=$($check.processState) window=$($check.windowState) responding=$($check.responding) close_accepted=$($check.closeAccepted.ToString().ToLowerInvariant()) close_attempts=$($check.closeAttempts)"
    }
    Save-WindowsReport
    foreach ($name in $previous.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
    Write-Host "Results recorded."
    Write-Host "Test workspace and logs retained: $root"
}

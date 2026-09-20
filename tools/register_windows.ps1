# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
param(
    [switch]$Remove,
    [string]$RegistryRoot = 'Software\Classes'
)
$ErrorActionPreference = 'Stop'
$exe = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'Stillus.exe'))
$command = '"' + $exe + '" --open "%1"'
$progId = 'Stillus.Document'
$extensions = @(
    '.md', '.markdown', '.txt', '.text', '.log', '.json', '.jsonc', '.jsonl', '.ndjson', '.csv', '.tsv', '.yaml',
    '.yml', '.toml', '.ini', '.cfg', '.conf', '.config', '.env', '.properties', '.xml', '.xsd', '.xsl', '.xslt',
    '.svg', '.html', '.htm', '.xhtml', '.css', '.scss', '.sass', '.less', '.js', '.mjs', '.cjs', '.jsx',
    '.ts', '.mts', '.cts', '.tsx', '.php', '.phtml', '.py', '.pyw', '.rb', '.rs', '.go', '.c',
    '.h', '.cc', '.cpp', '.cxx', '.hpp', '.cs', '.java', '.kt', '.kts', '.swift', '.sh', '.bash',
    '.zsh', '.fish', '.ps1', '.bat', '.cmd', '.sql', '.lua', '.pl', '.pm', '.r', '.vue', '.svelte',
    '.astro', '.graphql', '.gql', '.proto', '.tex', '.bib', '.rst', '.adoc', '.org', '.diff', '.patch'
)
$root = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($RegistryRoot)
try {
    $existingDocument = $root.OpenSubKey($progId)
    $exists = $null -ne $existingDocument
    $owned = $exists -and $existingDocument.GetValue('StillusOwner') -eq 'Stillus'
    if ($existingDocument) { $existingDocument.Dispose() }
    $existing = $root.OpenSubKey($progId + '\shell\open\command')
    $previous = if ($existing) { $existing.GetValue('') } else { $null }
    if ($existing) { $existing.Dispose() }
    if ($Remove) {
        # A moved/older package must not unregister the currently registered copy.
        if (-not $owned -or $previous -ne $command) { return }
        foreach ($extension in $extensions) {
            $key = $root.OpenSubKey($extension + '\OpenWithProgids', $true)
            if ($key) {
                try { $key.DeleteValue($progId, $false) } finally { $key.Dispose() }
            }
        }
        $root.DeleteSubKeyTree($progId, $false)
    } else {
        if (-not [IO.File]::Exists($exe)) { throw "Executable is missing: $exe" }
        if ($exists -and -not $owned) { throw 'Refusing to replace an unrelated ProgID.' }
        $document = $root.CreateSubKey($progId)
        try {
            $document.SetValue('', 'Stillus text document')
            $document.SetValue('StillusOwner', 'Stillus')
            $open = $document.CreateSubKey('shell\open\command')
            try { $open.SetValue('', $command, [Microsoft.Win32.RegistryValueKind]::String) } finally { $open.Dispose() }
            $icon = $document.CreateSubKey('DefaultIcon')
            try { $icon.SetValue('', '"' + $exe + '",0') } finally { $icon.Dispose() }
        } finally { $document.Dispose() }
        foreach ($extension in $extensions) {
            $key = $root.CreateSubKey($extension + '\OpenWithProgids')
            try { $key.SetValue($progId, [byte[]]@(), [Microsoft.Win32.RegistryValueKind]::None) } finally { $key.Dispose() }
        }
    }
} finally { $root.Dispose() }
Write-Host 'Stillus Open With registration updated. Default applications are unchanged.'

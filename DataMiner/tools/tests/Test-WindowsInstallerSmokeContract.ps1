[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$toolsRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$scriptPath = Join-Path $toolsRoot 'test-windows-installer.ps1'
$source = Get-Content -Raw -Encoding UTF8 -LiteralPath $scriptPath
$tokens = $null
$parseErrors = $null
[void][Management.Automation.Language.Parser]::ParseFile(
    $scriptPath,
    [ref]$tokens,
    [ref]$parseErrors
)
if ($parseErrors.Count -ne 0) {
    throw "Installer smoke script has parse errors: $($parseErrors.Message -join '; ')"
}

$requiredPatterns = [ordered]@{
    'a stopwatch beginning before launch' = '\[Diagnostics\.Stopwatch\]::StartNew\(\)'
    'a 15 second total budget' = '\$healthCheckBudgetMs\s*=\s*15000'
    'remaining time derived from elapsed time' = '\$remainingMs\s*=.*\$healthCheckBudgetMs\s*-.*ElapsedMilliseconds'
    'the 3000ms lower bound' = 'ElapsedMilliseconds\s*-lt\s*3000'
    'elapsed evidence' = 'elapsed_ms\s*=.*ElapsedMilliseconds'
}
foreach ($entry in $requiredPatterns.GetEnumerator()) {
    if ($source -notmatch $entry.Value) {
        throw "Installer smoke contract is missing $($entry.Key)."
    }
}
$stopwatchIndex = $source.IndexOf(
    '$stopwatch = [Diagnostics.Stopwatch]::StartNew()',
    [StringComparison]::Ordinal
)
$startProcessIndex = $source.IndexOf(
    '$process = Start-Process -FilePath $ClientPath',
    [StringComparison]::Ordinal
)
if ($stopwatchIndex -lt 0 -or $startProcessIndex -lt 0 -or $stopwatchIndex -gt $startProcessIndex) {
    throw 'Installer smoke stopwatch must start before Start-Process.'
}
if ($source -notmatch '\$process\.WaitForExit\(\$remainingMs\)') {
    throw 'Installer smoke must pass the computed remaining budget to WaitForExit.'
}
if ($source -match '\.WaitForExit\(15000\)') {
    throw 'Installer smoke still grants 15 seconds after setup checks instead of enforcing a total budget.'
}

Write-Host 'PASS: Windows installer smoke uses one measured 15-second launch budget.'

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$toolsRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataMinerRoot = (Resolve-Path (Join-Path $toolsRoot '..')).Path
. (Join-Path $toolsRoot 'windows-build-helpers.ps1')

# No compiler, game, permanent environment setting or test executable required:
# this regression runs on both Windows PowerShell 5.1 and modern PowerShell.
$names = @(
    'DATAMINER_BUILD_FLAVOR',
    'DATAMINER_EXPECT_BUILD_FLAVOR',
    'DATAMINER_EXPECT_DEBUG_ASSERTIONS',
    'DATAMINER_EXPECT_DEV_CATALOG',
    'DATAMINER_EXPECT_UPDATER_ENABLED'
)
$original = @{}
foreach ($name in $names) {
    $original[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$script:invocationCount = 0
$script:failureMode = ''
$script:failureIndex = 0
$caseCount = 0
function cargo {
    $script:invocationCount++
    if ($script:invocationCount -eq $script:failureIndex) {
        if ($script:failureMode -eq 'throw') { throw 'synthetic cargo invocation failure' }
        $global:LASTEXITCODE = 42
    } else {
        $global:LASTEXITCODE = 0
    }
}
try {
    $matrixCount = @(Get-WindowsShippedClientBuildMatrix -DataMinerRoot $dataMinerRoot).Count
    foreach ($state in @('absent', 'existing', 'empty')) {
        foreach ($outcome in @('success', 'first-fails', 'last-fails', 'throws')) {
            # Seed via .NET directly, independently of the helper under test.
            $expected = @{}
            foreach ($name in $names) {
                if ($state -eq 'absent') {
                    [Environment]::SetEnvironmentVariable($name, [NullString]::Value, 'Process')
                } elseif ($state -eq 'empty') {
                    [Environment]::SetEnvironmentVariable($name, '', 'Process')
                } else {
                    [Environment]::SetEnvironmentVariable($name, "caller-$name", 'Process')
                }
                $expected[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
            }
            # .NET Framework cannot represent empty environment values; the
            # absent case covers its behavior. Modern .NET must preserve empty.
            if ($state -eq 'empty' -and $null -eq $expected[$names[0]]) { continue }
            $script:invocationCount = 0
            $script:failureMode = if ($outcome -eq 'throws') { 'throw' } else { 'exit' }
            $script:failureIndex = switch ($outcome) {
                'success' { 0 }
                'last-fails' { $matrixCount }
                default { 1 }
            }
            $caught = $null
            try {
                Invoke-WindowsShippedClientClippy -DataMinerRoot $dataMinerRoot
            } catch {
                $caught = $_.Exception.Message
            }
            if ($outcome -eq 'success') {
                if ($null -ne $caught) { throw $caught }
                if ($script:invocationCount -ne $matrixCount) { throw 'Clippy matrix did not finish' }
            } else {
                $expectedError = if ($outcome -eq 'throws') {
                    'synthetic cargo invocation failure'
                } else { 'cargo clippy failed.*exit code 42' }
                if ($null -eq $caught -or $caught -notmatch $expectedError) {
                    throw "Unexpected failure for ${outcome}: $caught"
                }
                if ($script:invocationCount -ne $script:failureIndex) { throw 'Clippy continued after failure' }
            }
            foreach ($name in $names) {
                if (-not [object]::Equals($expected[$name], [Environment]::GetEnvironmentVariable($name, 'Process'))) {
                    throw "Environment restoration failed: state=$state outcome=$outcome name=$name"
                }
            }
            $caseCount++
        }
    }
    Write-Host "PASS: Windows build environment restoration ($caseCount cases; PowerShell $($PSVersionTable.PSVersion))"
} finally {
    # Restore even when testing an intentionally broken implementation.
    foreach ($name in $names) {
        if ($null -eq $original[$name]) {
            [Environment]::SetEnvironmentVariable($name, [NullString]::Value, 'Process')
        } else {
            [Environment]::SetEnvironmentVariable($name, [string]$original[$name], 'Process')
        }
    }
    Remove-Item -LiteralPath Function:\cargo -ErrorAction SilentlyContinue
}

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Int64]$WindowHandle,
    [Parameter(Mandatory = $true)]
    [int]$ScreenX,
    [Parameter(Mandatory = $true)]
    [int]$ScreenY
)

$ErrorActionPreference = 'Stop'
$clickScript = Join-Path $PSScriptRoot 'Invoke-Pr405Click.ps1'
$process = Start-Process PowerShell -Verb RunAs -PassThru -ArgumentList @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $clickScript,
    '-WindowHandle', $WindowHandle, '-ScreenX', $ScreenX, '-ScreenY', $ScreenY
)
$process.WaitForExit()
exit $process.ExitCode

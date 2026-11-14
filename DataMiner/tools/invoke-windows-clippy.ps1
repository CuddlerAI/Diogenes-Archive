[CmdletBinding()]
param(
    [switch]$HeadroomOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'windows-build-helpers.ps1')

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$configuredCargoTargetDir = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
$targetRoot = Resolve-WindowsCargoTargetDirectory `
    -DataMinerRoot $dataMinerRoot `
    -ConfiguredTargetDirectory $configuredCargoTargetDir `
    -Flavor default_dev

Assert-WindowsBuildDiskHeadroom -TargetPath $targetRoot
if ($HeadroomOnly) {
    Write-Host 'PASS: Windows clippy disk-headroom preflight'
    exit 0
}

$previousCargoTargetDir = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $targetRoot, 'Process')
Push-Location $dataMinerRoot
try {
    Invoke-WindowsShippedClientClippy -DataMinerRoot $dataMinerRoot
} finally {
    Pop-Location
    Set-WindowsBuildProcessEnvironmentVariable -Name 'CARGO_TARGET_DIR' -Value $previousCargoTargetDir
}

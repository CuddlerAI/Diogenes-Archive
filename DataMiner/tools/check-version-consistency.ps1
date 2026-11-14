# Guards constraint C7 of docs/15: the client version is duplicated across the
# Cargo workspace, two package.json files and the Tauri config. A drift between
# them would put a different version on the update channel than the one embedded
# in the shipped PE / used by the updater's semver comparison.
#
# Runs on both Windows and Linux PowerShell (CI uses ubuntu for the fast gate,
# the release workflow re-runs it against the pushed tag).
[CmdletBinding()]
param(
    # Optional release tag (e.g. dataminer-v0.2.0). When provided, the tag must
    # equal "dataminer-v<workspace version>" exactly.
    [string]$Tag
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$semverPattern = '^\d+\.\d+\.\d+$'

function Get-JsonVersion {
    param([Parameter(Mandatory = $true)][string]$Path)

    $json = Get-Content -Raw -Encoding UTF8 -LiteralPath $Path | ConvertFrom-Json
    $property = $json.PSObject.Properties['version']
    if ($null -eq $property -or [string]::IsNullOrWhiteSpace([string]$property.Value)) {
        throw "No version field found in $Path"
    }
    return [string]$property.Value
}

$workspaceTomlPath = Join-Path $dataMinerRoot 'Cargo.toml'
$workspaceToml = Get-Content -Raw -Encoding UTF8 -LiteralPath $workspaceTomlPath
if ($workspaceToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw "Cannot find [workspace.package] version in $workspaceTomlPath"
}

$versions = [ordered]@{
    'Cargo.toml [workspace.package]'        = $Matches[1]
    'package.json'                          = Get-JsonVersion (Join-Path $dataMinerRoot 'package.json')
    'apps/client/package.json'              = Get-JsonVersion (Join-Path $dataMinerRoot 'apps/client/package.json')
    'apps/client/src-tauri/tauri.conf.json' = Get-JsonVersion (Join-Path $dataMinerRoot 'apps/client/src-tauri/tauri.conf.json')
}

foreach ($entry in $versions.GetEnumerator()) {
    if ($entry.Value -notmatch $semverPattern) {
        throw "Version '$($entry.Value)' in $($entry.Key) is not MAJOR.MINOR.PATCH."
    }
}

$distinct = @($versions.Values | Sort-Object -Unique)
if ($distinct.Count -ne 1) {
    foreach ($entry in $versions.GetEnumerator()) {
        Write-Host ("  {0,-42} {1}" -f $entry.Key, $entry.Value)
    }
    throw "Client version mismatch across version sources: $($distinct -join ', ')"
}

$version = $distinct[0]
if ($Tag) {
    $expected = "dataminer-v$version"
    if ($Tag -cne $expected) {
        throw "Release tag '$Tag' does not match the workspace version (expected '$expected')."
    }
}

Write-Host "PASS: client version $version is consistent across Cargo workspace, package manifests and Tauri config."

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BuildDirectory,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][string]$PolicyPath,
    [Parameter(Mandatory = $true)][string]$SourceCommit
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'windows-build-helpers.ps1')

$buildRoot = (Resolve-Path -LiteralPath $BuildDirectory).Path
$policy = Get-Content -Raw -Encoding UTF8 -LiteralPath $PolicyPath | ConvertFrom-Json
Assert-DepthProbeArtifactJsonShape -Object $policy `
    -ExpectedProperties @(
        'schemaVersion',
        'runtimeVersion',
        'changeImpact',
        'changeReason',
        'd3d11Generation',
        'd3d12Generation'
    ) `
    -Label 'Depth probe generation policy'
if ([string]$policy.schemaVersion -ne 'dm-depth-probe-generation-policy-v1') {
    throw "Unsupported Depth probe generation policy: $($policy.schemaVersion)"
}
if ([string]$policy.changeImpact -notin @('none', 'd3d11', 'd3d12', 'both')) {
    throw "Invalid Depth probe changeImpact: $($policy.changeImpact)"
}
if ([string]::IsNullOrWhiteSpace([string]$policy.changeReason) -or
    [string]::IsNullOrWhiteSpace([string]$policy.runtimeVersion)) {
    throw 'Depth probe runtimeVersion and changeReason must not be empty.'
}
if ([long]$policy.d3d11Generation -le 0 -or [long]$policy.d3d12Generation -le 0) {
    throw 'Depth probe generations must be positive.'
}
if ($SourceCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'SourceCommit must be a full lowercase Git SHA.'
}
if (Test-Path -LiteralPath $OutputDirectory) {
    throw "Depth probe artifact output already exists: $OutputDirectory"
}
New-Item -ItemType Directory -Path $OutputDirectory | Out-Null

$binaryNames = @(
    'dm_depth_probe.dll',
    'dm-depth-inject.exe',
    'dm-depth-launch.exe',
    'dm-depth-extract.exe'
)
foreach ($name in $binaryNames) {
    $source = Join-Path $buildRoot $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Depth probe build output is missing: $source"
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $OutputDirectory $name)
}

$dllSha256 = Get-DepthProbeArtifactSha256 `
    -Path (Join-Path $OutputDirectory 'dm_depth_probe.dll')
$runtime = [ordered]@{
    schema = 'dm-depth-probe-runtime-manifest-v1'
    runtimeVersion = [string]$policy.runtimeVersion
    dllSha256 = $dllSha256
    d3d11Generation = [long]$policy.d3d11Generation
    d3d12Generation = [long]$policy.d3d12Generation
}
$runtimePath = Join-Path $OutputDirectory 'dm-depth-probe-runtime.json'
[IO.File]::WriteAllText(
    $runtimePath,
    (($runtime | ConvertTo-Json -Depth 10) + "`n"),
    [Text.UTF8Encoding]::new($false)
)

$fileNames = $binaryNames + @('dm-depth-probe-runtime.json')
$files = @($fileNames | ForEach-Object {
    $path = Join-Path $OutputDirectory $_
    [ordered]@{
        name = $_
        size = [long](Get-Item -LiteralPath $path).Length
        sha256 = Get-DepthProbeArtifactSha256 -Path $path
    }
})
$artifactId = "$($policy.runtimeVersion)-$($SourceCommit.Substring(0, 12))"
$lock = [ordered]@{
    schemaVersion = 'dm-depth-probe-artifact-lock-v1'
    artifactId = $artifactId
    files = $files
}
$lockPath = Join-Path $OutputDirectory 'artifact-lock.json'
[IO.File]::WriteAllText(
    $lockPath,
    (($lock | ConvertTo-Json -Depth 10) + "`n"),
    [Text.UTF8Encoding]::new($false)
)

# Reuse the ordinary installer consumer as the final producer-side verification. This catches a
# malformed lock or manifest before upload without rebuilding any bytes.
$verificationDirectory = Join-Path ([IO.Path]::GetTempPath()) "dm-depth-artifact-verify-$PID-$([Guid]::NewGuid().ToString('N'))"
try {
    $verified = Stage-LockedDepthProbeArtifact `
        -ArtifactDirectory $OutputDirectory `
        -DestinationDirectory $verificationDirectory
    Write-Host "Depth probe artifact ready: $artifactId (D3D11 $($verified.d3d11Generation), D3D12 $($verified.d3d12Generation))"
} finally {
    if (Test-Path -LiteralPath $verificationDirectory) {
        Remove-Item -LiteralPath $verificationDirectory -Recurse -Force
    }
}

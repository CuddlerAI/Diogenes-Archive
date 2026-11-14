[CmdletBinding()]
param(
    [string]$SteamRoot = 'C:\Program Files (x86)\Steam',
    [string]$ExpectedManifestPath = (Join-Path $PSScriptRoot 'cp2077-build-manifest.json'),
    [string]$RuntimeEvidencePath = (Join-Path $PSScriptRoot 'evidence\cp2077-runtime-d6-20260818.json'),
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-AcfValue {
    param(
        [Parameter(Mandatory = $true)][string]$Text,
        [Parameter(Mandatory = $true)][string]$Key
    )
    $pattern = '"' + [regex]::Escape($Key) + '"\s+"([^"]*)"'
    $match = [regex]::Match($Text, $pattern)
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value
}

function New-Check {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][bool]$Passed,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Actual,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Expected
    )
    return [pscustomobject][ordered]@{
        name = $Name
        passed = $Passed
        actual = $Actual
        expected = $Expected
    }
}

if (-not (Test-Path -LiteralPath $ExpectedManifestPath -PathType Leaf)) {
    throw "expected build manifest not found: $ExpectedManifestPath"
}
$expected = Get-Content -Raw -Encoding UTF8 -LiteralPath $ExpectedManifestPath | ConvertFrom-Json
if ($expected.schema_version -ne 'dm-cp2077-build-preflight-1') {
    throw "unsupported build manifest schema: $($expected.schema_version)"
}
$runtimeEvidence = $null
if (Test-Path -LiteralPath $RuntimeEvidencePath -PathType Leaf) {
    $runtimeEvidence = Get-Content -Raw -Encoding UTF8 -LiteralPath $RuntimeEvidencePath | ConvertFrom-Json
    if ($runtimeEvidence.schema_version -ne 'dm-cp2077-runtime-d6-observation-2') {
        throw "unsupported runtime evidence schema: $($runtimeEvidence.schema_version)"
    }
}

$steamManifestPath = Join-Path $SteamRoot 'steamapps\appmanifest_1091500.acf'
if (-not (Test-Path -LiteralPath $steamManifestPath -PathType Leaf)) {
    throw "Steam app manifest not found: $steamManifestPath"
}
$acf = Get-Content -Raw -Encoding UTF8 -LiteralPath $steamManifestPath
$installDir = Get-AcfValue $acf 'installdir'
if ([string]::IsNullOrWhiteSpace($installDir)) {
    throw 'Steam app manifest has no installdir'
}
$gameRoot = Join-Path (Join-Path $SteamRoot 'steamapps\common') $installDir
$exeRelativePath = $expected.executable.relative_path -replace '/', '\'
$exePath = Join-Path $gameRoot $exeRelativePath
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "game executable not found below the Steam library: $exeRelativePath"
}

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $exePath).Hash.ToLowerInvariant()
$version = (Get-Item -LiteralPath $exePath).VersionInfo
$signature = Get-AuthenticodeSignature -LiteralPath $exePath
$signerSubject = if ($null -eq $signature.SignerCertificate) {
    ''
} else {
    $signature.SignerCertificate.Subject
}

$checks = [Collections.Generic.List[object]]::new()
$checks.Add((New-Check 'steam_app_id' ((Get-AcfValue $acf 'appid') -eq $expected.source.app_id) (Get-AcfValue $acf 'appid') $expected.source.app_id))
$checks.Add((New-Check 'steam_build_id' ((Get-AcfValue $acf 'buildid') -eq $expected.source.build_id) (Get-AcfValue $acf 'buildid') $expected.source.build_id))
$checks.Add((New-Check 'steam_state_flags' ([int64](Get-AcfValue $acf 'StateFlags') -eq [int64]$expected.source.state_flags) (Get-AcfValue $acf 'StateFlags') ([string]$expected.source.state_flags)))
$checks.Add((New-Check 'steam_size_on_disk' ([int64](Get-AcfValue $acf 'SizeOnDisk') -eq [int64]$expected.source.size_on_disk_bytes) (Get-AcfValue $acf 'SizeOnDisk') ([string]$expected.source.size_on_disk_bytes)))
$checks.Add((New-Check 'executable_sha256' ($hash -eq $expected.executable.sha256) $hash $expected.executable.sha256))
$checks.Add((New-Check 'file_version' ($version.FileVersion -eq $expected.executable.file_version) $version.FileVersion $expected.executable.file_version))
$checks.Add((New-Check 'product_version' ($version.ProductVersion -eq $expected.executable.product_version) $version.ProductVersion $expected.executable.product_version))
$checks.Add((New-Check 'authenticode_status' ($signature.Status -eq 'Valid') ([string]$signature.Status) 'Valid'))
$checks.Add((New-Check 'authenticode_signer' ($signerSubject -like "*$($expected.executable.authenticode_signer)*") $signerSubject "contains $($expected.executable.authenticode_signer)"))

$commonModArtifacts = @(
    'bin\x64\version.dll',
    'bin\x64\winmm.dll',
    'bin\x64\dxgi.dll',
    'bin\x64\d3d12.dll',
    'bin\x64\plugins',
    'red4ext',
    'archive\pc\mod',
    'mods',
    'r6\scripts',
    'tools\redmod'
)
$observedModArtifacts = @(
    $commonModArtifacts |
        Where-Object { Test-Path -LiteralPath (Join-Path $gameRoot $_) }
)
$checks.Add((New-Check 'common_mod_loader_artifacts' ($observedModArtifacts.Count -eq 0) ($observedModArtifacts -join ',') 'none'))

# The project research-only lane was retired by issue #855 §E.5. The preflight now runs the
# default gate, which requires a publisher or counsel approval with all four scopes bounded.
$rightsGate = Join-Path $PSScriptRoot '..\game-adaptation\Test-GameDepthRightsGate.ps1'
$rightsJson = & $rightsGate -AppId 1091500
$rightsExit = $LASTEXITCODE
$rights = $rightsJson | ConvertFrom-Json
$rightsPassed = $rightsExit -eq 0 -and $rights.status -eq 'pass' -and $rights.publisher_or_counsel_approval
$checks.Add((New-Check 'publisher_or_counsel_rights_gate' $rightsPassed $rights.status 'pass'))

$runtimeEvidenceIdentityPassed = $false
if ($null -ne $runtimeEvidence) {
    $runtimeEvidenceIdentityPassed =
        $runtimeEvidence.identity.app_id -eq $expected.source.app_id -and
        $runtimeEvidence.identity.build_id -eq $expected.source.build_id -and
        $runtimeEvidence.identity.executable_sha256 -eq $expected.executable.sha256
    $checks.Add((New-Check 'runtime_evidence_identity' $runtimeEvidenceIdentityPassed ([string]$runtimeEvidence.identity.build_id) ([string]$expected.source.build_id)))
}

$userSettingsPath = Join-Path $env:LOCALAPPDATA 'CD Projekt Red\Cyberpunk 2077\UserSettings.json'
$userSettingsObserved = Test-Path -LiteralPath $userSettingsPath -PathType Leaf
$staticPassed = @($checks | Where-Object { -not $_.passed }).Count -eq 0
$runtimeGraphicsApiConfirmed =
    $null -ne $runtimeEvidence -and
    $runtimeEvidenceIdentityPassed -and
    $runtimeEvidence.graphics.direct3d12_confirmed
$runtimeProtectionGateCompleted =
    $null -ne $runtimeEvidence -and
    $runtimeEvidenceIdentityPassed -and
    $runtimeEvidence.protection_and_network.runtime_protection_gate_completed
$runtimeReady = $staticPassed -and $runtimeGraphicsApiConfirmed -and $runtimeProtectionGateCompleted

$report = [pscustomobject][ordered]@{
    schema_version = 'dm-cp2077-preflight-report-1'
    app_id = '1091500'
    build_id = Get-AcfValue $acf 'buildid'
    executable_relative_path = $expected.executable.relative_path
    non_trainable = $true
    static_preflight_passed = $staticPassed
    rights_gate_passed = $rightsPassed
    publisher_or_counsel_approval = $rights.publisher_or_counsel_approval
    formal_delivery_permitted = $rights.formal_delivery_permitted
    user_settings_observed = $userSettingsObserved
    runtime_graphics_api_confirmed = $runtimeGraphicsApiConfirmed
    runtime_protection_gate_completed = $runtimeProtectionGateCompleted
    runtime_ready = $runtimeReady
    status = if (-not $staticPassed) { 'blocked_static_preflight' } elseif (-not $runtimeReady) { 'preflight_pass_runtime_blocked' } else { 'pass' }
    observed_common_mod_artifacts = $observedModArtifacts
    checks = @($checks)
    restrictions = @(
        'This report contains no absolute install path, account name, save data or device-unique identifier.',
        'A static pass is not an authorization by itself: the unified rights decision is only one of the gates, and it never makes captured data trainable.',
        'The runtime renderer and protection gates remain mandatory before game-process instrumentation.'
    )
}

$json = $report | ConvertTo-Json -Depth 10
Write-Output $json
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $absoluteOutput = [IO.Path]::GetFullPath($OutputPath)
    if (Test-Path -LiteralPath $absoluteOutput) {
        throw "refusing to overwrite report: $absoluteOutput"
    }
    $parent = Split-Path -Parent $absoluteOutput
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
        [IO.Directory]::CreateDirectory($parent) | Out-Null
    }
    [IO.File]::WriteAllText($absoluteOutput, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

if (-not $staticPassed) { exit 2 }
exit 0

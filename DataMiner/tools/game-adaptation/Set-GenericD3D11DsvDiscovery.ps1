<#
.SYNOPSIS
  Arms or seals one bounded generic D3D11 DSV discovery session.

.DESCRIPTION
  Arm consumes already-collected point-in-time runtime gate evidence, rechecks the repository
  rights and Steam D6 gates, verifies the exact executable, and creates the fixed probe config
  with create-new semantics. It does not launch a game or inject a DLL.

  Seal changes only the exact matching discovery config to enabled=false and waits for the probe
  to create its discovery-only report. It never overwrites an evidence directory or report.
#>
[CmdletBinding(DefaultParameterSetName = 'Arm')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [switch]$Arm,

    [Parameter(Mandatory = $true, ParameterSetName = 'Seal')]
    [switch]$Seal,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$DiscoveryId,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedExecutableSha256,

    [Parameter(Mandatory = $true)]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$OutputLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$AppId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [ValidatePattern('^\d+$')]
    [string]$ExpectedBuildId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ProcessId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [string]$ExpectedExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [string]$RuntimeEvidencePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [string]$RepresentativeSceneAttestationPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Arm')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedRepresentativeSceneAttestationSha256,

    [Parameter(ParameterSetName = 'Arm')]
    [ValidateRange(1, 1200)]
    [uint32]$MaxFrames = 600,

    [Parameter(ParameterSetName = 'Arm')]
    [ValidateRange(1, 256)]
    [uint32]$MaxCandidates = 128,

    [Parameter(ParameterSetName = 'Arm')]
    [ValidateRange(1, 64)]
    [uint32]$MaxCandidatesPerFrame = 32,

    [Parameter(ParameterSetName = 'Arm')]
    [ValidateRange(1, 600)]
    [uint32]$MaximumRuntimeEvidenceAgeSeconds = 120,

    [Parameter(ParameterSetName = 'Seal')]
    [ValidateRange(1, 60)]
    [uint32]$SealTimeoutSeconds = 10
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-NoReparseAncestor {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$Label = 'path')
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains an NTFS reparse point: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
}

function Get-FixedConfigPath {
    $programData = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if (-not $programData) { throw 'CommonApplicationData is unavailable.' }
    return [IO.Path]::Combine($programData, 'DataMiner', 'depth-lab', 'probe-config.json')
}

function Read-JsonFile {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$Label = 'JSON')
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    Assert-NoReparseAncestor -Path $Path -Label $Label
    return (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json)
}

function Write-CreateNewUtf8Json {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)]$Value)
    $full = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $full) { throw "Refusing to overwrite existing config: $full" }
    $parent = [IO.Path]::GetDirectoryName($full)
    [IO.Directory]::CreateDirectory($parent) | Out-Null
    Assert-NoReparseAncestor -Path $parent -Label 'config directory'
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Depth 12) + "`n")
    $stream = [IO.File]::Open($full, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
}

function Get-CanonicalUtcTimestamp {
    param([Parameter(Mandatory = $true)][string]$Value, [string]$Label = 'UTC timestamp')
    try {
        $parsed = [DateTimeOffset]::ParseExact(
            $Value,
            'o',
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    } catch {
        throw "$Label is not a round-trip ISO-8601 timestamp."
    }
    $canonical = $parsed.UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    if ($Value -cne $canonical) {
        throw "$Label must be canonical UTC with the Z suffix."
    }
    return $canonical
}

function Get-LiveProcessStartTimeUtc {
    param([Parameter(Mandatory = $true)][uint32]$Id)
    $process = Get-Process -Id $Id -ErrorAction Stop
    return $process.StartTime.ToUniversalTime().ToString(
        'o', [Globalization.CultureInfo]::InvariantCulture)
}

$expectedHash = $ExpectedExecutableSha256.ToLowerInvariant()
$root = (Resolve-Path -LiteralPath $EvidenceRoot).Path
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
    throw "EvidenceRoot is not a directory: $EvidenceRoot"
}
Assert-NoReparseAncestor -Path $root -Label 'evidence root'
$output = [IO.Path]::Combine($root, $OutputLeaf)
if ([IO.Path]::GetDirectoryName($output) -ne $root) {
    throw 'OutputLeaf must resolve to a direct child of EvidenceRoot.'
}
$configPath = Get-FixedConfigPath

if ($PSCmdlet.ParameterSetName -eq 'Arm') {
    $expectedSceneAttestationHash = $ExpectedRepresentativeSceneAttestationSha256.ToLowerInvariant()
    if (Test-Path -LiteralPath $configPath) {
        Assert-NoReparseAncestor -Path $configPath -Label 'existing fixed probe config'
        throw "Refusing to replace an existing fixed probe config: $configPath"
    }
    if (Test-Path -LiteralPath $output) {
        throw "Discovery output must be new: $output"
    }
    $exe = (Resolve-Path -LiteralPath $ExpectedExecutablePath).Path
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        throw "Expected executable is not a file: $ExpectedExecutablePath"
    }
    Assert-NoReparseAncestor -Path $exe -Label 'expected executable'
    $actualHash = 'sha256:' + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "Expected executable hash mismatch: expected=$expectedHash actual=$actualHash"
    }

    # This is the durable repository-side authorization boundary for the generic injector.
    # Keep game/build/capability governance here rather than adding fields to the strict v1
    # probe config/report and accidentally changing its discovery-only schema ceiling.
    $targetScript = Join-Path $PSScriptRoot 'Test-GenericD3D11DsvDiscoveryTargets.ps1'
    $targetText = (& $targetScript `
        -GameId $GameId `
        -AppId ([string]$AppId) `
        -BuildId $ExpectedBuildId `
        -ExecutableBasename ([IO.Path]::GetFileName($exe)) `
        -ExecutableSha256 $expectedHash | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Generic D3D11 target binding blocked $GameId.`n$targetText" }
    $targetReport = $targetText | ConvertFrom-Json
    if ($targetReport.status -ne 'pass' -or $null -eq $targetReport.selected_target) {
        throw "Generic D3D11 target binding did not select $GameId."
    }
    $target = $targetReport.selected_target

    $rightsScript = Join-Path $PSScriptRoot 'Test-GameDepthRightsGate.ps1'
    $rightsText = (& $rightsScript -AppId $AppId | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Rights gate blocked AppID $AppId.`n$rightsText" }
    $rights = $rightsText | ConvertFrom-Json
    if ($rights.status -ne 'pass' -or -not $rights.publisher_or_counsel_approval) {
        throw "Rights gate did not pass for AppID $AppId."
    }
    if ($rights.selected_record_count -ne 1 -or
        @($rights.records).Count -ne 1 -or
        [string]$rights.records[0].app_id -ne [string]$AppId -or
        -not $rights.records[0].gate_pass -or
        -not (@($rights.records[0].catalog_game_ids) -ccontains $GameId) -or
        -not $rights.runtime_anti_cheat_gate_still_required) {
        throw "Rights gate did not bind AppID $AppId to gameId $GameId with the independent runtime protection gate intact."
    }

    $d6Script = Join-Path (Split-Path -Parent $PSScriptRoot) 'Test-SteamD6InstallGate.ps1'
    $d6Text = (& $d6Script -AppId $AppId | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Steam D6 gate blocked AppID $AppId.`n$d6Text" }
    $d6 = $d6Text | ConvertFrom-Json
    if ($d6.status -ne 'install_gate_passed') { throw "Steam D6 gate did not pass for AppID $AppId." }
    if ([string]$d6.app_id -ne [string]$AppId -or [string]$d6.build_id -ne $ExpectedBuildId) {
        throw "Steam D6 identity does not match target AppID/build: expected=$AppId/$ExpectedBuildId actual=$($d6.app_id)/$($d6.build_id)."
    }
    $matchingCandidate = @($d6.executable_candidates | Where-Object { $_.sha256 -eq $expectedHash })
    if ($matchingCandidate.Count -ne 1) {
        throw 'Steam D6 evidence does not contain exactly one candidate with the expected EXE hash.'
    }
    if (-not ([IO.Path]::GetFileName([string]$matchingCandidate[0].relative_path)).Equals(
        [string]$target.executableBasename, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Steam D6 executable basename does not match the discovery target.'
    }
    $d6CandidatePath = [IO.Path]::GetFullPath((Join-Path ([string]$d6.install_directory) ([string]$matchingCandidate[0].relative_path)))
    if (-not $d6CandidatePath.Equals($exe, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Requested executable path is not the exact Steam D6 candidate selected by hash.'
    }

    $runtime = Read-JsonFile -Path $RuntimeEvidencePath -Label 'runtime gate evidence'
    if ($runtime.schema -ne $target.protectionGate.runtimeEvidenceSchema -or $runtime.status -ne 'pass') {
        throw 'Runtime gate evidence is not a passing v1 report.'
    }
    if ([uint32]$runtime.process.process_id -ne $ProcessId) {
        throw 'Runtime gate PID does not match the requested injection PID.'
    }
    $reportedStartTimeUtc = Get-CanonicalUtcTimestamp `
        -Value ([string]$runtime.process.start_time_utc) `
        -Label 'runtime gate process.start_time_utc'
    $liveStartTimeUtc = Get-LiveProcessStartTimeUtc -Id $ProcessId
    if ($reportedStartTimeUtc -cne $liveStartTimeUtc) {
        throw "Runtime gate process instance does not match the live PID (reported_start=$reportedStartTimeUtc live_start=$liveStartTimeUtc)."
    }
    if ([string]$runtime.process.executable_sha256 -ne $expectedHash) {
        throw 'Runtime gate executable hash does not match the discovery identity.'
    }
    if (-not ([IO.Path]::GetFullPath([string]$runtime.process.executable_path)).Equals(
        [IO.Path]::GetFullPath($exe), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Runtime gate executable path does not match the requested executable.'
    }
    if ($runtime.graphics.expected_api -ne $target.protectionGate.expectedGraphicsApi -or -not (@($runtime.graphics.hits) -contains 'd3d11.dll')) {
        throw 'Runtime gate does not prove D3D11 for this process.'
    }
    if (-not $runtime.module_enumeration.complete -or
        -not $runtime.anti_cheat.require_none_loaded -or
        -not $runtime.anti_cheat.service_enumeration_complete -or
        -not $runtime.anti_cheat.driver_enumeration_complete -or
        @($runtime.anti_cheat.module_hits).Count -ne 0 -or
        [int]$runtime.anti_cheat.running_service_hits -ne 0 -or
        [int]$runtime.anti_cheat.running_driver_hits -ne 0 -or
        @($runtime.blocking).Count -ne 0) {
        throw 'Runtime anti-cheat/module/service/driver gate is incomplete or blocked.'
    }
    $captured = [DateTime]::Parse([string]$runtime.captured_utc).ToUniversalTime()
    $age = ([DateTime]::UtcNow - $captured).TotalSeconds
    if ($age -lt -5 -or $age -gt $MaximumRuntimeEvidenceAgeSeconds) {
        throw "Runtime evidence is not fresh enough for arming (age_seconds=$([Math]::Round($age, 1)))."
    }

    # This is a local operator assertion, not scene recognition. Its exact bytes, the arm runtime
    # report, live process instance, and current window identity are all rechecked fail closed.
    $sceneGate = $target.representativeSceneGate
    if (-not $sceneGate.required) { throw 'Target registry does not require representative-scene attestation.' }
    $sceneValidator = Join-Path $PSScriptRoot 'Test-GenericD3D11RepresentativeSceneAttestation.ps1'
    $sceneText = (& $sceneValidator `
        -AttestationPath $RepresentativeSceneAttestationPath `
        -ExpectedAttestationSha256 $expectedSceneAttestationHash `
        -GameId $GameId -AppId ([string]$AppId) -BuildId $ExpectedBuildId `
        -ExpectedDiscoveryId $DiscoveryId -ExpectedOutputLeaf $OutputLeaf `
        -ExecutableBasename ([IO.Path]::GetFileName($exe)) -ExecutableSha256 $expectedHash `
        -RuntimeEvidencePath $RuntimeEvidencePath -ExpectedProcessId $ProcessId `
        -ExpectedProcessStartTimeUtc $reportedStartTimeUtc `
        -ExpectedWindowClass ([string]$sceneGate.expectedWindow.className) `
        -ExpectedClientWidth ([uint32]$sceneGate.expectedWindow.clientWidth) `
        -ExpectedClientHeight ([uint32]$sceneGate.expectedWindow.clientHeight) `
        -MaximumAgeSeconds ([uint32]$sceneGate.maximumAgeSeconds) | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Representative-scene attestation blocked arming.`n$sceneText" }
    $sceneReport = $sceneText | ConvertFrom-Json
    if ($sceneReport.status -cne 'pass' -or
        $sceneReport.attestation_sha256 -cne $expectedSceneAttestationHash -or
        $sceneReport.discovery_id -cne $DiscoveryId -or
        $sceneReport.output_leaf -cne $OutputLeaf -or
        $sceneReport.scene_kind -cne 'representative_offline_single_player') {
        throw 'Representative-scene attestation did not return the exact passing identity.'
    }

    $config = [ordered]@{
        enabled = $true
        output_dir = $output
        unbind_from_end = 4
        max_frames = $null
        present_only = $false
        watermark_only = $false
        writer_bypass = $false
        target_depth_hz = $null
        lane_fps = $null
        camera_probe = $false
        camera_discovery = $null
        representative_scene_attestation_sha256 = $expectedSceneAttestationHash
        dsv_discovery = [ordered]@{
            schema = 'd3d11-dsv-discovery-config/v2'
            discovery_id = $DiscoveryId
            executable_sha256 = $expectedHash
            evidence_root = $root
            limits = [ordered]@{
                max_frames = $MaxFrames
                max_candidates = $MaxCandidates
                max_candidates_per_frame = $MaxCandidatesPerFrame
            }
        }
        camera_production = $null
        depth_profile_path = $null
        depth_profile_sha256 = $null
        session_id = "dsv-discovery-$DiscoveryId"
    }
    Write-CreateNewUtf8Json -Path $configPath -Value $config
    $configHash = 'sha256:' + (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [ordered]@{
        status = 'armed_discovery_only'
        recording_ready = $false
        config_path = $configPath
        config_sha256 = $configHash
        output_dir = $output
        game_id = $GameId
        steam_app_id = [string]$AppId
        steam_build_id = $ExpectedBuildId
        capability_at_arm = [string]$target.capabilityGate.capability
        process_id = $ProcessId
        process_start_time_utc = $reportedStartTimeUtc
        executable_sha256 = $expectedHash
        representative_scene_attestation_sha256 = $expectedSceneAttestationHash
        next_step = 'Run Invoke-GenericD3D11DsvInjection.ps1 immediately; direct dm-depth-inject use is outside this guarded workflow.'
    } | ConvertTo-Json -Depth 4
    exit 0
}

$active = Read-JsonFile -Path $configPath -Label 'active probe config'
if (-not $active.enabled -or
    $active.session_id -ne "dsv-discovery-$DiscoveryId" -or
    $active.dsv_discovery.schema -ne 'd3d11-dsv-discovery-config/v2' -or
    $active.dsv_discovery.discovery_id -ne $DiscoveryId -or
    $active.dsv_discovery.executable_sha256 -ne $expectedHash -or
    [IO.Path]::GetFullPath([string]$active.dsv_discovery.evidence_root) -ne $root -or
    [IO.Path]::GetFullPath([string]$active.output_dir) -ne $output) {
    throw 'Refusing to seal a config whose exact discovery identity does not match.'
}
$active.enabled = $false
$temporary = "$configPath.tmp-$([Guid]::NewGuid().ToString('N'))"
try {
    Write-CreateNewUtf8Json -Path $temporary -Value $active
    Move-Item -LiteralPath $temporary -Destination $configPath -Force
} finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
}
$report = Join-Path $output 'd3d11-dsv-discovery-report.json'
$deadline = [DateTime]::UtcNow.AddSeconds($SealTimeoutSeconds)
while (-not (Test-Path -LiteralPath $report -PathType Leaf) -and [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 100
}
if (-not (Test-Path -LiteralPath $report -PathType Leaf)) {
    throw "Matching config was disabled, but no sealed report appeared within $SealTimeoutSeconds seconds."
}
[ordered]@{
    status = 'sealed_discovery_only'
    recording_ready = $false
    report = (Resolve-Path -LiteralPath $report).Path
} | ConvertTo-Json -Depth 3

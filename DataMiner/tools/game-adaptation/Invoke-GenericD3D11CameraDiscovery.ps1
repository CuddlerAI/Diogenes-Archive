<#
.SYNOPSIS
  Creates a validated, non-executing generic D3D11 camera-discovery v3 field plan.

.DESCRIPTION
  Produces three exact probe configs for POSE_STATIC, POSE_WALK, and POSE_TURN plus the
  commands needed to validate sealed reports and run offline camera-evidence reduction. Without
  an exact comparison camera, the reducer is admitted only as an exit-1 human-review-only bridge.

  The script never writes ProgramData's fixed probe config, starts or drives a game, injects a
  DLL, changes protection state, or guesses that an action is complete from elapsed time. A human
  must confirm each scene/action, and a separately approved exact-instance loader/controller must
  own initial loading and config transitions.
#>
[CmdletBinding(DefaultParameterSetName = 'Plan')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [switch]$Plan,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^\d+$')]
    [string]$AppId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^\d+$')]
    [string]$BuildId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ProcessId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [string]$ExpectedExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedExecutableSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [string]$RuntimeEvidencePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [string]$RepresentativeSceneAttestationPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedRepresentativeSceneAttestationSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,127}\.json$')]
    [string]$SelectionAttestationLocator,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedSelectionAttestationSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$')]
    [string]$PlanLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$')]
    [string]$DiscoveryId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$')]
    [string]$OutputLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Plan')]
    [string]$DepthLabPath,

    [Parameter(ParameterSetName = 'Plan')]
    [string]$ComparisonCameraReferencePath = '',

    [Parameter(ParameterSetName = 'Plan')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$SampleEveryNPresents = 1,

    [Parameter(ParameterSetName = 'Plan')]
    [ValidateRange(1, 16)]
    [uint32]$MaxMainPassesPerPresent = 4,

    [Parameter(ParameterSetName = 'Plan')]
    [ValidateRange(0, [uint32]::MaxValue)]
    [uint32]$DrawWindowStart = 0,

    [Parameter(ParameterSetName = 'Plan')]
    [ValidateRange(1, 256)]
    [uint32]$DrawWindowCount = 128,

    [Parameter(ParameterSetName = 'Plan')]
    [switch]$DryRun
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

function Get-FullExistingPath {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label,
        [ValidateSet('Leaf', 'Container')][string]$Kind = 'Leaf'
    )
    if (-not [IO.Path]::IsPathRooted($Path)) { throw "$Label must be absolute." }
    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.Substring($root.Length).Contains(':')) { throw "$Label must not use an alternate data stream." }
    $pathType = if ($Kind -eq 'Leaf') { 'Leaf' } else { 'Container' }
    if (-not (Test-Path -LiteralPath $full -PathType $pathType)) { throw "$Label does not exist: $full" }
    Assert-NoReparseAncestor -Path $full -Label $Label
    return (Resolve-Path -LiteralPath $full).Path
}

function Assert-DirectChildPath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $pathFull = [IO.Path]::GetFullPath($Path)
    if (-not [IO.Path]::GetDirectoryName($pathFull).Equals($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must be a direct child of evidence root."
    }
}

function Read-Utf8Json {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Label)
    $full = Get-FullExistingPath -Path $Path -Label $Label -Kind Leaf
    $bytes = Read-BoundedStableBytes -Path $full -MaximumBytes 1MB -Label $Label
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw "$Label must be UTF-8 without BOM."
    }
    try { $value = [Text.UTF8Encoding]::new($false, $true).GetString($bytes) | ConvertFrom-Json }
    catch { throw "$Label is not strict UTF-8 JSON: $($_.Exception.Message)" }
    return [pscustomobject][ordered]@{
        path = $full
        sha256 = Get-BytesSha256 -Bytes $bytes
        value = $value
    }
}

function Read-BoundedStableBytes {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][int64]$MaximumBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $length = $stream.Length
        if ($length -le 0 -or $length -gt $MaximumBytes -or $length -gt [int]::MaxValue) {
            throw "$Label has an unsafe byte length."
        }
        $bytes = [byte[]]::new([int]$length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -le 0) { throw "$Label ended during its bounded read." }
            $offset += $read
        }
        if ($stream.Length -ne $length -or $stream.ReadByte() -ne -1) {
            throw "$Label changed during its bounded read."
        }
        return , $bytes
    } finally {
        $stream.Dispose()
    }
}

function Get-BytesSha256 {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $digest = $hasher.ComputeHash($Bytes) }
    finally { $hasher.Dispose() }
    return 'sha256:' + (($digest | ForEach-Object { $_.ToString('x2') }) -join '')
}

function Write-CreateNewUtf8Json {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)]$Value)
    $full = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $full) { throw "refusing to overwrite: $full" }
    $parent = [IO.Path]::GetDirectoryName($full)
    Assert-NoReparseAncestor -Path $parent -Label 'plan directory'
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Depth 32) + "`n")
    $stream = [IO.File]::Open($full, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) }
    finally { $stream.Dispose() }
}

function Write-CreateNewUtf8Line {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Value)
    $full = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $full) { throw "refusing to overwrite: $full" }
    Assert-NoReparseAncestor -Path ([IO.Path]::GetDirectoryName($full)) -Label 'plan directory'
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Value + "`n")
    $stream = [IO.File]::Open($full, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) }
    finally { $stream.Dispose() }
}

function Invoke-JsonCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $text = (& $Executable @Arguments | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "$Label failed with exit=$LASTEXITCODE.`n$text" }
    try { return ($text | ConvertFrom-Json) }
    catch { throw "$Label did not return JSON: $($_.Exception.Message)" }
}

function Get-FixedConfigPath {
    $programData = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if (-not $programData) { throw 'CommonApplicationData is unavailable.' }
    return [IO.Path]::Combine($programData, 'DataMiner', 'depth-lab', 'probe-config.json')
}

function Remove-DryRunDirectory {
    param([Parameter(Mandatory = $true)][string]$Root, [Parameter(Mandatory = $true)][string]$Directory)
    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $directoryFull = [IO.Path]::GetFullPath($Directory)
    $leaf = [IO.Path]::GetFileName($directoryFull)
    if (-not [IO.Path]::GetDirectoryName($directoryFull).Equals($rootFull, [StringComparison]::OrdinalIgnoreCase) -or
        -not $leaf.StartsWith('.camera-plan-pending-', [StringComparison]::Ordinal)) {
        throw 'refusing to remove an unverified dry-run directory.'
    }
    [IO.Directory]::Delete($directoryFull, $true)
}

try {
    $expectedExeHash = $ExpectedExecutableSha256.ToLowerInvariant()
    $expectedSceneHash = $ExpectedRepresentativeSceneAttestationSha256.ToLowerInvariant()
    $expectedSelectionHash = $ExpectedSelectionAttestationSha256.ToLowerInvariant()
    if (($SelectionAttestationLocator.Contains('\')) -or $SelectionAttestationLocator.Contains('/') -or
        $SelectionAttestationLocator.Contains(':')) {
        throw 'SelectionAttestationLocator must be one direct-child JSON leaf.'
    }
    if ([uint64]$MaxMainPassesPerPresent * [uint64]$DrawWindowCount -gt 512 -or
        [uint64]$DrawWindowStart + [uint64]$DrawWindowCount -gt 512) {
        throw 'camera draw sampling exceeds the v3 bounded envelope.'
    }
    if ($PlanLeaf.Equals($OutputLeaf, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'PlanLeaf and OutputLeaf must be distinct under Windows ordinal-ignore-case semantics.'
    }

    $root = Get-FullExistingPath -Path $EvidenceRoot -Label 'evidence root' -Kind Container
    $exe = Get-FullExistingPath -Path $ExpectedExecutablePath -Label 'target executable' -Kind Leaf
    $runtime = Read-Utf8Json -Path $RuntimeEvidencePath -Label 'runtime evidence'
    $scene = Read-Utf8Json -Path $RepresentativeSceneAttestationPath -Label 'representative-scene attestation'
    $depthLab = Get-FullExistingPath -Path $DepthLabPath -Label 'depth-lab executable' -Kind Leaf
    Assert-DirectChildPath -Root $root -Path $runtime.path -Label 'runtime evidence'
    Assert-DirectChildPath -Root $root -Path $scene.path -Label 'representative-scene attestation'
    if ($scene.sha256 -cne $expectedSceneHash) { throw 'representative-scene attestation bytes changed.' }
    $selectionPath = Join-Path $root $SelectionAttestationLocator
    Assert-DirectChildPath -Root $root -Path $selectionPath -Label 'selection attestation'
    $selection = Read-Utf8Json -Path $selectionPath -Label 'selection attestation'
    if ($selection.sha256 -cne $expectedSelectionHash) { throw 'selection attestation bytes changed.' }
    $actualExeHash = 'sha256:' + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualExeHash -cne $expectedExeHash) { throw 'target executable bytes differ from ExpectedExecutableSha256.' }
    if ([uint32]$runtime.value.process.process_id -ne $ProcessId) { throw 'runtime evidence PID differs from ProcessId.' }
    $processStart = [string]$runtime.value.process.start_time_utc
    if ([string]::IsNullOrWhiteSpace($processStart)) { throw 'runtime evidence has no process start identity.' }

    $fixedConfig = Get-FixedConfigPath
    if (Test-Path -LiteralPath $fixedConfig) {
        Assert-NoReparseAncestor -Path $fixedConfig -Label 'existing fixed probe config'
        throw "fixed probe config already exists; its owner must close it before planning: $fixedConfig"
    }
    $finalDirectory = Join-Path $root $PlanLeaf
    $outputDirectory = Join-Path $root $OutputLeaf
    Assert-DirectChildPath -Root $root -Path $finalDirectory -Label 'plan directory'
    Assert-DirectChildPath -Root $root -Path $outputDirectory -Label 'camera output'
    if (Test-Path -LiteralPath $finalDirectory) { throw "plan directory must be new: $finalDirectory" }
    if (Test-Path -LiteralPath $outputDirectory) { throw "camera output must be new: $outputDirectory" }

    $targetScript = Join-Path $PSScriptRoot 'Test-GenericD3D11DsvDiscoveryTargets.ps1'
    $targetResult = Invoke-JsonCommand -Executable $targetScript -Label 'target registry selection' -Arguments @(
        '-GameId', $GameId, '-AppId', $AppId, '-BuildId', $BuildId,
        '-ExecutableBasename', [IO.Path]::GetFileName($exe), '-ExecutableSha256', $expectedExeHash
    )
    if ([string]$targetResult.status -cne 'pass' -or $null -eq $targetResult.selected_target) {
        throw 'generic D3D11 target registry did not authorize this exact target.'
    }
    $target = $targetResult.selected_target

    $selectionResult = Invoke-JsonCommand -Executable $depthLab -Label 'DSV selection source replay' -Arguments @(
        'dsv-selection-validate', '--evidence-root', $root, '--input', $selectionPath,
        '--expected-locator', $SelectionAttestationLocator, '--expected-sha256', $expectedSelectionHash,
        '--expected-executable-sha256', $expectedExeHash
    )
    if ([string]$selectionResult.status -cne 'pass') { throw 'selection replay did not pass.' }

    $comparisonReference = $null
    $hasComparisonReference = -not [string]::IsNullOrWhiteSpace($ComparisonCameraReferencePath)
    if ($hasComparisonReference) {
        $comparisonReference = (Read-Utf8Json -Path $ComparisonCameraReferencePath -Label 'comparison camera reference').value
    }

    $pendingLeaf = '.camera-plan-pending-' + [Guid]::NewGuid().ToString('N')
    $pendingDirectory = Join-Path $root $pendingLeaf
    Assert-DirectChildPath -Root $root -Path $pendingDirectory -Label 'pending plan directory'
    [IO.Directory]::CreateDirectory($pendingDirectory) | Out-Null
    Assert-NoReparseAncestor -Path $pendingDirectory -Label 'pending plan directory'

    $phaseOrder = @('POSE_STATIC', 'POSE_WALK', 'POSE_TURN')
    $phaseInstructions = @{
        POSE_STATIC = 'Human confirms the representative offline single-player scene, then remains fully static. Do not infer completion from elapsed time.'
        POSE_WALK = 'After the prior create-new sealed report passes validation, human confirms straight walking without turning the view.'
        POSE_TURN = 'After the prior create-new sealed report passes validation, human confirms turning in place without walking.'
    }
    $sessionId = "camera-discovery-$DiscoveryId"
    $phases = @()
    foreach ($phase in $phaseOrder) {
        $cameraDiscovery = [ordered]@{
            schema = 'camera-discovery-config/v3'
            phase_id = $phase
            executable_sha256 = $expectedExeHash
            dsv_selection = [ordered]@{
                evidence_root = $root
                attestation_locator = $SelectionAttestationLocator
                attestation_sha256 = $expectedSelectionHash
            }
            sample_every_n_presents = $SampleEveryNPresents
            max_main_passes_per_present = $MaxMainPassesPerPresent
            draw_window_start = $DrawWindowStart
            draw_window_count = $DrawWindowCount
        }
        if ($hasComparisonReference) {
            $cameraDiscovery.comparison_camera_reference = $comparisonReference
        }
        $config = [ordered]@{
            enabled = $true
            output_dir = $outputDirectory
            unbind_from_end = 4
            max_frames = $null
            present_only = $false
            watermark_only = $false
            writer_bypass = $false
            target_depth_hz = $null
            lane_fps = $null
            camera_probe = $true
            camera_discovery = $cameraDiscovery
            dsv_discovery = $null
            camera_production = $null
            depth_profile_path = $null
            depth_profile_sha256 = $null
            session_id = $sessionId
        }
        $configLeaf = "camera-config-$phase.json"
        $configPath = Join-Path $pendingDirectory $configLeaf
        Write-CreateNewUtf8Json -Path $configPath -Value $config
        $configHash = 'sha256:' + (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $phases += [ordered]@{
            phaseId = $phase
            configLocator = $configLeaf
            configSha256 = $configHash
            sealedReportLocator = "camera-candidates-$phase.json"
            operatorConfirmation = $phaseInstructions[$phase]
        }
    }

    $testScript = Join-Path $PSScriptRoot 'Test-GenericD3D11CameraDiscovery.ps1'
    $finalPlanPath = Join-Path $finalDirectory 'camera-discovery-plan.json'
    $reportPaths = @(
        (Join-Path $outputDirectory 'camera-candidates-POSE_STATIC.json'),
        (Join-Path $outputDirectory 'camera-candidates-POSE_WALK.json'),
        (Join-Path $outputDirectory 'camera-candidates-POSE_TURN.json')
    )
    $validatePlanCommand = [ordered]@{
        executable = 'powershell.exe'
        arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
            '-PlanPath', $finalPlanPath, '-DepthLabPath', $depthLab)
    }
    $validateReportsCommand = [ordered]@{
        executable = 'powershell.exe'
        arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
            '-PlanPath', $finalPlanPath, '-DepthLabPath', $depthLab,
            '-StaticReportPath', $reportPaths[0], '-WalkReportPath', $reportPaths[1],
            '-TurnReportPath', $reportPaths[2])
    }
    $phaseProtocol = @()
    for ($i = 0; $i -lt 3; $i++) {
        $phaseProtocol += [ordered]@{
            phaseId = $phaseOrder[$i]
            humanConfirmationRequired = $true
            configLocator = $phases[$i].configLocator
            sealInstruction = 'Use only the separately approved exact-owned controller to seal this phase; never delete evidence and never use sleep as action completion.'
            validateSealedReport = [ordered]@{
                executable = $depthLab
                arguments = @('camera-discovery-phase-validate', '--input', $reportPaths[$i],
                    '--expected-config', (Join-Path $finalDirectory $phases[$i].configLocator),
                    '--expected-config-sha256', $phases[$i].configSha256)
            }
        }
    }
    $offlineAnalysis = if ($hasComparisonReference) {
        [ordered]@{
            available = $true
            expectedReducerExitCode = 0
            outputSchema = 'camera-evidence-report/v2'
            capabilityCeiling = 'candidate_requires_e_geo'
            prerequisite = 'Run validateSealedReports successfully first.'
            executable = 'powershell.exe'
            arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
                '-PlanPath', $finalPlanPath, '-DepthLabPath', $depthLab,
                '-StaticReportPath', $reportPaths[0], '-WalkReportPath', $reportPaths[1],
                '-TurnReportPath', $reportPaths[2], '-RunOfflineAnalysis',
                '-AnalysisOutputPath', (Join-Path $outputDirectory 'camera-evidence-report.json'))
        }
    } else {
        [ordered]@{
            available = $true
            expectedReducerExitCode = 1
            outputSchema = 'camera-reference-free-candidate-review/v1'
            capabilityCeiling = 'human_review_candidates_only'
            prerequisite = 'Run validateSealedReports successfully first; exit=1 is the expected bridge-only result, not a selected camera.'
            executable = 'powershell.exe'
            arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
                '-PlanPath', $finalPlanPath, '-DepthLabPath', $depthLab,
                '-StaticReportPath', $reportPaths[0], '-WalkReportPath', $reportPaths[1],
                '-TurnReportPath', $reportPaths[2], '-RunOfflineAnalysis',
                '-AnalysisOutputPath', (Join-Path $outputDirectory 'camera-reference-free-candidate-review.json'))
        }
    }

    $planValue = [ordered]@{
        schema = 'generic-d3d11-camera-discovery-plan/v1'
        status = 'validated_plan_only'
        capabilityCeiling = 'camera_discovery_only'
        publicationAllowed = $false
        deliverable = $false
        recordingReady = $false
        target = [ordered]@{
            gameId = $GameId
            steamAppId = $AppId
            steamBuildId = $BuildId
            executable = [ordered]@{
                path = $exe
                basename = [IO.Path]::GetFileName($exe)
                sha256 = $expectedExeHash
            }
        }
        liveInstance = [ordered]@{
            processId = $ProcessId
            processStartTimeUtc = $processStart
        }
        gates = [ordered]@{
            runtimeEvidence = [ordered]@{ path = $runtime.path; sha256 = $runtime.sha256 }
            representativeScene = [ordered]@{ path = $scene.path; sha256 = $expectedSceneHash }
            dsvSelection = [ordered]@{
                evidenceRoot = $root
                attestationLocator = $SelectionAttestationLocator
                attestationSha256 = $expectedSelectionHash
            }
        }
        run = [ordered]@{
            planLeaf = $PlanLeaf
            discoveryId = $DiscoveryId
            outputLeaf = $OutputLeaf
            outputDir = $outputDirectory
            sessionId = $sessionId
        }
        selectionReplay = [ordered]@{
            target = $selectionResult.target
            selectedDescriptor = $selectionResult.selectedDescriptor
        }
        phaseOrder = $phaseOrder
        phases = $phases
        planSha256Locator = 'camera-discovery-plan.sha256'
        commands = [ordered]@{
            validatePlan = $validatePlanCommand
            phaseProtocol = $phaseProtocol
            validateSealedReports = $validateReportsCommand
            offlineAnalysis = $offlineAnalysis
        }
    }
    $pendingPlanPath = Join-Path $pendingDirectory 'camera-discovery-plan.json'
    Write-CreateNewUtf8Json -Path $pendingPlanPath -Value $planValue
    $pendingPlanHash = 'sha256:' + (Get-FileHash -LiteralPath $pendingPlanPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-CreateNewUtf8Line -Path (Join-Path $pendingDirectory 'camera-discovery-plan.sha256') -Value $pendingPlanHash

    $validationText = (& $testScript -PlanPath $pendingPlanPath -DepthLabPath $depthLab `
        -StagedPlanValidation | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "pending camera plan failed full gate replay.`n$validationText" }
    $validation = $validationText | ConvertFrom-Json
    if ([string]$validation.status -cne 'pass') { throw 'pending camera plan validator did not return pass.' }

    if ($DryRun) {
        Remove-DryRunDirectory -Root $root -Directory $pendingDirectory
        [pscustomobject][ordered]@{
            status = 'dry_run_pass'
            planSha256 = $pendingPlanHash
            planWouldBe = $finalPlanPath
            outputWouldBe = $outputDirectory
            gatesReplayed = $validation.gatesReplayed
            writesFixedProbeConfig = $false
            launchesOrDrivesGame = $false
            injectsModule = $false
            recordingReady = $false
        } | ConvertTo-Json -Depth 5
        exit 0
    }

    [IO.Directory]::Move($pendingDirectory, $finalDirectory)
    $publishedPlan = Join-Path $finalDirectory 'camera-discovery-plan.json'
    $publishedValidationText = (& $testScript -PlanPath $publishedPlan -DepthLabPath $depthLab | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "published plan failed its exact-directory gate replay; no config was activated.`n$publishedValidationText"
    }
    $publishedValidation = $publishedValidationText | ConvertFrom-Json
    if ([string]$publishedValidation.status -cne 'pass') {
        throw 'published camera plan validator did not return pass; no config was activated.'
    }
    [pscustomobject][ordered]@{
        status = 'validated_plan_created'
        planPath = $publishedPlan
        planSha256 = 'sha256:' + (Get-FileHash -LiteralPath $publishedPlan -Algorithm SHA256).Hash.ToLowerInvariant()
        outputDir = $outputDirectory
        gatesReplayed = $publishedValidation.gatesReplayed
        nextStep = 'A human must separately authorize guarded exact-instance loading, confirm each action, and validate every create-new sealed report before advancing.'
        writesFixedProbeConfig = $false
        launchesOrDrivesGame = $false
        injectsModule = $false
        recordingReady = $false
    } | ConvertTo-Json -Depth 5
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}

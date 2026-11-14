<#
.SYNOPSIS
  Validates a generic D3D11 camera-discovery v3 plan and optional sealed phase reports.

.DESCRIPTION
  This is a fail-closed control-plane validator. It replays the repository target, rights,
  Steam D6, runtime/protection, representative-scene, and DSV-selection gates. It never starts
  or drives a game, writes the fixed probe config, injects a module, or changes protection state.

  With all three report paths, it also asks depth-lab to validate each raw reliable v3 phase
  against its exact planned config, then enforces one selection descriptor/reference across the
  STATIC/WALK/TURN set. Offline reduction is opt-in: referenced evidence requires a selected v2
  result, while reference-free evidence requires an exit-1 human-review-only bridge artifact.
#>
[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$PlanPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$DepthLabPath,

    [Parameter(ParameterSetName = 'Validate')]
    [string]$StaticReportPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$WalkReportPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$TurnReportPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [switch]$RunOfflineAnalysis,

    [Parameter(ParameterSetName = 'Validate')]
    [string]$AnalysisOutputPath = '',

    [Parameter(ParameterSetName = 'Validate', DontShow = $true)]
    [switch]$StagedPlanValidation,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:PlanSchema = 'generic-d3d11-camera-discovery-plan/v1'
$script:ConfigValidationSchema = 'camera-discovery-config-validation/v1'
$script:PhaseValidationSchema = 'camera-discovery-phase-validation/v1'
$script:SelectionValidationSchema = 'd3d11-dsv-selection-validation/v1'
$script:PhaseOrder = @('POSE_STATIC', 'POSE_WALK', 'POSE_TURN')
$script:MaximumJsonBytes = 1MB

function Get-Field {
    param([AllowNull()]$Object, [Parameter(Mandatory = $true)][string]$Name)
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Assert-ExactProperties {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string[]]$Required,
        [string[]]$Optional = @(),
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($null -eq $Object) { throw "$Label must be an object." }
    $names = @($Object.PSObject.Properties.Name)
    foreach ($name in $Required) {
        if ($names -cnotcontains $name) { throw "$Label is missing required field '$name'." }
    }
    $allowed = @($Required + $Optional)
    foreach ($name in $names) {
        if ($allowed -cnotcontains $name) { throw "$Label contains unknown field '$name'." }
    }
}

function Assert-CanonicalSha256 {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [string] -or $Value -cnotmatch '^sha256:[0-9a-f]{64}$') {
        throw "$Label must be canonical lowercase sha256:<64 hex>."
    }
}

function Assert-ValidatorExactConfigHash {
    param(
        [AllowNull()]$Actual,
        [Parameter(Mandatory = $true)][string]$Expected,
        [Parameter(Mandatory = $true)][string]$Label
    )
    Assert-CanonicalSha256 $Expected "$Label expected config hash"
    if ($Actual -isnot [string] -or [string]$Actual -cne $Expected) {
        throw "$Label exactConfigSha256 does not bind the validated config bytes."
    }
}

function Assert-SafeLeaf {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory = $true)][string]$Label,
        [string]$Extension = ''
    )
    if ($Value -isnot [string] -or $Value -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$' -or
        $Value.Contains('\') -or $Value.Contains('/') -or $Value.Contains(':')) {
        throw "$Label must be one safe ASCII leaf."
    }
    if ($Extension -and -not $Value.EndsWith($Extension, [StringComparison]::Ordinal)) {
        throw "$Label must end with $Extension."
    }
}

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
    if (-not [IO.Path]::IsPathRooted($Path)) { throw "$Label must be an absolute path." }
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
        throw "$Label must be a direct child of $rootFull."
    }
    Assert-SafeLeaf -Value ([IO.Path]::GetFileName($pathFull)) -Label "$Label leaf"
}

function Read-VerifiedJson {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [string]$ExpectedSha256 = '',
        [Parameter(Mandatory = $true)][string]$Label
    )
    $full = Get-FullExistingPath -Path $Path -Label $Label -Kind Leaf
    $bytes = Read-BoundedStableBytes -Path $full -MaximumBytes $script:MaximumJsonBytes -Label $Label
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw "$Label must be UTF-8 without BOM."
    }
    $sha = Get-BytesSha256 -Bytes $bytes
    if ($ExpectedSha256 -and $sha -cne $ExpectedSha256) {
        throw "$Label exact-byte hash mismatch: expected=$ExpectedSha256 actual=$sha."
    }
    try {
        $value = [Text.UTF8Encoding]::new($false, $true).GetString($bytes) | ConvertFrom-Json
    } catch {
        throw "$Label is not strict UTF-8 JSON: $($_.Exception.Message)"
    }
    return [pscustomobject][ordered]@{ path = $full; sha256 = $sha; value = $value }
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
            throw "$Label byte length is outside 1..$MaximumBytes."
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

function Read-PlanWithSidecar {
    param([Parameter(Mandatory = $true)][string]$Path)
    $full = Get-FullExistingPath -Path $Path -Label 'camera discovery plan' -Kind Leaf
    if ([IO.Path]::GetFileName($full) -cne 'camera-discovery-plan.json') {
        throw 'camera discovery plan must use the fixed camera-discovery-plan.json locator.'
    }
    $sidecar = Join-Path ([IO.Path]::GetDirectoryName($full)) 'camera-discovery-plan.sha256'
    $sidecarFull = Get-FullExistingPath -Path $sidecar -Label 'camera discovery plan hash sidecar' -Kind Leaf
    $bytes = Read-BoundedStableBytes -Path $sidecarFull -MaximumBytes 72 `
        -Label 'camera discovery plan hash sidecar'
    if ($bytes.Length -ne 72 -or $bytes[$bytes.Length - 1] -ne 0x0A) {
        throw 'camera discovery plan hash sidecar must be one canonical SHA-256 line.'
    }
    try { $expected = [Text.UTF8Encoding]::new($false, $true).GetString($bytes, 0, $bytes.Length - 1) }
    catch { throw 'camera discovery plan hash sidecar is not strict UTF-8.' }
    Assert-CanonicalSha256 $expected 'camera discovery plan hash sidecar'
    return Read-VerifiedJson -Path $full -ExpectedSha256 $expected -Label 'camera discovery plan'
}

function ConvertTo-StableJson {
    param([AllowNull()]$Value)
    return ($Value | ConvertTo-Json -Depth 32 -Compress)
}

function Get-FixedConfigPath {
    $programData = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if (-not $programData) { throw 'CommonApplicationData is unavailable.' }
    return [IO.Path]::Combine($programData, 'DataMiner', 'depth-lab', 'probe-config.json')
}

function Assert-JsonEqual {
    param([AllowNull()]$Actual, [AllowNull()]$Expected, [Parameter(Mandatory = $true)][string]$Label)
    if ((ConvertTo-StableJson $Actual) -cne (ConvertTo-StableJson $Expected)) {
        throw "$Label differs."
    }
}

function Assert-ReferenceFreeReview {
    param([Parameter(Mandatory = $true)]$Review, [Parameter(Mandatory = $true)]$Plan)
    Assert-ExactProperties $Review @(
        'schema', 'status', 'phase_ids', 'executable_sha256', 'dsv_selection',
        'selected_dsv_descriptor', 'candidates', 'human_review_required',
        'metric_scale_available', 'automatic_selection_authorized', 'camera_binding_generated',
        'recording_ready', 'deliverable', 'publication_allowed',
        'requires_metric_scale_validation', 'requires_e_geo_validation',
        'production_binding_authorized'
    ) @() 'reference-free camera review'
    if ([string]$Review.schema -cne 'camera-reference-free-candidate-review/v1' -or
        [string]$Review.status -cne 'human_review_required' -or
        [string]$Review.executable_sha256 -cne [string]$Plan.target.executable.sha256 -or
        $Review.human_review_required -ne $true -or
        $Review.metric_scale_available -ne $false -or
        $Review.automatic_selection_authorized -ne $false -or
        $Review.camera_binding_generated -ne $false -or
        $Review.recording_ready -ne $false -or $Review.deliverable -ne $false -or
        $Review.publication_allowed -ne $false -or
        $Review.requires_metric_scale_validation -ne $true -or
        $Review.requires_e_geo_validation -ne $true -or
        $Review.production_binding_authorized -ne $false) {
        throw 'reference-free camera review exceeded its human-review-only ceiling.'
    }
    if ($Review.phase_ids -isnot [Array] -or @($Review.phase_ids).Count -ne 3) {
        throw 'reference-free camera review phase identity is invalid.'
    }
    for ($i = 0; $i -lt 3; $i++) {
        if ([string]$Review.phase_ids[$i] -cne $script:PhaseOrder[$i]) {
            throw 'reference-free camera review phase order differs from the plan.'
        }
    }
    Assert-JsonEqual $Review.dsv_selection `
        ([pscustomobject][ordered]@{
            evidence_root = $Plan.gates.dsvSelection.evidenceRoot
            attestation_locator = $Plan.gates.dsvSelection.attestationLocator
            attestation_sha256 = $Plan.gates.dsvSelection.attestationSha256
        }) 'reference-free review selection binding'
    Assert-JsonEqual $Review.selected_dsv_descriptor $Plan.selectionReplay.selectedDescriptor `
        'reference-free review descriptor'
    if ($Review.PSObject.Properties.Name -ccontains 'selection') {
        throw 'reference-free review must not contain a selected camera source.'
    }
    if ($Review.candidates -isnot [Array]) {
        throw 'reference-free review candidates must be an array.'
    }
    foreach ($candidate in @($Review.candidates)) {
        Assert-ExactProperties $candidate @('source', 'static_motion', 'walk_motion', 'turn_motion') @() `
            'reference-free candidate'
        Assert-ExactProperties $candidate.source @(
            'shader_stage', 'bind_slot', 'pass_ordinal', 'draw_ordinal', 'buffer_bytes',
            'offset', 'encoding', 'layout', 'transpose', 'role', 'translation'
        ) @() 'reference-free candidate.source'
        if ([string]$candidate.source.shader_stage -cne 'vertex' -or
            [string]$candidate.source.encoding -cne 'f32_little_endian' -or
            @('row_major', 'column_major') -cnotcontains [string]$candidate.source.layout -or
            [string]$candidate.source.role -cne 'view' -or
            @('row3', 'col3') -cnotcontains [string]$candidate.source.translation -or
            $candidate.source.transpose -isnot [bool]) {
            throw 'reference-free candidate source identity is invalid.'
        }
        foreach ($number in @('bind_slot', 'pass_ordinal', 'draw_ordinal', 'buffer_bytes', 'offset')) {
            if ($candidate.source.$number -isnot [ValueType] -or $candidate.source.$number -is [bool] -or
                [int64]$candidate.source.$number -lt 0 -or [int64]$candidate.source.$number -gt [uint32]::MaxValue) {
                throw "reference-free candidate.source.$number is not a u32."
            }
        }
        foreach ($motionName in @('static_motion', 'walk_motion', 'turn_motion')) {
            $motion = $candidate.$motionName
            Assert-ExactProperties $motion @(
                'samples', 'steps', 'translation_median', 'translation_p95',
                'rotation_median_rad', 'rotation_p95_rad'
            ) @() "reference-free candidate.$motionName"
            $sampleCount = [uint64]$motion.samples
            $maximumSteps = if ($sampleCount -gt 0) { $sampleCount - 1 } else { [uint64]0 }
            if ($motion.samples -isnot [ValueType] -or $motion.samples -is [bool] -or
                $motion.steps -isnot [ValueType] -or $motion.steps -is [bool] -or
                [int64]$motion.samples -lt 0 -or [int64]$motion.steps -lt 0 -or
                [uint64]$motion.steps -gt $maximumSteps) {
                throw "reference-free candidate.$motionName sample/step counts are invalid."
            }
            foreach ($field in @('translation_median', 'translation_p95', 'rotation_median_rad', 'rotation_p95_rad')) {
                if ($motion.$field -isnot [ValueType] -or $motion.$field -is [bool]) {
                    throw "reference-free candidate.$motionName.$field must be numeric."
                }
                $value = [double]$motion.$field
                if ([double]::IsNaN($value) -or [double]::IsInfinity($value) -or $value -lt 0.0) {
                    throw "reference-free candidate.$motionName.$field must be finite and non-negative."
                }
            }
        }
    }
}

function Assert-ReferencedEvidenceReport {
    param([Parameter(Mandatory = $true)]$Report, [Parameter(Mandatory = $true)]$Plan)
    Assert-ExactProperties $Report @(
        'schema', 'phase_ids', 'executable_sha256', 'candidates',
        'legacy_reference_static_motion', 'legacy_reference_walk_motion',
        'legacy_reference_turn_motion', 'selection', 'requires_e_geo_validation',
        'production_binding_authorized'
    ) @() 'referenced camera evidence report'
    if ([string]$Report.schema -cne 'camera-evidence-report/v2' -or
        [string]$Report.executable_sha256 -cne [string]$Plan.target.executable.sha256 -or
        [string]$Report.selection.status -cne 'selected' -or
        $Report.requires_e_geo_validation -ne $true -or
        $Report.production_binding_authorized -ne $false) {
        throw 'referenced camera evidence output is not a selected v2 authoring candidate.'
    }
}

function Assert-OfflineReducerOutcome {
    param(
        [Parameter(Mandatory = $true)][bool]$HasReference,
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)]$Document,
        [Parameter(Mandatory = $true)]$Plan
    )
    if ($HasReference) {
        if ($ExitCode -ne 0) {
            throw "referenced camera-evidence must select one v2 candidate with exit=0; actual=$ExitCode."
        }
        Assert-ReferencedEvidenceReport -Report $Document -Plan $Plan
    } else {
        if ($ExitCode -ne 1) {
            throw "reference-free camera-evidence must emit bridge-only exit=1; actual=$ExitCode."
        }
        Assert-ReferenceFreeReview -Review $Document -Plan $Plan
    }
}

function Assert-Descriptor {
    param([AllowNull()]$Descriptor, [Parameter(Mandatory = $true)][string]$Label)
    Assert-ExactProperties $Descriptor @(
        'view_format', 'view_dimension', 'view_flags', 'mip_slice', 'first_array_slice',
        'view_array_size', 'resource_format', 'width', 'height', 'resource_mip_levels',
        'resource_array_size', 'sample_count', 'sample_quality', 'usage', 'bind_flags',
        'cpu_access_flags', 'misc_flags'
    ) @() $Label
    foreach ($name in $Descriptor.PSObject.Properties.Name) {
        if ($Descriptor.$name -isnot [ValueType]) { throw "$Label.$name must be numeric." }
    }
}

function Assert-TargetIdentity {
    param([AllowNull()]$Target, [Parameter(Mandatory = $true)][string]$Label)
    Assert-ExactProperties $Target @('gameId', 'steamAppId', 'steamBuildId', 'executable') @() $Label
    if ([string]::IsNullOrWhiteSpace([string]$Target.gameId) -or
        [string]$Target.steamAppId -cnotmatch '^\d+$' -or
        [string]$Target.steamBuildId -cnotmatch '^\d+$') {
        throw "$Label game/App/build identity is invalid."
    }
    Assert-ExactProperties $Target.executable @('path', 'basename', 'sha256') @() "$Label.executable"
    if (-not [IO.Path]::IsPathRooted([string]$Target.executable.path)) {
        throw "$Label.executable.path must be absolute."
    }
    if ([string]$Target.executable.basename -cnotmatch '^[^\\/:*?"<>|]+\.exe$') {
        throw "$Label.executable.basename is invalid."
    }
    Assert-CanonicalSha256 $Target.executable.sha256 "$Label.executable.sha256"
}

function Assert-PhaseConfig {
    param(
        [Parameter(Mandatory = $true)]$Config,
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)][string]$ExpectedPhase
    )
    Assert-ExactProperties $Config @(
        'enabled', 'output_dir', 'unbind_from_end', 'max_frames', 'present_only',
        'watermark_only', 'writer_bypass', 'target_depth_hz', 'lane_fps', 'camera_probe',
        'camera_discovery', 'dsv_discovery', 'camera_production', 'depth_profile_path',
        'depth_profile_sha256', 'session_id'
    ) @() "config[$ExpectedPhase]"
    if ($Config.enabled -isnot [bool] -or -not $Config.enabled -or
        $Config.camera_probe -isnot [bool] -or -not $Config.camera_probe -or
        $Config.present_only -ne $false -or $Config.watermark_only -ne $false -or
        $Config.writer_bypass -ne $false -or $null -ne $Config.max_frames -or
        $null -ne $Config.target_depth_hz -or $null -ne $Config.lane_fps -or
        $null -ne $Config.dsv_discovery -or $null -ne $Config.camera_production -or
        $null -ne $Config.depth_profile_path -or $null -ne $Config.depth_profile_sha256) {
        throw "config[$ExpectedPhase] is not discovery-only."
    }
    if ([string]$Config.output_dir -cne [string]$Plan.run.outputDir -or
        [string]$Config.session_id -cne [string]$Plan.run.sessionId -or
        [int64]$Config.unbind_from_end -ne 4) {
        throw "config[$ExpectedPhase] run identity differs from the plan."
    }
    Assert-ExactProperties $Config.camera_discovery @(
        'schema', 'phase_id', 'executable_sha256', 'dsv_selection',
        'sample_every_n_presents', 'max_main_passes_per_present',
        'draw_window_start', 'draw_window_count'
    ) @('comparison_camera_reference') "config[$ExpectedPhase].camera_discovery"
    $camera = $Config.camera_discovery
    if ([string]$camera.schema -cne 'camera-discovery-config/v3' -or
        [string]$camera.phase_id -cne $ExpectedPhase -or
        [string]$camera.executable_sha256 -cne [string]$Plan.target.executable.sha256) {
        throw "config[$ExpectedPhase] is not the exact planned v3 phase."
    }
    if ($camera.PSObject.Properties.Name -ccontains 'render') {
        throw "config[$ExpectedPhase] carries forbidden legacy render authority."
    }
    Assert-ExactProperties $camera.dsv_selection @(
        'evidence_root', 'attestation_locator', 'attestation_sha256'
    ) @() "config[$ExpectedPhase].camera_discovery.dsv_selection"
    if ([string]$camera.dsv_selection.evidence_root -cne [string]$Plan.gates.dsvSelection.evidenceRoot -or
        [string]$camera.dsv_selection.attestation_locator -cne [string]$Plan.gates.dsvSelection.attestationLocator -or
        [string]$camera.dsv_selection.attestation_sha256 -cne [string]$Plan.gates.dsvSelection.attestationSha256) {
        throw "config[$ExpectedPhase] selection binding differs from the plan."
    }
    $sample = [int64]$camera.sample_every_n_presents
    $passes = [int64]$camera.max_main_passes_per_present
    $start = [int64]$camera.draw_window_start
    $count = [int64]$camera.draw_window_count
    if ($sample -lt 1 -or $passes -lt 1 -or $passes -gt 16 -or $start -lt 0 -or
        $count -lt 1 -or $count -gt 256 -or ($passes * $count) -gt 512 -or
        ($start + $count) -gt 512) {
        throw "config[$ExpectedPhase] sampling envelope is invalid."
    }
}

function Assert-PlanCore {
    param([Parameter(Mandatory = $true)]$Plan, [Parameter(Mandatory = $true)]$Configs)
    Assert-ExactProperties $Plan @(
        'schema', 'status', 'capabilityCeiling', 'publicationAllowed', 'deliverable',
        'recordingReady', 'target', 'liveInstance', 'gates', 'run', 'selectionReplay',
        'phaseOrder', 'phases', 'planSha256Locator', 'commands'
    ) @() 'plan'
    if ([string]$Plan.schema -cne $script:PlanSchema -or
        [string]$Plan.status -cne 'validated_plan_only' -or
        [string]$Plan.capabilityCeiling -cne 'camera_discovery_only' -or
        $Plan.publicationAllowed -ne $false -or $Plan.deliverable -ne $false -or
        $Plan.recordingReady -ne $false) {
        throw 'plan overclaims its discovery-only authority.'
    }
    if ([string]$Plan.planSha256Locator -cne 'camera-discovery-plan.sha256') {
        throw 'plan must bind the fixed camera-discovery-plan.sha256 sidecar locator.'
    }
    Assert-TargetIdentity $Plan.target 'plan.target'
    Assert-ExactProperties $Plan.liveInstance @('processId', 'processStartTimeUtc') @() 'plan.liveInstance'
    if ([int64]$Plan.liveInstance.processId -lt 1 -or
        [string]$Plan.liveInstance.processStartTimeUtc -cnotmatch 'Z$') {
        throw 'plan.liveInstance is invalid.'
    }
    Assert-ExactProperties $Plan.gates @('runtimeEvidence', 'representativeScene', 'dsvSelection') @() 'plan.gates'
    foreach ($bindingName in @('runtimeEvidence', 'representativeScene')) {
        $binding = $Plan.gates.$bindingName
        Assert-ExactProperties $binding @('path', 'sha256') @() "plan.gates.$bindingName"
        if (-not [IO.Path]::IsPathRooted([string]$binding.path)) {
            throw "plan.gates.$bindingName.path must be absolute."
        }
        Assert-CanonicalSha256 $binding.sha256 "plan.gates.$bindingName.sha256"
    }
    Assert-ExactProperties $Plan.gates.dsvSelection @(
        'evidenceRoot', 'attestationLocator', 'attestationSha256'
    ) @() 'plan.gates.dsvSelection'
    if (-not [IO.Path]::IsPathRooted([string]$Plan.gates.dsvSelection.evidenceRoot)) {
        throw 'plan.gates.dsvSelection.evidenceRoot must be absolute.'
    }
    Assert-SafeLeaf $Plan.gates.dsvSelection.attestationLocator 'selection locator' '.json'
    Assert-CanonicalSha256 $Plan.gates.dsvSelection.attestationSha256 'selection hash'
    Assert-ExactProperties $Plan.run @(
        'planLeaf', 'discoveryId', 'outputLeaf', 'outputDir', 'sessionId'
    ) @() 'plan.run'
    foreach ($name in @('planLeaf', 'discoveryId', 'outputLeaf', 'sessionId')) {
        Assert-SafeLeaf $Plan.run.$name "plan.run.$name"
    }
    if ([string]$Plan.run.planLeaf -ieq [string]$Plan.run.outputLeaf) {
        throw 'planLeaf and outputLeaf must be distinct under Windows ordinal-ignore-case semantics.'
    }
    if (-not [IO.Path]::IsPathRooted([string]$Plan.run.outputDir)) {
        throw 'plan.run.outputDir must be absolute.'
    }
    Assert-DirectChildPath -Root $Plan.gates.dsvSelection.evidenceRoot -Path $Plan.run.outputDir -Label 'camera output'
    Assert-ExactProperties $Plan.selectionReplay @('target', 'selectedDescriptor') @() 'plan.selectionReplay'
    $replayTarget = [pscustomobject][ordered]@{
        gameId = $Plan.target.gameId
        steamAppId = $Plan.target.steamAppId
        steamBuildId = $Plan.target.steamBuildId
        executable = [pscustomobject][ordered]@{
            basename = $Plan.target.executable.basename
            sha256 = $Plan.target.executable.sha256
        }
    }
    Assert-JsonEqual $Plan.selectionReplay.target $replayTarget 'selection replay target'
    Assert-Descriptor $Plan.selectionReplay.selectedDescriptor 'plan.selectionReplay.selectedDescriptor'
    if ($Plan.phaseOrder -isnot [Array] -or @($Plan.phaseOrder).Count -ne 3) {
        throw 'plan.phaseOrder must contain exactly three phases.'
    }
    for ($i = 0; $i -lt 3; $i++) {
        if ([string]$Plan.phaseOrder[$i] -cne $script:PhaseOrder[$i]) {
            throw 'plan phase order must be POSE_STATIC -> POSE_WALK -> POSE_TURN.'
        }
    }
    if ($Plan.phases -isnot [Array] -or @($Plan.phases).Count -ne 3) {
        throw 'plan.phases must contain exactly three entries.'
    }
    $baselineReference = $null
    $baselineReferencePresent = $false
    $expectedInstructions = @{
        POSE_STATIC = 'Human confirms the representative offline single-player scene, then remains fully static. Do not infer completion from elapsed time.'
        POSE_WALK = 'After the prior create-new sealed report passes validation, human confirms straight walking without turning the view.'
        POSE_TURN = 'After the prior create-new sealed report passes validation, human confirms turning in place without walking.'
    }
    for ($i = 0; $i -lt 3; $i++) {
        $phaseId = $script:PhaseOrder[$i]
        $phase = $Plan.phases[$i]
        Assert-ExactProperties $phase @(
            'phaseId', 'configLocator', 'configSha256', 'sealedReportLocator',
            'operatorConfirmation'
        ) @() "plan.phases[$i]"
        if ([string]$phase.phaseId -cne $phaseId) { throw "phase[$i] order differs." }
        Assert-SafeLeaf $phase.configLocator "phase[$i].configLocator" '.json'
        Assert-SafeLeaf $phase.sealedReportLocator "phase[$i].sealedReportLocator" '.json'
        Assert-CanonicalSha256 $phase.configSha256 "phase[$i].configSha256"
        if ([string]$phase.operatorConfirmation -cne [string]$expectedInstructions[$phaseId]) {
            throw "phase[$i] human confirmation instruction differs from the fixed contract."
        }
        if (-not $Configs.ContainsKey($phaseId)) { throw "config[$phaseId] is missing." }
        $config = $Configs[$phaseId]
        Assert-PhaseConfig -Config $config -Plan $Plan -ExpectedPhase $phaseId
        $referenceProperty = $config.camera_discovery.PSObject.Properties['comparison_camera_reference']
        $present = $null -ne $referenceProperty
        $reference = if ($present) { $referenceProperty.Value } else { $null }
        if ($present -and $null -eq $reference) {
            throw 'comparison_camera_reference must be omitted rather than null.'
        }
        if ($i -eq 0) {
            $baselineReferencePresent = $present
            $baselineReference = $reference
        } elseif ($present -ne $baselineReferencePresent -or
            (ConvertTo-StableJson $reference) -cne (ConvertTo-StableJson $baselineReference)) {
            throw 'mixed comparison_camera_reference values across phases are forbidden.'
        }
    }
    Assert-ExactProperties $Plan.commands @(
        'validatePlan', 'phaseProtocol', 'validateSealedReports', 'offlineAnalysis'
    ) @() 'plan.commands'
    if ($Plan.commands.phaseProtocol -isnot [Array] -or @($Plan.commands.phaseProtocol).Count -ne 3) {
        throw 'plan.commands.phaseProtocol must contain exactly three human-gated entries.'
    }
    for ($i = 0; $i -lt 3; $i++) {
        $protocol = $Plan.commands.phaseProtocol[$i]
        Assert-ExactProperties $protocol @(
            'phaseId', 'humanConfirmationRequired', 'configLocator', 'sealInstruction',
            'validateSealedReport'
        ) @() "plan.commands.phaseProtocol[$i]"
        if ([string]$protocol.phaseId -cne $script:PhaseOrder[$i] -or
            $protocol.humanConfirmationRequired -ne $true -or
            [string]$protocol.configLocator -cne [string]$Plan.phases[$i].configLocator -or
            [string]::IsNullOrWhiteSpace([string]$protocol.sealInstruction)) {
            throw "plan.commands.phaseProtocol[$i] weakens the human/seal boundary."
        }
        Assert-ExactProperties $protocol.validateSealedReport @('executable', 'arguments') @() `
            "plan.commands.phaseProtocol[$i].validateSealedReport"
        if (@($protocol.validateSealedReport.arguments) -cnotcontains 'camera-discovery-phase-validate') {
            throw "plan.commands.phaseProtocol[$i] does not validate the sealed v3 report."
        }
    }
    $commandJson = ConvertTo-StableJson $Plan.commands
    if ($commandJson -match '(?i)dm-depth-inject|LoadLibrary|SendInput|keybd_event|mouse_event') {
        throw 'plan commands must not inject a module or synthesize input.'
    }
    $offline = $Plan.commands.offlineAnalysis
    Assert-ExactProperties $offline @(
        'available', 'expectedReducerExitCode', 'outputSchema', 'capabilityCeiling',
        'prerequisite', 'executable', 'arguments'
    ) @() 'plan.commands.offlineAnalysis'
    if ($offline.available -ne $true -or
        @($offline.arguments).Count -lt 2 -or
        @($offline.arguments) -cnotcontains '-RunOfflineAnalysis') {
        throw 'plan.commands.offlineAnalysis does not use the validating offline wrapper.'
    }
    if ($baselineReferencePresent) {
        if ([int]$offline.expectedReducerExitCode -ne 0 -or
            [string]$offline.outputSchema -cne 'camera-evidence-report/v2' -or
            [string]$offline.capabilityCeiling -cne 'candidate_requires_e_geo') {
            throw 'referenced offline analysis command has the wrong exit/schema ceiling.'
        }
    } elseif ([int]$offline.expectedReducerExitCode -ne 1 -or
        [string]$offline.outputSchema -cne 'camera-reference-free-candidate-review/v1' -or
        [string]$offline.capabilityCeiling -cne 'human_review_candidates_only') {
        throw 'reference-free offline analysis must preserve the bridge-only exit/schema ceiling.'
    }
}

function New-ExpectedCommands {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)]$Configs,
        [Parameter(Mandatory = $true)][string]$DepthLab
    )
    $testScript = Join-Path $PSScriptRoot 'Test-GenericD3D11CameraDiscovery.ps1'
    $finalDirectory = Join-Path ([string]$Plan.gates.dsvSelection.evidenceRoot) ([string]$Plan.run.planLeaf)
    $finalPlanPath = Join-Path $finalDirectory 'camera-discovery-plan.json'
    $reportPaths = @(
        (Join-Path ([string]$Plan.run.outputDir) 'camera-candidates-POSE_STATIC.json'),
        (Join-Path ([string]$Plan.run.outputDir) 'camera-candidates-POSE_WALK.json'),
        (Join-Path ([string]$Plan.run.outputDir) 'camera-candidates-POSE_TURN.json')
    )
    $validatePlan = [ordered]@{
        executable = 'powershell.exe'
        arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
            '-PlanPath', $finalPlanPath, '-DepthLabPath', $DepthLab)
    }
    $validateReports = [ordered]@{
        executable = 'powershell.exe'
        arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
            '-PlanPath', $finalPlanPath, '-DepthLabPath', $DepthLab,
            '-StaticReportPath', $reportPaths[0], '-WalkReportPath', $reportPaths[1],
            '-TurnReportPath', $reportPaths[2])
    }
    $phaseProtocol = @()
    for ($i = 0; $i -lt 3; $i++) {
        $phaseProtocol += [ordered]@{
            phaseId = $script:PhaseOrder[$i]
            humanConfirmationRequired = $true
            configLocator = [string]$Plan.phases[$i].configLocator
            sealInstruction = 'Use only the separately approved exact-owned controller to seal this phase; never delete evidence and never use sleep as action completion.'
            validateSealedReport = [ordered]@{
                executable = $DepthLab
                arguments = @('camera-discovery-phase-validate', '--input', $reportPaths[$i],
                    '--expected-config', (Join-Path $finalDirectory ([string]$Plan.phases[$i].configLocator)),
                    '--expected-config-sha256', [string]$Plan.phases[$i].configSha256)
            }
        }
    }
    $hasReference = $null -ne $Configs['POSE_STATIC'].camera_discovery.PSObject.Properties['comparison_camera_reference']
    $analysisLeaf = if ($hasReference) { 'camera-evidence-report.json' } else { 'camera-reference-free-candidate-review.json' }
    $offline = [ordered]@{
        available = $true
        expectedReducerExitCode = if ($hasReference) { 0 } else { 1 }
        outputSchema = if ($hasReference) { 'camera-evidence-report/v2' } else { 'camera-reference-free-candidate-review/v1' }
        capabilityCeiling = if ($hasReference) { 'candidate_requires_e_geo' } else { 'human_review_candidates_only' }
        prerequisite = if ($hasReference) {
            'Run validateSealedReports successfully first.'
        } else {
            'Run validateSealedReports successfully first; exit=1 is the expected bridge-only result, not a selected camera.'
        }
        executable = 'powershell.exe'
        arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $testScript,
            '-PlanPath', $finalPlanPath, '-DepthLabPath', $DepthLab,
            '-StaticReportPath', $reportPaths[0], '-WalkReportPath', $reportPaths[1],
            '-TurnReportPath', $reportPaths[2], '-RunOfflineAnalysis',
            '-AnalysisOutputPath', (Join-Path ([string]$Plan.run.outputDir) $analysisLeaf))
    }
    return [pscustomobject][ordered]@{
        validatePlan = $validatePlan
        phaseProtocol = $phaseProtocol
        validateSealedReports = $validateReports
        offlineAnalysis = $offline
    }
}

function Assert-CommandContract {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)]$Configs,
        [Parameter(Mandatory = $true)][string]$DepthLab
    )
    $expected = New-ExpectedCommands -Plan $Plan -Configs $Configs -DepthLab $DepthLab
    Assert-JsonEqual $Plan.commands $expected 'plan commands exact argv contract'
}

function Assert-PlanDirectoryBinding {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)][string]$ActualDirectory,
        [switch]$AllowStaged
    )
    $root = [IO.Path]::GetFullPath([string]$Plan.gates.dsvSelection.evidenceRoot).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $actual = [IO.Path]::GetFullPath($ActualDirectory).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $expected = [IO.Path]::GetFullPath((Join-Path $root ([string]$Plan.run.planLeaf))).TrimEnd([IO.Path]::DirectorySeparatorChar)
    if (-not $AllowStaged) {
        if (-not $actual.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'PlanPath parent must be the exact evidenceRoot/planLeaf directory.'
        }
        return
    }
    $leaf = [IO.Path]::GetFileName($actual)
    if (-not [IO.Path]::GetDirectoryName($actual).Equals($root, [StringComparison]::OrdinalIgnoreCase) -or
        $leaf -cnotmatch '^\.camera-plan-pending-[0-9a-f]{32}$') {
        throw 'staged plan validation is restricted to the planner-owned direct-child pending directory.'
    }
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

function Invoke-ConfigValidation {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)]$Configs,
        [Parameter(Mandatory = $true)][string]$PlanDirectory,
        [Parameter(Mandatory = $true)][string]$DepthLab
    )
    foreach ($phase in $Plan.phases) {
        $phaseId = [string]$phase.phaseId
        $config = $Configs[$phaseId]
        $path = Join-Path $PlanDirectory ([string]$phase.configLocator)
        $result = Invoke-JsonCommand -Executable $DepthLab -Label "$phaseId typed config validation" -Arguments @(
            'camera-discovery-config-validate', '--input', $path,
            '--expected-sha256', [string]$phase.configSha256
        )
        Assert-ExactProperties $result @(
            'schema', 'status', 'exactConfigSha256', 'phaseId', 'executableSha256', 'dsvSelection',
            'selectedDsvDescriptor', 'comparisonCameraReferencePresent', 'capabilityCeiling',
            'automaticSelectionAuthorized', 'productionBindingAuthorized', 'publicationAllowed',
            'recordingReady'
        ) @() "$phaseId typed config validation result"
        Assert-ValidatorExactConfigHash $result.exactConfigSha256 ([string]$phase.configSha256) `
            "$phaseId typed config validation result"
        $expectedReferencePresent = $null -ne $config.camera_discovery.PSObject.Properties['comparison_camera_reference']
        if ([string]$result.schema -cne $script:ConfigValidationSchema -or
            [string]$result.status -cne 'pass' -or
            [string]$result.phaseId -cne $phaseId -or
            [string]$result.executableSha256 -cne [string]$Plan.target.executable.sha256 -or
            [bool]$result.comparisonCameraReferencePresent -ne $expectedReferencePresent -or
            [string]$result.capabilityCeiling -cne 'camera_discovery_only' -or
            $result.automaticSelectionAuthorized -ne $false -or
            $result.productionBindingAuthorized -ne $false -or
            $result.publicationAllowed -ne $false -or $result.recordingReady -ne $false) {
            throw "$phaseId typed config validator returned a drifted or overclaimed identity."
        }
        Assert-JsonEqual $result.dsvSelection $config.camera_discovery.dsv_selection `
            "$phaseId typed config selection"
        Assert-JsonEqual $result.selectedDsvDescriptor $Plan.selectionReplay.selectedDescriptor `
            "$phaseId typed config replayed descriptor"
    }
}

function Assert-ReplayedGates {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)]$TargetResult,
        [Parameter(Mandatory = $true)]$RightsResult,
        [Parameter(Mandatory = $true)]$D6Result,
        [Parameter(Mandatory = $true)]$SceneResult,
        [Parameter(Mandatory = $true)]$SelectionResult
    )
    if ([string]$TargetResult.status -cne 'pass' -or $null -eq $TargetResult.selected_target) {
        throw 'target authorization gate did not pass.'
    }
    $target = $TargetResult.selected_target
    if ([string]$target.gameId -cne [string]$Plan.target.gameId -or
        [string]$target.appId -cne [string]$Plan.target.steamAppId -or
        [string]$target.steamBuildId -cne [string]$Plan.target.steamBuildId -or
        -not ([string]$target.executableBasename).Equals([string]$Plan.target.executable.basename, [StringComparison]::OrdinalIgnoreCase) -or
        [string]$target.executableSha256 -cne [string]$Plan.target.executable.sha256) {
        throw 'target authorization result differs from the plan.'
    }
    if ([string]$RightsResult.status -cne 'pass' -or
        -not [bool]$RightsResult.publisher_or_counsel_approval -or
        [int]$RightsResult.selected_record_count -ne 1 -or
        -not [bool]$RightsResult.runtime_anti_cheat_gate_still_required) {
        throw 'rights gate did not pass with the runtime protection gate intact.'
    }
    if ([string]$D6Result.status -cne 'install_gate_passed' -or
        [string]$D6Result.app_id -cne [string]$Plan.target.steamAppId -or
        [string]$D6Result.build_id -cne [string]$Plan.target.steamBuildId) {
        throw 'Steam D6 gate differs from the planned AppID/build.'
    }
    $matching = @($D6Result.executable_candidates | Where-Object {
        [string]$_.sha256 -ceq [string]$Plan.target.executable.sha256 -and
        ([IO.Path]::GetFileName([string]$_.relative_path)).Equals(
            [string]$Plan.target.executable.basename, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($matching.Count -ne 1) { throw 'Steam D6 gate did not return one exact executable candidate.' }
    if ([string]$SceneResult.status -cne 'pass' -or
        [string]$SceneResult.attestation_sha256 -cne [string]$Plan.gates.representativeScene.sha256 -or
        [string]$SceneResult.game_id -cne [string]$Plan.target.gameId -or
        [string]$SceneResult.discovery_id -cne [string]$Plan.run.discoveryId -or
        [string]$SceneResult.output_leaf -cne [string]$Plan.run.outputLeaf -or
        [string]$SceneResult.scene_kind -cne 'representative_offline_single_player') {
        throw 'representative-scene/runtime/protection replay differs from the plan.'
    }
    if ([string]$SelectionResult.schema -cne $script:SelectionValidationSchema -or
        [string]$SelectionResult.status -cne 'pass' -or
        [string]$SelectionResult.capabilityCeiling -cne 'dsv_selection_bridge_only' -or
        $SelectionResult.publicationAllowed -ne $false -or $SelectionResult.recordingReady -ne $false -or
        [string]$SelectionResult.locator -cne [string]$Plan.gates.dsvSelection.attestationLocator -or
        [string]$SelectionResult.exactBytesSha256 -cne [string]$Plan.gates.dsvSelection.attestationSha256) {
        throw 'DSV selection source replay differs from the plan.'
    }
    Assert-JsonEqual $SelectionResult.target $Plan.selectionReplay.target 'replayed selection target'
    Assert-JsonEqual $SelectionResult.selectedDescriptor $Plan.selectionReplay.selectedDescriptor 'replayed selection descriptor'
}

function Invoke-LiveGateReplay {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)][string]$DepthLab
    )
    $targetScript = Join-Path $PSScriptRoot 'Test-GenericD3D11DsvDiscoveryTargets.ps1'
    $rightsScript = Join-Path $PSScriptRoot 'Test-GameDepthRightsGate.ps1'
    $d6Script = Join-Path (Split-Path -Parent $PSScriptRoot) 'Test-SteamD6InstallGate.ps1'
    $sceneScript = Join-Path $PSScriptRoot 'Test-GenericD3D11RepresentativeSceneAttestation.ps1'
    $targetResult = Invoke-JsonCommand -Executable $targetScript -Label 'target registry replay' -Arguments @(
        '-GameId', [string]$Plan.target.gameId,
        '-AppId', [string]$Plan.target.steamAppId,
        '-BuildId', [string]$Plan.target.steamBuildId,
        '-ExecutableBasename', [string]$Plan.target.executable.basename,
        '-ExecutableSha256', [string]$Plan.target.executable.sha256
    )
    $rightsResult = Invoke-JsonCommand -Executable $rightsScript -Label 'rights replay' -Arguments @(
        '-AppId', [string]$Plan.target.steamAppId
    )
    $d6Result = Invoke-JsonCommand -Executable $d6Script -Label 'Steam D6 replay' -Arguments @(
        '-AppId', [string]$Plan.target.steamAppId
    )
    $target = $targetResult.selected_target
    if ($null -eq $target) { throw 'target replay returned no selected target.' }
    $sceneGate = $target.representativeSceneGate
    $sceneResult = Invoke-JsonCommand -Executable $sceneScript -Label 'scene/runtime/protection replay' -Arguments @(
        '-AttestationPath', [string]$Plan.gates.representativeScene.path,
        '-ExpectedAttestationSha256', [string]$Plan.gates.representativeScene.sha256,
        '-GameId', [string]$Plan.target.gameId,
        '-AppId', [string]$Plan.target.steamAppId,
        '-BuildId', [string]$Plan.target.steamBuildId,
        '-ExpectedDiscoveryId', [string]$Plan.run.discoveryId,
        '-ExpectedOutputLeaf', [string]$Plan.run.outputLeaf,
        '-ExecutableBasename', [string]$Plan.target.executable.basename,
        '-ExecutableSha256', [string]$Plan.target.executable.sha256,
        '-RuntimeEvidencePath', [string]$Plan.gates.runtimeEvidence.path,
        '-ExpectedProcessId', [string]$Plan.liveInstance.processId,
        '-ExpectedProcessStartTimeUtc', [string]$Plan.liveInstance.processStartTimeUtc,
        '-ExpectedWindowClass', [string]$sceneGate.expectedWindow.className,
        '-ExpectedClientWidth', [string]$sceneGate.expectedWindow.clientWidth,
        '-ExpectedClientHeight', [string]$sceneGate.expectedWindow.clientHeight,
        '-MaximumAgeSeconds', [string]$sceneGate.maximumAgeSeconds
    )
    $selectionResult = Invoke-JsonCommand -Executable $DepthLab -Label 'DSV selection source replay' -Arguments @(
        'dsv-selection-validate',
        '--evidence-root', [string]$Plan.gates.dsvSelection.evidenceRoot,
        '--input', (Join-Path ([string]$Plan.gates.dsvSelection.evidenceRoot) ([string]$Plan.gates.dsvSelection.attestationLocator)),
        '--expected-locator', [string]$Plan.gates.dsvSelection.attestationLocator,
        '--expected-sha256', [string]$Plan.gates.dsvSelection.attestationSha256,
        '--expected-executable-sha256', [string]$Plan.target.executable.sha256
    )
    Assert-ReplayedGates -Plan $Plan -TargetResult $targetResult -RightsResult $rightsResult `
        -D6Result $d6Result -SceneResult $sceneResult -SelectionResult $selectionResult
}

function Invoke-PhaseReportValidation {
    param(
        [Parameter(Mandatory = $true)]$Plan,
        [Parameter(Mandatory = $true)]$Configs,
        [Parameter(Mandatory = $true)][string]$PlanDirectory,
        [Parameter(Mandatory = $true)][string]$DepthLab,
        [Parameter(Mandatory = $true)][string[]]$ReportPaths
    )
    $summaries = @()
    for ($i = 0; $i -lt 3; $i++) {
        $phase = $Plan.phases[$i]
        $expectedReport = Join-Path ([string]$Plan.run.outputDir) ([string]$phase.sealedReportLocator)
        $actualReport = Get-FullExistingPath -Path $ReportPaths[$i] -Label "$($phase.phaseId) sealed report" -Kind Leaf
        if (-not $actualReport.Equals([IO.Path]::GetFullPath($expectedReport), [StringComparison]::OrdinalIgnoreCase)) {
            throw "$($phase.phaseId) report path differs from its planned create-new locator."
        }
        $configPath = Join-Path $PlanDirectory ([string]$phase.configLocator)
        $summary = Invoke-JsonCommand -Executable $DepthLab -Label "$($phase.phaseId) raw v3 report validation" -Arguments @(
            'camera-discovery-phase-validate', '--input', $actualReport, '--expected-config', $configPath,
            '--expected-config-sha256', [string]$phase.configSha256
        )
        Assert-ExactProperties $summary @(
            'schema', 'status', 'exactConfigSha256', 'phaseId', 'dsvSelection',
            'selectedDsvDescriptor', 'comparisonCameraReference', 'capabilityCeiling',
            'publicationAllowed', 'recordingReady'
        ) @() "$($phase.phaseId) raw v3 report validation result"
        Assert-ValidatorExactConfigHash $summary.exactConfigSha256 ([string]$phase.configSha256) `
            "$($phase.phaseId) raw v3 report validation result"
        if ([string]$summary.schema -cne $script:PhaseValidationSchema -or
            [string]$summary.status -cne 'pass' -or
            [string]$summary.phaseId -cne [string]$phase.phaseId -or
            [string]$summary.capabilityCeiling -cne 'camera_discovery_only' -or
            $summary.publicationAllowed -ne $false -or $summary.recordingReady -ne $false) {
            throw "$($phase.phaseId) validator overclaimed or returned the wrong identity."
        }
        Assert-JsonEqual $summary.dsvSelection `
            $Configs[[string]$phase.phaseId].camera_discovery.dsv_selection `
            "$($phase.phaseId) report selection"
        Assert-JsonEqual $summary.selectedDsvDescriptor $Plan.selectionReplay.selectedDescriptor "$($phase.phaseId) report descriptor"
        $summaries += $summary
    }
    for ($i = 1; $i -lt 3; $i++) {
        Assert-JsonEqual $summaries[$i].dsvSelection $summaries[0].dsvSelection 'phase report selection continuity'
        Assert-JsonEqual $summaries[$i].selectedDsvDescriptor $summaries[0].selectedDsvDescriptor 'phase report descriptor continuity'
        Assert-JsonEqual $summaries[$i].comparisonCameraReference $summaries[0].comparisonCameraReference 'phase report comparison reference continuity'
    }
    return , $summaries
}

function New-SelfTestFixture {
    $shaA = 'sha256:' + ('a' * 64)
    $shaB = 'sha256:' + ('b' * 64)
    $descriptor = [pscustomobject][ordered]@{
        view_format = 45; view_dimension = 3; view_flags = 0; mip_slice = 0
        first_array_slice = 0; view_array_size = 1; resource_format = 44
        width = 1920; height = 1080; resource_mip_levels = 1; resource_array_size = 1
        sample_count = 1; sample_quality = 0; usage = 0; bind_flags = 64
        cpu_access_flags = 0; misc_flags = 0
    }
    $target = [pscustomobject][ordered]@{
        gameId = 'assassins-creed-iii-remastered-steam'; steamAppId = '911400'; steamBuildId = '11324511'
        executable = [pscustomobject][ordered]@{ path = 'C:\Games\ACIII.exe'; basename = 'ACIII.exe'; sha256 = $shaA }
    }
    $plan = [pscustomobject][ordered]@{
        schema = $script:PlanSchema; status = 'validated_plan_only'; capabilityCeiling = 'camera_discovery_only'
        publicationAllowed = $false; deliverable = $false; recordingReady = $false
        target = $target
        liveInstance = [pscustomobject][ordered]@{ processId = 1234; processStartTimeUtc = '2026-09-03T00:00:00.0000000Z' }
        gates = [pscustomobject][ordered]@{
            runtimeEvidence = [pscustomobject][ordered]@{ path = 'C:\evidence\runtime.json'; sha256 = $shaB }
            representativeScene = [pscustomobject][ordered]@{ path = 'C:\evidence\scene.json'; sha256 = $shaB }
            dsvSelection = [pscustomobject][ordered]@{ evidenceRoot = 'C:\evidence'; attestationLocator = 'selection.json'; attestationSha256 = $shaB }
        }
        run = [pscustomobject][ordered]@{ planLeaf = 'ac3-camera-plan'; discoveryId = 'ac3-camera-001'; outputLeaf = 'ac3-camera-001'; outputDir = 'C:\evidence\ac3-camera-001'; sessionId = 'ac3-camera-001' }
        selectionReplay = [pscustomobject][ordered]@{
            target = [pscustomobject][ordered]@{ gameId = $target.gameId; steamAppId = $target.steamAppId; steamBuildId = $target.steamBuildId; executable = [pscustomobject][ordered]@{ basename = 'ACIII.exe'; sha256 = $shaA } }
            selectedDescriptor = $descriptor
        }
        phaseOrder = @($script:PhaseOrder)
        phases = @()
        planSha256Locator = 'camera-discovery-plan.sha256'
        commands = [pscustomobject][ordered]@{
            validatePlan = [pscustomobject][ordered]@{ executable = 'powershell'; arguments = @('-File', 'Test.ps1') }
        phaseProtocol = @()
            validateSealedReports = [pscustomobject][ordered]@{ executable = 'powershell'; arguments = @('-File', 'Test.ps1') }
            offlineAnalysis = [pscustomobject][ordered]@{
                available = $true; expectedReducerExitCode = 1
                outputSchema = 'camera-reference-free-candidate-review/v1'
                capabilityCeiling = 'human_review_candidates_only'
                prerequisite = 'validate reports first'; executable = 'powershell.exe'
                arguments = @('-File', 'Test.ps1', '-RunOfflineAnalysis')
            }
        }
    }
    $configs = @{}
    $phaseInstructions = @{
        POSE_STATIC = 'Human confirms the representative offline single-player scene, then remains fully static. Do not infer completion from elapsed time.'
        POSE_WALK = 'After the prior create-new sealed report passes validation, human confirms straight walking without turning the view.'
        POSE_TURN = 'After the prior create-new sealed report passes validation, human confirms turning in place without walking.'
    }
    foreach ($phase in $script:PhaseOrder) {
        $locator = "camera-config-$phase.json"
        $plan.phases += [pscustomobject][ordered]@{
            phaseId = $phase; configLocator = $locator; configSha256 = $shaA
            sealedReportLocator = "camera-candidates-$phase.json"; operatorConfirmation = $phaseInstructions[$phase]
        }
        $configs[$phase] = [pscustomobject][ordered]@{
            enabled = $true; output_dir = $plan.run.outputDir; unbind_from_end = 4; max_frames = $null
            present_only = $false; watermark_only = $false; writer_bypass = $false
            target_depth_hz = $null; lane_fps = $null; camera_probe = $true
            camera_discovery = [pscustomobject][ordered]@{
                schema = 'camera-discovery-config/v3'; phase_id = $phase
                executable_sha256 = $shaA
                dsv_selection = [pscustomobject][ordered]@{ evidence_root = 'C:\evidence'; attestation_locator = 'selection.json'; attestation_sha256 = $shaB }
                sample_every_n_presents = 1; max_main_passes_per_present = 4
                draw_window_start = 0; draw_window_count = 128
            }
            dsv_discovery = $null; camera_production = $null; depth_profile_path = $null
            depth_profile_sha256 = $null; session_id = $plan.run.sessionId
        }
        $plan.commands.phaseProtocol += [pscustomobject][ordered]@{
            phaseId = $phase; humanConfirmationRequired = $true
            configLocator = $locator; sealInstruction = 'human-owned exact seal'
            validateSealedReport = [pscustomobject][ordered]@{
                executable = 'depth-lab.exe'
                arguments = @('camera-discovery-phase-validate', '--input', "camera-candidates-$phase.json",
                    '--expected-config', $locator, '--expected-config-sha256', $shaA)
            }
        }
    }
    $plan.commands = (New-ExpectedCommands -Plan $plan -Configs $configs -DepthLab 'C:\tools\depth-lab.exe' |
        ConvertTo-Json -Depth 32 | ConvertFrom-Json)
    return [pscustomobject]@{ plan = $plan; configs = $configs }
}

function New-PassingGateFixtures {
    param([Parameter(Mandatory = $true)]$Plan)
    $selectedTarget = [pscustomobject][ordered]@{
        gameId = $Plan.target.gameId; appId = $Plan.target.steamAppId
        steamBuildId = $Plan.target.steamBuildId; executableBasename = $Plan.target.executable.basename
        executableSha256 = $Plan.target.executable.sha256
    }
    return [pscustomobject]@{
        target = [pscustomobject]@{ status = 'pass'; selected_target = $selectedTarget }
        rights = [pscustomobject]@{ status = 'pass'; publisher_or_counsel_approval = $true; selected_record_count = 1; runtime_anti_cheat_gate_still_required = $true }
        d6 = [pscustomobject]@{ status = 'install_gate_passed'; app_id = $Plan.target.steamAppId; build_id = $Plan.target.steamBuildId; executable_candidates = @([pscustomobject]@{ relative_path = 'ACIII.exe'; sha256 = $Plan.target.executable.sha256 }) }
        scene = [pscustomobject]@{ status = 'pass'; attestation_sha256 = $Plan.gates.representativeScene.sha256; game_id = $Plan.target.gameId; discovery_id = $Plan.run.discoveryId; output_leaf = $Plan.run.outputLeaf; scene_kind = 'representative_offline_single_player' }
        selection = [pscustomobject][ordered]@{ schema = $script:SelectionValidationSchema; status = 'pass'; locator = $Plan.gates.dsvSelection.attestationLocator; exactBytesSha256 = $Plan.gates.dsvSelection.attestationSha256; target = $Plan.selectionReplay.target; selectedDescriptor = $Plan.selectionReplay.selectedDescriptor; capabilityCeiling = 'dsv_selection_bridge_only'; publicationAllowed = $false; recordingReady = $false }
    }
}

function Assert-Throws {
    param([Parameter(Mandatory = $true)][scriptblock]$Action, [Parameter(Mandatory = $true)][string]$Name)
    $blocked = $false
    try { & $Action } catch { $blocked = $true }
    if (-not $blocked) { throw "self-test '$Name' did not fail closed." }
}

function Invoke-SelfTest {
    $tests = 0
    $fixture = New-SelfTestFixture
    Assert-PlanCore -Plan $fixture.plan -Configs $fixture.configs
    Assert-CommandContract -Plan $fixture.plan -Configs $fixture.configs -DepthLab 'C:\tools\depth-lab.exe'
    Assert-PlanDirectoryBinding -Plan $fixture.plan -ActualDirectory 'C:\evidence\ac3-camera-plan'
    $gates = New-PassingGateFixtures -Plan $fixture.plan
    Assert-ReplayedGates -Plan $fixture.plan -TargetResult $gates.target -RightsResult $gates.rights `
        -D6Result $gates.d6 -SceneResult $gates.scene -SelectionResult $gates.selection
    $tests++ # AC3 authorized happy plan

    $unauthorized = New-PassingGateFixtures -Plan $fixture.plan
    $unauthorized.rights.status = 'blocked'
    Assert-Throws { Assert-ReplayedGates -Plan $fixture.plan -TargetResult $unauthorized.target -RightsResult $unauthorized.rights -D6Result $unauthorized.d6 -SceneResult $unauthorized.scene -SelectionResult $unauthorized.selection } 'unauthorized target'
    $tests++

    $protection = New-PassingGateFixtures -Plan $fixture.plan
    $protection.scene.status = 'blocked_runtime_protection'
    Assert-Throws { Assert-ReplayedGates -Plan $fixture.plan -TargetResult $protection.target -RightsResult $protection.rights -D6Result $protection.d6 -SceneResult $protection.scene -SelectionResult $protection.selection } 'runtime protection blocked'
    $tests++

    $selectionDrift = New-PassingGateFixtures -Plan $fixture.plan
    $selectionDrift.selection.exactBytesSha256 = 'sha256:' + ('c' * 64)
    Assert-Throws { Assert-ReplayedGates -Plan $fixture.plan -TargetResult $selectionDrift.target -RightsResult $selectionDrift.rights -D6Result $selectionDrift.d6 -SceneResult $selectionDrift.scene -SelectionResult $selectionDrift.selection } 'selection drift'
    $tests++

    $missingScene = New-SelfTestFixture
    $missingScene.plan.gates.representativeScene = $null
    Assert-Throws { Assert-PlanCore -Plan $missingScene.plan -Configs $missingScene.configs } 'missing representative scene'
    $tests++

    $mixed = New-SelfTestFixture
    $mixed.configs['POSE_WALK'].camera_discovery | Add-Member -NotePropertyName comparison_camera_reference -NotePropertyValue ([pscustomobject]@{ provider = 'unexpected' })
    Assert-Throws { Assert-PlanCore -Plan $mixed.plan -Configs $mixed.configs } 'mixed references'
    $tests++

    $review = [pscustomobject][ordered]@{
        schema = 'camera-reference-free-candidate-review/v1'; status = 'human_review_required'
        phase_ids = @($script:PhaseOrder); executable_sha256 = $fixture.plan.target.executable.sha256
        dsv_selection = $fixture.configs['POSE_STATIC'].camera_discovery.dsv_selection
        selected_dsv_descriptor = $fixture.plan.selectionReplay.selectedDescriptor; candidates = @()
        human_review_required = $true; metric_scale_available = $false
        automatic_selection_authorized = $false; camera_binding_generated = $false
        recording_ready = $false; deliverable = $false; publication_allowed = $false
        requires_metric_scale_validation = $true; requires_e_geo_validation = $true
        production_binding_authorized = $false
    }
    Assert-OfflineReducerOutcome -HasReference $false -ExitCode 1 -Document $review -Plan $fixture.plan
    $tests++
    $review.automatic_selection_authorized = $true
    Assert-Throws { Assert-ReferenceFreeReview -Review $review -Plan $fixture.plan } 'reference-free authority overclaim'
    $tests++

    $boundary = New-SelfTestFixture
    foreach ($phase in $script:PhaseOrder) {
        $boundary.configs[$phase].camera_discovery.draw_window_start = 384
        $boundary.configs[$phase].camera_discovery.draw_window_count = 128
    }
    Assert-PlanCore -Plan $boundary.plan -Configs $boundary.configs
    $tests++
    $boundary.configs['POSE_TURN'].camera_discovery.draw_window_start = 385
    Assert-Throws { Assert-PlanCore -Plan $boundary.plan -Configs $boundary.configs } 'draw window exceeds 512'
    $tests++

    $leafCollision = New-SelfTestFixture
    $leafCollision.plan.run.outputLeaf = 'AC3-CAMERA-PLAN'
    $leafCollision.plan.run.outputDir = 'C:\evidence\AC3-CAMERA-PLAN'
    foreach ($phase in $script:PhaseOrder) { $leafCollision.configs[$phase].output_dir = $leafCollision.plan.run.outputDir }
    Assert-Throws { Assert-PlanCore -Plan $leafCollision.plan -Configs $leafCollision.configs } 'plan/output leaf collision'
    $tests++

    $commandDrift = New-SelfTestFixture
    $commandDrift.plan.commands.validatePlan.arguments[0] = '-NoLogo'
    Assert-Throws { Assert-CommandContract -Plan $commandDrift.plan -Configs $commandDrift.configs -DepthLab 'C:\tools\depth-lab.exe' } 'command argv drift'
    $tests++

    $phaseHashCommandDrift = New-SelfTestFixture
    $phaseHashCommandDrift.plan.commands.phaseProtocol[0].validateSealedReport.arguments[-1] = `
        'sha256:' + ('f' * 64)
    Assert-Throws {
        Assert-CommandContract -Plan $phaseHashCommandDrift.plan -Configs $phaseHashCommandDrift.configs `
            -DepthLab 'C:\tools\depth-lab.exe'
    } 'phase command expected config hash drift'
    $tests++

    Assert-ValidatorExactConfigHash ('sha256:' + ('a' * 64)) ('sha256:' + ('a' * 64)) `
        'validator config summary'
    Assert-Throws {
        Assert-ValidatorExactConfigHash ('sha256:' + ('b' * 64)) ('sha256:' + ('a' * 64)) `
            'validator config summary'
    } 'validator exact config hash drift'
    $tests++

    Assert-Throws { Assert-PlanDirectoryBinding -Plan $fixture.plan -ActualDirectory 'C:\evidence\wrong-plan' } 'plan parent drift'
    $tests++

    $motion = [pscustomobject][ordered]@{
        samples = 8; steps = 7; translation_median = 0.0; translation_p95 = 0.0
        rotation_median_rad = 0.0; rotation_p95_rad = 0.0
    }
    $candidate = [pscustomobject][ordered]@{
        source = [pscustomobject][ordered]@{
            shader_stage = 'vertex'; bind_slot = 0; pass_ordinal = 0; draw_ordinal = 1
            buffer_bytes = 256; offset = 0; encoding = 'f32_little_endian'
            layout = 'row_major'; transpose = $false; role = 'view'; translation = 'row3'
        }
        static_motion = $motion; walk_motion = $motion; turn_motion = $motion
    }
    $nested = [pscustomobject][ordered]@{
        schema = 'camera-reference-free-candidate-review/v1'; status = 'human_review_required'
        phase_ids = @($script:PhaseOrder); executable_sha256 = $fixture.plan.target.executable.sha256
        dsv_selection = $fixture.configs['POSE_STATIC'].camera_discovery.dsv_selection
        selected_dsv_descriptor = $fixture.plan.selectionReplay.selectedDescriptor
        candidates = @($candidate); human_review_required = $true; metric_scale_available = $false
        automatic_selection_authorized = $false; camera_binding_generated = $false
        recording_ready = $false; deliverable = $false; publication_allowed = $false
        requires_metric_scale_validation = $true; requires_e_geo_validation = $true
        production_binding_authorized = $false
    }
    Assert-ReferenceFreeReview -Review $nested -Plan $fixture.plan
    $candidate | Add-Member -NotePropertyName selectedMetric -NotePropertyValue 1.0
    Assert-Throws { Assert-ReferenceFreeReview -Review $nested -Plan $fixture.plan } 'nested candidate authority field'
    $tests++

    $referenced = New-SelfTestFixture
    $reference = [pscustomobject][ordered]@{
        provider = 'test-reference'; camera_singleton_relocation = 1; camera_root_offset = 2
        world_transform_offset = 3; metres_per_game_unit = '1.0'; axis_mapping = 'x,y,z'
    }
    foreach ($phase in $script:PhaseOrder) {
        $referenced.configs[$phase].camera_discovery | Add-Member `
            -NotePropertyName comparison_camera_reference -NotePropertyValue $reference
    }
    $referenced.plan.commands = (New-ExpectedCommands -Plan $referenced.plan `
        -Configs $referenced.configs -DepthLab 'C:\tools\depth-lab.exe' |
        ConvertTo-Json -Depth 32 | ConvertFrom-Json)
    Assert-PlanCore -Plan $referenced.plan -Configs $referenced.configs
    Assert-CommandContract -Plan $referenced.plan -Configs $referenced.configs -DepthLab 'C:\tools\depth-lab.exe'
    $referencedReport = [pscustomobject][ordered]@{
        schema = 'camera-evidence-report/v2'; phase_ids = @($script:PhaseOrder)
        executable_sha256 = $referenced.plan.target.executable.sha256; candidates = @()
        legacy_reference_static_motion = $motion; legacy_reference_walk_motion = $motion
        legacy_reference_turn_motion = $motion
        selection = [pscustomobject]@{ status = 'selected'; source = $candidate.source; equivalent_draw_ordinals = @() }
        requires_e_geo_validation = $true; production_binding_authorized = $false
    }
    Assert-OfflineReducerOutcome -HasReference $true -ExitCode 0 -Document $referencedReport -Plan $referenced.plan
    $tests++
    Assert-Throws { Assert-OfflineReducerOutcome -HasReference $true -ExitCode 1 -Document $referencedReport -Plan $referenced.plan } 'referenced reducer exit one'
    $tests++

    $tempRoot = Join-Path ([IO.Path]::GetTempPath()) ('dm-camera-plan-selftest-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($tempRoot) | Out-Null
    try {
        $planFile = Join-Path $tempRoot 'camera-discovery-plan.json'
        $planBytes = [Text.UTF8Encoding]::new($false).GetBytes("{}`n")
        [IO.File]::WriteAllBytes($planFile, $planBytes)
        $planHash = 'sha256:' + (Get-FileHash -LiteralPath $planFile -Algorithm SHA256).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText((Join-Path $tempRoot 'camera-discovery-plan.sha256'), $planHash + "`n", [Text.UTF8Encoding]::new($false))
        Read-PlanWithSidecar -Path $planFile | Out-Null
        [IO.File]::WriteAllBytes($planFile, [Text.UTF8Encoding]::new($false).GetBytes("{ }`n"))
        Assert-Throws { Read-PlanWithSidecar -Path $planFile | Out-Null } 'plan exact-byte drift'
        $tests++

        [IO.File]::WriteAllBytes((Join-Path $tempRoot 'camera-discovery-plan.sha256'), [byte[]]::new(73))
        Assert-Throws { Read-PlanWithSidecar -Path $planFile | Out-Null } 'oversized plan hash sidecar'
        $tests++

        $tamperedPlanBytes = [Text.UTF8Encoding]::new($false).GetBytes(
            ($commandDrift.plan | ConvertTo-Json -Depth 32) + "`n")
        [IO.File]::WriteAllBytes($planFile, $tamperedPlanBytes)
        $tamperedPlanHash = Get-BytesSha256 -Bytes $tamperedPlanBytes
        [IO.File]::WriteAllText((Join-Path $tempRoot 'camera-discovery-plan.sha256'),
            $tamperedPlanHash + "`n", [Text.UTF8Encoding]::new($false))
        $tamperedDocument = Read-PlanWithSidecar -Path $planFile
        Assert-Throws {
            Assert-CommandContract -Plan $tamperedDocument.value -Configs $commandDrift.configs `
                -DepthLab 'C:\tools\depth-lab.exe'
        } 'command drift with matching changed sidecar'
        $tests++

        $configFile = Join-Path $tempRoot 'config.json'
        [IO.File]::WriteAllBytes($configFile, [Text.UTF8Encoding]::new($false).GetBytes("{}`n"))
        Assert-Throws { Read-VerifiedJson -Path $configFile -ExpectedSha256 ('sha256:' + ('f' * 64)) -Label 'config' | Out-Null } 'config hash drift'
        $tests++
    } finally {
        if (Test-Path -LiteralPath $tempRoot -PathType Container) {
            $resolvedTemp = [IO.Path]::GetFullPath($tempRoot)
            $expectedParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar)
            if ([IO.Path]::GetDirectoryName($resolvedTemp).Equals($expectedParent, [StringComparison]::OrdinalIgnoreCase) -and
                [IO.Path]::GetFileName($resolvedTemp).StartsWith('dm-camera-plan-selftest-', [StringComparison]::Ordinal)) {
                [IO.Directory]::Delete($resolvedTemp, $true)
            }
        }
    }

    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

try {
    if ($SelfTest) { Invoke-SelfTest; exit 0 }

    $depthLab = Get-FullExistingPath -Path $DepthLabPath -Label 'depth-lab executable' -Kind Leaf
    $planDocument = Read-PlanWithSidecar -Path $PlanPath
    $plan = $planDocument.value
    $planDirectory = [IO.Path]::GetDirectoryName($planDocument.path)
    $configs = @{}
    foreach ($phase in @($plan.phases)) {
        Assert-SafeLeaf $phase.configLocator 'phase config locator' '.json'
        $configDocument = Read-VerifiedJson `
            -Path (Join-Path $planDirectory ([string]$phase.configLocator)) `
            -ExpectedSha256 ([string]$phase.configSha256) `
            -Label "$($phase.phaseId) phase config"
        $configs[[string]$phase.phaseId] = $configDocument.value
    }
    Assert-PlanCore -Plan $plan -Configs $configs
    Assert-PlanDirectoryBinding -Plan $plan -ActualDirectory $planDirectory -AllowStaged:$StagedPlanValidation
    Assert-CommandContract -Plan $plan -Configs $configs -DepthLab $depthLab
    Invoke-ConfigValidation -Plan $plan -Configs $configs -PlanDirectory $planDirectory -DepthLab $depthLab

    $evidenceRoot = Get-FullExistingPath -Path $plan.gates.dsvSelection.evidenceRoot -Label 'evidence root' -Kind Container
    Assert-DirectChildPath -Root $evidenceRoot -Path $plan.gates.runtimeEvidence.path -Label 'runtime evidence'
    Assert-DirectChildPath -Root $evidenceRoot -Path $plan.gates.representativeScene.path -Label 'representative-scene attestation'
    Read-VerifiedJson -Path $plan.gates.runtimeEvidence.path -ExpectedSha256 $plan.gates.runtimeEvidence.sha256 -Label 'runtime evidence' | Out-Null
    Read-VerifiedJson -Path $plan.gates.representativeScene.path -ExpectedSha256 $plan.gates.representativeScene.sha256 -Label 'representative-scene attestation' | Out-Null
    $selectionPath = Join-Path $evidenceRoot ([string]$plan.gates.dsvSelection.attestationLocator)
    Assert-DirectChildPath -Root $evidenceRoot -Path $selectionPath -Label 'selection attestation'
    Read-VerifiedJson -Path $selectionPath -ExpectedSha256 $plan.gates.dsvSelection.attestationSha256 -Label 'selection attestation' | Out-Null
    $exe = Get-FullExistingPath -Path $plan.target.executable.path -Label 'target executable' -Kind Leaf
    if (-not ([IO.Path]::GetFileName($exe)).Equals([string]$plan.target.executable.basename, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'target executable basename differs from the plan.'
    }
    $exeHash = 'sha256:' + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($exeHash -cne [string]$plan.target.executable.sha256) { throw 'target executable bytes changed.' }

    $fixedConfig = Get-FixedConfigPath
    if (Test-Path -LiteralPath $fixedConfig) {
        Assert-NoReparseAncestor -Path $fixedConfig -Label 'existing fixed probe config'
        throw 'fixed probe config still exists; the exact owner must close it before validation or offline reduction.'
    }

    Invoke-LiveGateReplay -Plan $plan -DepthLab $depthLab

    $reports = @($StaticReportPath, $WalkReportPath, $TurnReportPath)
    $reportCount = @($reports | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }).Count
    $summaries = @()
    if ($reportCount -ne 0 -and $reportCount -ne 3) {
        throw 'StaticReportPath, WalkReportPath, and TurnReportPath must be supplied together.'
    }
    if ($reportCount -eq 3) {
        $summaries = Invoke-PhaseReportValidation -Plan $plan -Configs $configs `
            -PlanDirectory $planDirectory -DepthLab $depthLab -ReportPaths $reports
    } else {
        if (Test-Path -LiteralPath ([string]$plan.run.outputDir)) {
            throw 'camera output already exists; supply all three create-new sealed reports for validation.'
        }
    }

    $analysisPath = $null
    if ($RunOfflineAnalysis) {
        if ($reportCount -ne 3) { throw 'offline analysis requires all three validated phase reports.' }
        if ([string]::IsNullOrWhiteSpace($AnalysisOutputPath) -or -not [IO.Path]::IsPathRooted($AnalysisOutputPath)) {
            throw 'AnalysisOutputPath must be a new absolute JSON path.'
        }
        $analysisPath = [IO.Path]::GetFullPath($AnalysisOutputPath)
        if (Test-Path -LiteralPath $analysisPath) { throw "analysis output must be new: $analysisPath" }
        Assert-SafeLeaf -Value ([IO.Path]::GetFileName($analysisPath)) -Label 'analysis output leaf' -Extension '.json'
        Assert-DirectChildPath -Root $plan.run.outputDir -Path $analysisPath -Label 'analysis output'
        Get-FullExistingPath -Path ([IO.Path]::GetDirectoryName($analysisPath)) -Label 'analysis output directory' -Kind Container | Out-Null
        & $depthLab camera-evidence --static $reports[0] --walk $reports[1] --turn $reports[2] --out $analysisPath
        $analysisExit = $LASTEXITCODE
        $analysisDocument = (Read-VerifiedJson -Path $analysisPath -Label 'offline camera analysis').value
        if ($null -eq $summaries[0].comparisonCameraReference) {
            Assert-OfflineReducerOutcome -HasReference $false -ExitCode $analysisExit `
                -Document $analysisDocument -Plan $plan
        } else {
            Assert-OfflineReducerOutcome -HasReference $true -ExitCode $analysisExit `
                -Document $analysisDocument -Plan $plan
        }
    }

    $phaseConfigSha256 = [ordered]@{}
    foreach ($phase in @($plan.phases)) {
        $phaseConfigSha256[[string]$phase.phaseId] = [string]$phase.configSha256
    }
    [pscustomobject][ordered]@{
        status = 'pass'
        schema = $script:PlanSchema
        planSha256 = $planDocument.sha256
        phaseConfigSha256 = $phaseConfigSha256
        gatesReplayed = @('target', 'rights', 'steam_d6', 'runtime_protection_scene', 'dsv_selection_sources')
        phaseReportsValidated = ($reportCount -eq 3)
        analysisOutput = $analysisPath
        capabilityCeiling = 'camera_discovery_only'
        publicationAllowed = $false
        recordingReady = $false
    } | ConvertTo-Json -Depth 5
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}

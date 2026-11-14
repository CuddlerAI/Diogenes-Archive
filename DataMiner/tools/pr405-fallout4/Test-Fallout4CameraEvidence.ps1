<# Pure regression tests: parse/import only helper functions; never touch the game or ProgramData. #>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Rejected {
    param([scriptblock]$Operation, [string]$Pattern)
    $caught = $false
    try { & $Operation | Out-Null } catch {
        $caught = $true
        if ($_.Exception.Message -notmatch $Pattern) { throw }
    }
    if (-not $caught) { throw "Expected rejection matching: $Pattern" }
}

foreach ($name in @('Invoke-Fallout4CameraEvidence.ps1', 'Invoke-Fallout4CameraEvidenceSweep.ps1')) {
    $tokens = $null
    $parseErrors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PSScriptRoot $name), [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count -ne 0) { throw ($parseErrors | Out-String) }
    $helpers = $ast.FindAll({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -in @('Assert-CameraReference', 'Assert-PhaseReady', 'Wait-Progress', 'Wait-SealedProgress', 'Get-NextEvidencePhase', 'Test-FalloutProbeModuleName')
    }, $true)
    foreach ($helper in $helpers) { . ([scriptblock]::Create($helper.Extent.Text)) }
}

foreach ($name in @('dm_depth_probe.dll', ('DM_DEPTH_PROBE-' + ('A' * 64) + '.DLL'))) {
    if (-not (Test-FalloutProbeModuleName $name)) { throw "Resident probe missed: $name" }
}
foreach ($name in @('', 'other.dll', ('dm_depth_probe-' + ('a' * 63) + '.dll'), 'dm_depth_probe.dll.backup')) {
    if (Test-FalloutProbeModuleName $name) { throw "Unexpected resident probe match: $name" }
}

$candidatePath = Join-Path $PSScriptRoot 'authoring\1.11.240.0\camera-reference.json'
$reference = Get-Content -LiteralPath $candidatePath -Raw | ConvertFrom-Json
$candidateHash = 'fdcef37ac1230af6d0b0050eb2142b139ef3a867b37b9211fb6edfcc646072f8'
Assert-CameraReference -Binding $reference -ActualHash $candidateHash
Assert-Rejected { Assert-CameraReference $reference ('f' * 64) } 'EXE hash'
$reference.schema = 'camera-binding/v2'
Assert-Rejected { Assert-CameraReference $reference $candidateHash } '格式'
$reference.schema = 'fallout4-camera-reference/v1'
$reference.game_camera.provider = 'unknown'
Assert-Rejected { Assert-CameraReference $reference $candidateHash } 'singleton'

# Reviewed authoring files retain distinct identities from the immutable measured inputs.
$reviewRoot = Join-Path $PSScriptRoot 'authoring\1.11.240.0'
$review = Get-Content -LiteralPath (Join-Path $reviewRoot 'geometry-evidence.json') -Raw | ConvertFrom-Json
if ($review.production_binding_authorized -or $review.performance_certified -or $review.deliverable) {
    throw 'Trial summary must not grant production or performance approval.'
}
foreach ($entry in @($review.review_files.camera_binding, $review.review_files.depth_profile)) {
    if ($entry.path -notin @('camera-binding.json', 'depth-profile.json')) { throw 'Unexpected review file path.' }
    $actual = 'sha256:' + (Get-FileHash -LiteralPath (Join-Path $reviewRoot $entry.path) -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $entry.sha256) { throw "Reviewed candidate bytes changed: $($entry.path)" }
}

$MinimumPhaseSamples = 24
if ((Get-NextEvidencePhase 'PROJECTION_FOV75_A') -ne 'PROJECTION_HOLD90' -or
    (Get-NextEvidencePhase 'PROJECTION_HOLD90') -ne 'PROJECTION_FOV90' -or
    (Get-NextEvidencePhase 'PROJECTION_FOV90') -ne 'PROJECTION_HOLD75' -or
    (Get-NextEvidencePhase 'PROJECTION_HOLD75') -ne 'PROJECTION_FOV75_B') {
    throw 'Projection phases must seal before FOV changes in HOLD.'
}
Assert-Rejected { Get-NextEvidencePhase 'PROJECTION_FOV75_B' } 'no next phase'
Assert-Rejected { Get-NextEvidencePhase 'POSE_TURN' } 'no next phase'
$report = [pscustomobject]@{
    schema = 'camera-discovery-report/v4'
    phase_id = 'POSE_STATIC'
    comparison_camera_samples = @(1..24)
    reliable = $true
    blockers = @()
}
Assert-PhaseReady $report 'POSE_STATIC' $true
$report.schema = 'camera-discovery-report/v2'
Assert-Rejected { Assert-PhaseReady $report 'POSE_STATIC' $true } '身份不匹配'
$report.schema = 'camera-discovery-report/v4'
Assert-Rejected { Assert-PhaseReady $report 'POSE_WALK' $true } '身份不匹配'
$report.reliable = $false
Assert-Rejected { Assert-PhaseReady $report 'POSE_STATIC' $true } '不可靠'
$report.reliable = $true
$report.comparison_camera_samples = @(1..23)
Assert-Rejected { Assert-PhaseReady $report 'POSE_STATIC' $true } '同帧样本'

# Progress waiters run against synthetic in-memory JSON, not a live report or filesystem.
$script:progressFixture = [ordered]@{
    schema = 'camera-discovery-progress/v1'
    report_schema = 'camera-discovery-report/v4'
    phase_id = 'POSE_STATIC_W000'
    draw_window_start = 0
    draw_window_count = 8
    comparison_camera_samples = 24
    view_candidates = 2
    processed_draws = 192
    candidate_overflows = 0
    view_observation_overflows = 0
    phase_boundary_discarded_presents = 0
    phase_boundary_discarded_draws = 0
    reliable = $true
    blockers = @()
}
function Get-Content {
    param($LiteralPath, [switch]$Raw, $Encoding, $ErrorAction)
    $script:progressFixture | ConvertTo-Json -Depth 6
}
$WindowTimeoutSeconds = 1
$MaxMainPassesPerPresent = 1
$syntheticRoot = $PSScriptRoot
try {
    [void](Wait-Progress $syntheticRoot 'POSE_STATIC_W000' 0 8 24 $true)
    [void](Wait-SealedProgress $syntheticRoot 'POSE_STATIC_W000' 0 8 24)
    $script:progressFixture.report_schema = 'camera-discovery-report/v2'
    Assert-Rejected { Wait-Progress $syntheticRoot 'POSE_STATIC_W000' 0 8 24 $true } 'timed out'
    Assert-Rejected { Wait-SealedProgress $syntheticRoot 'POSE_STATIC_W000' 0 8 24 } 'timed out'
    $script:progressFixture.report_schema = 'camera-discovery-report/v4'
    $script:progressFixture.candidate_overflows = 1
    Assert-Rejected { Wait-Progress $syntheticRoot 'POSE_STATIC_W000' 0 8 24 $true } 'overflow'
    Assert-Rejected { Wait-SealedProgress $syntheticRoot 'POSE_STATIC_W000' 0 8 24 } 'overflow'
} finally {
    Remove-Item Function:\Get-Content
}
Write-Output 'PASS: exact reference and candidate identities, trial isolation, v4 report/progress, legacy rejection, phase/sample/reliability/overflow gates.'

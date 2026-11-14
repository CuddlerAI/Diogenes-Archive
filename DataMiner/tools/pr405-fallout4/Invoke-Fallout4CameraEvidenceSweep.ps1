<#
.SYNOPSIS
  #636 automatic multi-window render-camera discovery.

.DESCRIPTION
  The operator performs only three macro actions: remain static, translate without rotating, and
  rotate in place. Within each macro action this script automatically seals and advances bounded
  draw windows. It never focuses Fallout, sends input, or changes game settings.

  This is authoring evidence only. Even a unique sweep result still requires RGB/depth E-geo and
  never authorizes a production CameraBinding by itself.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('StartStatic', 'RunWalk', 'RunTurnAndAnalyze', 'CaptureOne', 'Abort')]
    [string]$Action,

    [string]$OutputDir = '',
    [ValidateSet('STATIC', 'WALK', 'TURN')]
    [string]$SinglePhase = 'STATIC',
    [ValidateRange(1, 16384)]
    [uint32]$RenderWidth = 1920,
    [ValidateRange(1, 16384)]
    [uint32]$RenderHeight = 1080,
    [ValidateRange(0, 511)]
    [uint32]$DrawWindowStart = 0,
    [ValidateRange(1, 64)]
    [uint32]$WindowCount = 16,
    [ValidateRange(1, 256)]
    [uint32]$DrawWindowCount = 8,
    [ValidateRange(1, 16)]
    [uint32]$MaxMainPassesPerPresent = 1,
    [ValidateRange(1, 600)]
    [uint32]$SampleEveryNPresents = 30,
    [ValidateRange(8, 4096)]
    [uint32]$MinimumWindowSamples = 24,
    [ValidateRange(1, 120)]
    [uint32]$WindowTimeoutSeconds = 30,
    [string]$Fallout4Path = '',
    [string]$ArtifactRoot = '',
    [string]$CameraBindingPath = '',
    [string]$ExpectedFallout4Hash = '428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$baseScript = Join-Path $PSScriptRoot 'Invoke-Fallout4CameraEvidence.ps1'
$configPath = Join-Path $env:ProgramData 'DataMiner\depth-lab\probe-config.json'
if (-not $ArtifactRoot) { $ArtifactRoot = Join-Path $dataMinerRoot 'target\release' }
if (-not $CameraBindingPath) {
    $CameraBindingPath = Join-Path $dataMinerRoot 'config\catalogs\dev\fallout4-steam-release-test\camera-binding.json'
}
$script:ownedOutputRoot = $null

$effectiveWindowCount = if ($Action -eq 'CaptureOne') { 1 } else { $WindowCount }
$lastDrawExclusive = [uint64]$DrawWindowStart +
    ([uint64]$effectiveWindowCount * [uint64]$DrawWindowCount)
if ($lastDrawExclusive -gt 512) {
    throw "automatic sweep exceeds the 512-draw discovery limit: end=$lastDrawExclusive"
}
if ([uint64]$MaxMainPassesPerPresent * [uint64]$DrawWindowCount -gt 512) {
    throw 'main-pass x draw-window work exceeds the 512-draw per-sampled-Present limit.'
}

function Write-AtomicUtf8Json {
    param([string]$Path, [object]$Value)
    $dir = Split-Path -Parent $Path
    [IO.Directory]::CreateDirectory($dir) | Out-Null
    $tmp = "$Path.tmp"
    [IO.File]::WriteAllText(
        $tmp,
        ($Value | ConvertTo-Json -Depth 16),
        [Text.UTF8Encoding]::new($false)
    )
    if (Test-Path -LiteralPath $Path) {
        $backup = Join-Path $dir ([IO.Path]::GetRandomFileName())
        try { [IO.File]::Replace($tmp, $Path, $backup) }
        finally { Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue }
    } else {
        Move-Item -LiteralPath $tmp -Destination $Path
    }
}

function Read-ActiveConfig {
    if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
        throw "probe config does not exist: $configPath"
    }
    $config = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not [bool]$config.enabled -or -not [bool]$config.camera_probe -or
        [string]$config.camera_discovery.schema -ne 'camera-discovery-config/v2') {
        throw 'active config is not a #636 camera discovery; refusing to touch another run.'
    }
    return $config
}

function Assert-OutputRoot {
    param([object]$Config)
    $activeRoot = [IO.Path]::GetFullPath([string]$Config.output_dir)
    if ($OutputDir -and -not [IO.Path]::GetFullPath($OutputDir).Equals(
        $activeRoot,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "requested output is not the active evidence root: requested=$OutputDir active=$activeRoot"
    }
    return $activeRoot
}

function Wait-SealedReport {
    param([string]$Root, [string]$PhaseId)
    $path = Join-Path $Root "camera-candidates-$PhaseId.json"
    $deadline = [DateTime]::UtcNow.AddSeconds($WindowTimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $path -PathType Leaf) { return $path }
        Start-Sleep -Milliseconds 200
    }
    throw "timed out waiting for sealed phase: $path"
}

function Wait-SealedProgress {
    param(
        [string]$Root,
        [string]$PhaseId,
        [uint32]$WindowStart,
        [uint32]$WindowSize,
        [uint32]$MinimumSamples
    )
    $path = Join-Path $Root "camera-discovery-progress-$PhaseId.json"
    $deadline = [DateTime]::UtcNow.AddSeconds($WindowTimeoutSeconds)
    $last = 'sealed progress has not appeared'
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $progress = Get-Content -LiteralPath $path -Raw -Encoding UTF8 -ErrorAction Stop |
                ConvertFrom-Json
            $maxBoundaryDraws = [uint64]$MaxMainPassesPerPresent * [uint64]$WindowSize
            if ([string]$progress.schema -ne 'camera-discovery-progress/v1' -or
                [string]$progress.report_schema -ne 'camera-discovery-report/v4') {
                $last = "schema=$($progress.schema) report_schema=$($progress.report_schema)"
            } elseif ([string]$progress.phase_id -ne $PhaseId -or
                [uint32]$progress.draw_window_start -ne $WindowStart -or
                [uint32]$progress.draw_window_count -ne $WindowSize) {
                $last = "identity phase=$($progress.phase_id) window=$($progress.draw_window_start)+$($progress.draw_window_count)"
            } elseif ([uint64]$progress.comparison_camera_samples -lt $MinimumSamples) {
                $last = "samples=$($progress.comparison_camera_samples)/$MinimumSamples"
            } elseif ([uint64]$progress.view_candidates -eq 0 -or
                [uint64]$progress.processed_draws -eq 0) {
                $last = "view_candidates=$($progress.view_candidates) processed_draws=$($progress.processed_draws)"
            } elseif ([uint64]$progress.candidate_overflows -ne 0 -or
                [uint64]$progress.view_observation_overflows -ne 0) {
                throw "sealed phase=$PhaseId contains discovery overflow"
            } elseif ([uint64]$progress.phase_boundary_discarded_presents -gt 1 -or
                [uint64]$progress.phase_boundary_discarded_draws -gt $maxBoundaryDraws) {
                throw "sealed phase=$PhaseId has an invalid boundary discard budget"
            } elseif ((-not [bool]$progress.reliable) -or @($progress.blockers).Count -ne 0) {
                throw "sealed phase=$PhaseId is not reliable: $(@($progress.blockers) -join ',')"
            } else {
                return $progress
            }
        } catch {
            if ($_.Exception.Message -like 'sealed phase=*') { throw }
            $last = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 200
    }
    throw "timed out waiting for sealed phase=$PhaseId acceptance: $last"
}

function Wait-Progress {
    param(
        [string]$Root,
        [string]$PhaseId,
        [uint32]$WindowStart,
        [uint32]$WindowSize,
        [uint32]$MinimumSamples,
        [bool]$RequireReliable
    )
    $path = Join-Path $Root 'camera-discovery-progress.json'
    $deadline = [DateTime]::UtcNow.AddSeconds($WindowTimeoutSeconds)
    $last = 'progress sidecar has not appeared'
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $progress = Get-Content -LiteralPath $path -Raw -Encoding UTF8 -ErrorAction Stop |
                ConvertFrom-Json
            if ([string]$progress.schema -ne 'camera-discovery-progress/v1') {
                $last = "schema=$($progress.schema)"
            } elseif ([string]$progress.report_schema -ne 'camera-discovery-report/v4') {
                $last = "report_schema=$($progress.report_schema)"
            } elseif ([string]$progress.phase_id -ne $PhaseId) {
                $last = "phase=$($progress.phase_id)"
            } elseif ([uint32]$progress.draw_window_start -ne $WindowStart -or
                [uint32]$progress.draw_window_count -ne $WindowSize) {
                $last = "window=$($progress.draw_window_start)+$($progress.draw_window_count)"
            } elseif ([uint64]$progress.comparison_camera_samples -lt $MinimumSamples) {
                $last = "samples=$($progress.comparison_camera_samples)/$MinimumSamples"
            } elseif ([uint64]$progress.processed_draws -eq 0) {
                $last = 'processed_draws=0'
            } elseif ($MinimumSamples -gt 0 -and [uint64]$progress.view_candidates -eq 0) {
                $last = 'view_candidates=0'
            } elseif ([uint64]$progress.candidate_overflows -ne 0 -or
                [uint64]$progress.view_observation_overflows -ne 0) {
                throw "phase=$PhaseId discovery overflow; refusing to continue the sweep."
            } elseif ($RequireReliable -and
                ((-not [bool]$progress.reliable) -or @($progress.blockers).Count -ne 0)) {
                $last = "reliable=$($progress.reliable) blockers=$(@($progress.blockers) -join ',')"
            } else {
                return $progress
            }
        } catch {
            if ($_.Exception.Message -like '*discovery overflow*') { throw }
            $last = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 200
    }
    throw "timed out waiting for phase=$PhaseId acceptance: $last"
}

function Set-DiscoveryPhase {
    param(
        [object]$Config,
        [string]$Root,
        [string]$PhaseId,
        [uint32]$WindowStart,
        [uint32]$WindowSize,
        [uint32]$SampleInterval,
        [uint32]$MinimumSamples,
        [bool]$RequireReliable
    )
    $previous = [string]$Config.camera_discovery.phase_id
    if ($previous -eq $PhaseId) {
        return Wait-Progress -Root $Root -PhaseId $PhaseId -WindowStart $WindowStart `
            -WindowSize $WindowSize -MinimumSamples $MinimumSamples `
            -RequireReliable $RequireReliable
    }
    $previousStart = [uint32]$Config.camera_discovery.draw_window_start
    $previousSize = [uint32]$Config.camera_discovery.draw_window_count
    $previousIsEvidence = $previous -match '^POSE_(STATIC|WALK|TURN)_W\d{3}$'
    $Config.camera_discovery.phase_id = $PhaseId
    $Config.camera_discovery.draw_window_start = $WindowStart
    $Config.camera_discovery.draw_window_count = $WindowSize
    $Config.camera_discovery.sample_every_n_presents = $SampleInterval
    Write-AtomicUtf8Json -Path $configPath -Value $Config
    [void](Wait-SealedReport -Root $Root -PhaseId $previous)
    if ($previousIsEvidence) {
        [void](Wait-SealedProgress -Root $Root -PhaseId $previous `
            -WindowStart $previousStart -WindowSize $previousSize `
            -MinimumSamples $MinimumWindowSamples)
    }
    return Wait-Progress -Root $Root -PhaseId $PhaseId -WindowStart $WindowStart `
        -WindowSize $WindowSize -MinimumSamples $MinimumSamples `
        -RequireReliable $RequireReliable
}

function Invoke-MacroSweep {
    param([object]$Config, [string]$Root, [string]$Macro)
    for ($index = 0; $index -lt $WindowCount; $index++) {
        $start = [uint32]($DrawWindowStart + ($index * $DrawWindowCount))
        $phase = "POSE_$($Macro)_W$($start.ToString('000'))"
        $progress = Set-DiscoveryPhase -Config $Config -Root $Root -PhaseId $phase `
            -WindowStart $start -WindowSize $DrawWindowCount `
            -SampleInterval $SampleEveryNPresents -MinimumSamples $MinimumWindowSamples `
            -RequireReliable $true
        Write-Progress -Activity "#636 $Macro automatic sweep" `
            -Status "$($index + 1)/$WindowCount  draw=$start..$($start + $DrawWindowCount - 1)  samples=$($progress.comparison_camera_samples)" `
            -PercentComplete ((($index + 1) * 100) / $WindowCount)
    }
    Write-Progress -Activity "#636 $Macro automatic sweep" -Completed
}

function Enter-Hold {
    param([object]$Config, [string]$Root, [string]$Macro)
    $hold = "HOLD_AFTER_$Macro"
    [void](Set-DiscoveryPhase -Config $Config -Root $Root -PhaseId $hold `
        -WindowStart 0 -WindowSize 1 -SampleInterval 600 -MinimumSamples 0 `
        -RequireReliable $false)
    return $hold
}

function Restore-Config {
    param([string]$Root)
    $backup = Join-Path $Root 'probe-config.before.json'
    if (Test-Path -LiteralPath $backup -PathType Leaf) {
        Copy-Item -LiteralPath $backup -Destination $configPath -Force
    }
}

function Stop-FailClosed {
    try {
        $config = Read-ActiveConfig
        $root = [string]$config.output_dir
        $phase = [string]$config.camera_discovery.phase_id
        $hasPlan = (Test-Path -LiteralPath (Join-Path $root 'camera-evidence-sweep-plan.json') -PathType Leaf) -or
            (Test-Path -LiteralPath (Join-Path $root 'camera-evidence-single-plan.json') -PathType Leaf)
        $ownedStartup = $script:ownedOutputRoot -and [IO.Path]::GetFullPath($script:ownedOutputRoot).Equals(
            [IO.Path]::GetFullPath($root),
            [StringComparison]::OrdinalIgnoreCase
        )
        if ($phase -notmatch '^(POSE_(STATIC|WALK|TURN)_W\d{3}|HOLD_AFTER_(STATIC|WALK))$' -or
            (-not $hasPlan -and -not $ownedStartup)) {
            return $false
        }
        $config.enabled = $false
        Write-AtomicUtf8Json -Path $configPath -Value $config
        try { [void](Wait-SealedReport -Root $root -PhaseId $phase) } catch { }
        Restore-Config -Root $root
        return $true
    } catch { return $false }
}

function Assert-FreshCaptureOneProcess {
    $processes = @(Get-Process -Name 'Fallout4' -ErrorAction SilentlyContinue)
    foreach ($process in $processes) {
        try {
            $resident = @($process.Modules | Where-Object {
                $_.ModuleName -ieq 'dm_depth_probe.dll'
            })
        } catch {
            throw "cannot verify whether PID $($process.Id) already hosts dm_depth_probe.dll: $($_.Exception.Message)"
        }
        if ($resident.Count -ne 0) {
            throw "CaptureOne requires a fresh Fallout4 process because the legacy config watcher owns one session per process. Fully restart Fallout4 before this repair capture (PID $($process.Id) already hosts dm_depth_probe.dll)."
        }
    }
}

if ($Action -eq 'Abort') {
    if (-not (Stop-FailClosed)) {
        throw 'no owned automatic sweep was active; nothing was stopped.'
    }
    [ordered]@{ action = 'Abort'; result = 'probe_disabled_fail_closed' } |
        ConvertTo-Json -Depth 4
    exit 0
}

try {
    if ($Action -eq 'CaptureOne') {
        if (-not $OutputDir) { throw 'CaptureOne requires -OutputDir.' }
        Assert-FreshCaptureOneProcess
        $phase = "POSE_$($SinglePhase)_W$($DrawWindowStart.ToString('000'))"
        $shell = (Get-Process -Id $PID).Path
        $arguments = @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $baseScript,
            '-Action', 'Start', '-OutputDir', $OutputDir,
            '-RenderWidth', $RenderWidth, '-RenderHeight', $RenderHeight,
            '-InitialPhase', $phase,
            '-DrawWindowStart', $DrawWindowStart,
            '-DrawWindowCount', $DrawWindowCount,
            '-MaxMainPassesPerPresent', $MaxMainPassesPerPresent,
            '-SampleEveryNPresents', $SampleEveryNPresents,
            '-MinimumPhaseSamples', $MinimumWindowSamples,
            '-ArtifactRoot', $ArtifactRoot,
            '-CameraBindingPath', $CameraBindingPath,
            '-ExpectedFallout4Hash', $ExpectedFallout4Hash,
            '-SealTimeoutSeconds', $WindowTimeoutSeconds
        )
        if ($Fallout4Path) { $arguments += @('-Fallout4Path', $Fallout4Path) }
        & $shell @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "single-phase discovery startup failed: exit=$LASTEXITCODE"
        }
        $config = Read-ActiveConfig
        $root = Assert-OutputRoot -Config $config
        $script:ownedOutputRoot = $root
        Write-AtomicUtf8Json -Path (Join-Path $root 'camera-evidence-single-plan.json') -Value ([ordered]@{
            schema = 'camera-evidence-single-plan/v1'
            session_id = [string]$config.session_id
            phase_id = $phase
            draw_window_start = $DrawWindowStart
            draw_window_count = $DrawWindowCount
            sample_every_n_presents = $SampleEveryNPresents
            minimum_window_samples = $MinimumWindowSamples
            artifact_root = [IO.Path]::GetFullPath($ArtifactRoot)
        })
        [void](Wait-Progress -Root $root -PhaseId $phase -WindowStart $DrawWindowStart `
            -WindowSize $DrawWindowCount -MinimumSamples $MinimumWindowSamples `
            -RequireReliable $true)
        $config.enabled = $false
        Write-AtomicUtf8Json -Path $configPath -Value $config
        [void](Wait-SealedReport -Root $root -PhaseId $phase)
        $sealed = Wait-SealedProgress -Root $root -PhaseId $phase `
            -WindowStart $DrawWindowStart -WindowSize $DrawWindowCount `
            -MinimumSamples $MinimumWindowSamples
        Restore-Config -Root $root
        [ordered]@{
            action = 'CaptureOne'
            result = 'single_phase_complete'
            phase_id = $phase
            comparison_samples = [uint64]$sealed.comparison_camera_samples
            boundary_discarded_presents = [uint64]$sealed.phase_boundary_discarded_presents
            boundary_discarded_draws = [uint64]$sealed.phase_boundary_discarded_draws
            report = Join-Path $root "camera-candidates-$phase.json"
        } | ConvertTo-Json -Depth 5
        exit 0
    }

    if ($Action -eq 'StartStatic') {
        if (-not $OutputDir) { throw 'StartStatic requires -OutputDir.' }
        $initialPhase = "POSE_STATIC_W$($DrawWindowStart.ToString('000'))"
        $shell = (Get-Process -Id $PID).Path
        $arguments = @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $baseScript,
            '-Action', 'Start', '-OutputDir', $OutputDir,
            '-RenderWidth', $RenderWidth, '-RenderHeight', $RenderHeight,
            '-InitialPhase', $initialPhase,
            '-DrawWindowStart', $DrawWindowStart,
            '-DrawWindowCount', $DrawWindowCount,
            '-MaxMainPassesPerPresent', $MaxMainPassesPerPresent,
            '-SampleEveryNPresents', $SampleEveryNPresents,
            '-MinimumPhaseSamples', $MinimumWindowSamples,
            '-ArtifactRoot', $ArtifactRoot,
            '-CameraBindingPath', $CameraBindingPath,
            '-ExpectedFallout4Hash', $ExpectedFallout4Hash,
            '-SealTimeoutSeconds', $WindowTimeoutSeconds
        )
        if ($Fallout4Path) { $arguments += @('-Fallout4Path', $Fallout4Path) }
        & $shell @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "camera discovery startup failed: exit=$LASTEXITCODE"
        }
        $config = Read-ActiveConfig
        $root = Assert-OutputRoot -Config $config
        $script:ownedOutputRoot = $root
        $plan = [ordered]@{
            schema = 'camera-evidence-sweep-plan/v1'
            session_id = [string]$config.session_id
            draw_window_start = $DrawWindowStart
            window_count = $WindowCount
            draw_window_count = $DrawWindowCount
            max_main_passes_per_present = $MaxMainPassesPerPresent
            sample_every_n_presents = $SampleEveryNPresents
            minimum_window_samples = $MinimumWindowSamples
            window_timeout_seconds = $WindowTimeoutSeconds
            artifact_root = [IO.Path]::GetFullPath($ArtifactRoot)
        }
        Write-AtomicUtf8Json -Path (Join-Path $root 'camera-evidence-sweep-plan.json') -Value $plan
        Invoke-MacroSweep -Config $config -Root $root -Macro 'STATIC'
        $hold = Enter-Hold -Config $config -Root $root -Macro 'STATIC'
        [ordered]@{
            action = 'StartStatic'
            result = 'static_sweep_complete'
            windows = $WindowCount
            active_phase = $hold
            next = 'RunWalk while translating back and forth without rotating the view.'
        } | ConvertTo-Json -Depth 5
        exit 0
    }

    $config = Read-ActiveConfig
    $root = Assert-OutputRoot -Config $config
    $planPath = Join-Path $root 'camera-evidence-sweep-plan.json'
    if (-not (Test-Path -LiteralPath $planPath -PathType Leaf)) {
        throw "automatic sweep plan is missing: $planPath"
    }
    $plan = Get-Content -LiteralPath $planPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if ([string]$plan.schema -ne 'camera-evidence-sweep-plan/v1') {
        throw "unsupported automatic sweep plan schema: $($plan.schema)"
    }
    if ([string]$plan.session_id -ne [string]$config.session_id) {
        throw "active session does not match the sealed sweep plan: active=$($config.session_id) planned=$($plan.session_id)"
    }
    $stored = [ordered]@{
        DrawWindowStart = [uint32]$plan.draw_window_start
        WindowCount = [uint32]$plan.window_count
        DrawWindowCount = [uint32]$plan.draw_window_count
        MaxMainPassesPerPresent = [uint32]$plan.max_main_passes_per_present
        SampleEveryNPresents = [uint32]$plan.sample_every_n_presents
        MinimumWindowSamples = [uint32]$plan.minimum_window_samples
        WindowTimeoutSeconds = [uint32]$plan.window_timeout_seconds
    }
    foreach ($name in $stored.Keys) {
        $requested = Get-Variable -Name $name -ValueOnly
        if ($PSBoundParameters.ContainsKey($name) -and [uint64]$requested -ne [uint64]$stored[$name]) {
            throw "parameter $name differs from the sealed sweep plan: requested=$requested planned=$($stored[$name])"
        }
        Set-Variable -Name $name -Value $stored[$name]
    }
    $storedArtifactRoot = [IO.Path]::GetFullPath([string]$plan.artifact_root)
    if ($PSBoundParameters.ContainsKey('ArtifactRoot') -and
        -not [IO.Path]::GetFullPath($ArtifactRoot).Equals(
            $storedArtifactRoot,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "ArtifactRoot differs from the sealed sweep plan: requested=$ArtifactRoot planned=$storedArtifactRoot"
    }
    $ArtifactRoot = $storedArtifactRoot
    $current = [string]$config.camera_discovery.phase_id

    if ($Action -eq 'RunWalk') {
        if ($current -ne 'HOLD_AFTER_STATIC') {
            throw "RunWalk requires active phase HOLD_AFTER_STATIC; actual=$current"
        }
        Invoke-MacroSweep -Config $config -Root $root -Macro 'WALK'
        $hold = Enter-Hold -Config $config -Root $root -Macro 'WALK'
        [ordered]@{
            action = 'RunWalk'
            result = 'walk_sweep_complete'
            windows = $WindowCount
            active_phase = $hold
            next = 'RunTurnAndAnalyze while rotating in place without walking.'
        } | ConvertTo-Json -Depth 5
        exit 0
    }

    if ($current -ne 'HOLD_AFTER_WALK') {
        throw "RunTurnAndAnalyze requires active phase HOLD_AFTER_WALK; actual=$current"
    }
    Invoke-MacroSweep -Config $config -Root $root -Macro 'TURN'
    $lastStart = [uint32]($DrawWindowStart + (($WindowCount - 1) * $DrawWindowCount))
    $lastPhase = "POSE_TURN_W$($lastStart.ToString('000'))"
    $config.enabled = $false
    Write-AtomicUtf8Json -Path $configPath -Value $config
    [void](Wait-SealedReport -Root $root -PhaseId $lastPhase)
    [void](Wait-SealedProgress -Root $root -PhaseId $lastPhase `
        -WindowStart $lastStart -WindowSize $DrawWindowCount `
        -MinimumSamples $MinimumWindowSamples)
    Restore-Config -Root $root

    $depthLab = Join-Path $ArtifactRoot 'depth-lab.exe'
    if (-not (Test-Path -LiteralPath $depthLab -PathType Leaf)) {
        throw "release analyzer is missing: $depthLab"
    }
    $reportPath = Join-Path $root 'camera-evidence-sweep-report.json'
    & $depthLab camera-evidence-sweep --root $root --out $reportPath --analysis-mode fallout-render-hybrid
    $analysisExit = $LASTEXITCODE
    if ($analysisExit -gt 1) {
        throw "camera evidence sweep analysis failed: exit=$analysisExit"
    }
    [ordered]@{
        action = 'RunTurnAndAnalyze'
        result = if ($analysisExit -eq 0) { 'unique_authoring_candidate' } else { 'no_unique_candidate' }
        windows = $WindowCount
        report = $reportPath
        production_binding_authorized = $false
        next = 'Inspect the sweep report; a unique candidate still requires RGB/depth E-geo.'
    } | ConvertTo-Json -Depth 5
    exit $analysisExit
} catch {
    [void](Stop-FailClosed)
    throw
}

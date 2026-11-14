<#
.SYNOPSIS
  #636 FO4 render-camera discovery: start, switch controlled phases, stop, or analyze.

.DESCRIPTION
  This workflow is non-deliverable authoring evidence. It records bounded GPU view-matrix tracks
  and an exact-build PlayerCamera reference on the same sampled Presents. The operator owns all in-game
  actions; this script never focuses the game, sends input, or changes Fallout settings.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Start', 'SetPhase', 'Stop', 'Abort', 'Analyze')]
    [string]$Action,

    [string]$OutputDir = '',
    [ValidatePattern('^(?:POSE_(STATIC|WALK|TURN)(?:_W\d{3})?|PROJECTION_FOV75_A)$')]
    [string]$InitialPhase = 'POSE_STATIC',
    [ValidateSet('POSE_WALK', 'POSE_TURN', 'PROJECTION_HOLD90', 'PROJECTION_FOV90', 'PROJECTION_HOLD75', 'PROJECTION_FOV75_B')]
    [string]$PhaseId = 'POSE_WALK',
    [ValidateRange(1, 16384)]
    [uint32]$RenderWidth = 1920,
    [ValidateRange(1, 16384)]
    [uint32]$RenderHeight = 1080,
    [uint32]$DrawWindowStart = 0,
    [ValidateRange(1, 256)]
    [uint32]$DrawWindowCount = 8,
    [ValidateRange(1, 16)]
    [uint32]$MaxMainPassesPerPresent = 1,
    [ValidateRange(1, 600)]
    [uint32]$SampleEveryNPresents = 30,
    [ValidateRange(8, 4096)]
    [uint32]$MinimumPhaseSamples = 24,
    [string]$SessionId = '',
    [string]$Fallout4Path = '',
    [string]$ArtifactRoot = '',
    [string]$CameraBindingPath = '',
    [string]$ExpectedFallout4Hash = '428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61',
    [ValidateRange(1, 120)]
    [uint32]$SealTimeoutSeconds = 30
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$configPath = Join-Path $env:ProgramData 'DataMiner\depth-lab\probe-config.json'
if (-not $Fallout4Path) {
    $Fallout4Path = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) `
        'Steam\steamapps\common\Fallout 4\Fallout4.exe'
}
if (-not $ArtifactRoot) { $ArtifactRoot = Join-Path $dataMinerRoot 'target\release' }
if (-not $CameraBindingPath) {
    $CameraBindingPath = Join-Path $dataMinerRoot 'config\catalogs\dev\fallout4-steam-release-test\camera-binding.json'
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
        throw "probe 配置不存在：$configPath"
    }
    $config = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not [bool]$config.enabled -or -not [bool]$config.camera_probe -or
        [string]$config.camera_discovery.schema -ne 'camera-discovery-config/v2') {
        throw '当前不是 #636 camera evidence discovery；拒绝碰其他采集任务。'
    }
    return $config
}

function Read-Report {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    return Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
}

function Test-FalloutProbeModuleName {
    param([string]$Name)
    # Managed capture loads immutable hash-named snapshots, not always the plain DLL name.
    return $Name -imatch '^dm_depth_probe(?:-[0-9a-f]{64})?\.dll$'
}

function Assert-CameraReference {
    param([object]$Binding, [string]$ActualHash)
    if ([string]$Binding.schema -notin @('camera-binding/v1', 'fallout4-camera-reference/v1')) {
        throw '需要 CameraBinding v1 或独立 authoring camera reference；不接受其他格式。'
    }
    if ([string]$Binding.executable_sha256 -cne "sha256:$ActualHash") {
        throw 'Camera reference 的 EXE hash 与当前游戏不一致，拒绝沿用其他版本的对象链。'
    }
    if ($null -eq $Binding.game_camera -or
        [string]$Binding.game_camera.provider -ne 'fallout4-player-camera-root-exact-build/v1') {
        throw 'Camera reference 没有 exact-build PlayerCamera singleton。'
    }
}

function Wait-SealedReport {
    param([string]$Root, [string]$SealedPhase)
    $path = Join-Path $Root "camera-candidates-$SealedPhase.json"
    $deadline = [DateTime]::UtcNow.AddSeconds($SealTimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $path -PathType Leaf) { return $path }
        Start-Sleep -Milliseconds 250
    }
    throw "等待 phase 封口超时：$path"
}

function Wait-ActivePhaseReport {
    param([string]$Root, [string]$ExpectedPhase, [DateTime]$NotBeforeUtc)
    $path = Join-Path $Root 'camera-candidates.json'
    $deadline = [DateTime]::UtcNow.AddSeconds($SealTimeoutSeconds)
    $last = '尚未看到新 phase 报告'
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $file = Get-Item -LiteralPath $path -ErrorAction Stop
            if ($file.LastWriteTimeUtc -ge $NotBeforeUtc) {
                $report = Read-Report -Path $path
                $referenceCount = @($report.comparison_camera_samples).Count
                $trackCount = @($report.view_candidates | Where-Object {
                    @($_.observations).Count -gt 0
                }).Count
                if ($report.schema -ne 'camera-discovery-report/v4') {
                    $last = "schema=$($report.schema)"
                } elseif ([string]$report.phase_id -ne $ExpectedPhase) {
                    $last = "phase=$($report.phase_id)"
                } elseif ([uint64]$report.processed_draws -eq 0) {
                    $last = 'processed_draws=0'
                } elseif ($referenceCount -eq 0) {
                    $last = 'comparison_camera_samples=0'
                } elseif ($trackCount -eq 0) {
                    $last = '没有带 observations 的 view candidate'
                } else {
                    return $report
                }
            }
        } catch { $last = $_.Exception.Message }
        Start-Sleep -Milliseconds 250
    }
    throw "等待 phase=$ExpectedPhase 激活超时：$last"
}

function Assert-PhaseReady {
    param([object]$Report, [string]$ExpectedPhase, [bool]$RequireSealed)
    if ($null -eq $Report) { throw "phase=$ExpectedPhase 报告不存在" }
    if ($Report.schema -ne 'camera-discovery-report/v4' -or
        [string]$Report.phase_id -ne $ExpectedPhase) {
        throw "phase 身份不匹配：expected=$ExpectedPhase actual=$($Report.phase_id) schema=$($Report.schema)"
    }
    $samples = @($Report.comparison_camera_samples).Count
    if ($samples -lt $MinimumPhaseSamples) {
        throw "phase=$ExpectedPhase 只有 $samples 个同帧样本，至少需要 $MinimumPhaseSamples；先继续保持该动作。"
    }
    if ($RequireSealed -and (-not [bool]$Report.reliable -or @($Report.blockers).Count -ne 0)) {
        throw "phase=$ExpectedPhase 不可靠：$(@($Report.blockers) -join ',')"
    }
}

function Get-NextEvidencePhase {
    param([string]$Current)
    switch ($Current) {
        'POSE_STATIC' { return 'POSE_WALK' }
        'POSE_WALK' { return 'POSE_TURN' }
        'PROJECTION_FOV75_A' { return 'PROJECTION_HOLD90' }
        'PROJECTION_HOLD90' { return 'PROJECTION_FOV90' }
        'PROJECTION_FOV90' { return 'PROJECTION_HOLD75' }
        'PROJECTION_HOLD75' { return 'PROJECTION_FOV75_B' }
        default { throw "phase=$Current has no next phase; Stop or Abort this run." }
    }
}

if ($Action -eq 'Analyze') {
    if (-not $OutputDir) { throw 'Analyze 需要 -OutputDir。' }
    $depthLab = Join-Path $ArtifactRoot 'depth-lab.exe'
    if (-not (Test-Path -LiteralPath $depthLab -PathType Leaf)) {
        throw "缺 release 分析器：$depthLab"
    }
    $reportPath = Join-Path $OutputDir 'camera-evidence-report.json'
    & $depthLab camera-evidence `
        --analysis-mode fallout-render-hybrid `
        --static (Join-Path $OutputDir 'camera-candidates-POSE_STATIC.json') `
        --walk (Join-Path $OutputDir 'camera-candidates-POSE_WALK.json') `
        --turn (Join-Path $OutputDir 'camera-candidates-POSE_TURN.json') `
        --out $reportPath
    if ($LASTEXITCODE -ne 0) {
        throw "camera evidence 分析未得到唯一候选，exit=$LASTEXITCODE；见 $reportPath"
    }
    Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8
    exit 0
}

if ($Action -eq 'Start') {
    if (-not $OutputDir) { throw 'Start 需要 -OutputDir。' }
    if ($DrawWindowStart + $DrawWindowCount -gt 512 -or
        $MaxMainPassesPerPresent * $DrawWindowCount -gt 512) {
        throw 'draw window 超出每 sampled Present 的 512 draw 硬上限。'
    }
    foreach ($path in @($Fallout4Path, $CameraBindingPath)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "文件不存在：$path" }
    }
    $actualHash = (Get-FileHash -LiteralPath $Fallout4Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $ExpectedFallout4Hash) {
        throw "Fallout4 build 不匹配：expected=$ExpectedFallout4Hash actual=$actualHash"
    }
    $binding = Get-Content -LiteralPath $CameraBindingPath -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-CameraReference -Binding $binding -ActualHash $actualHash
    if (Test-Path -LiteralPath $OutputDir) {
        throw "输出目录已存在，拒绝混证据：$OutputDir"
    }
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='Fallout4.exe'" |
        Where-Object {
            $_.ExecutablePath -and
            [IO.Path]::GetFullPath([string]$_.ExecutablePath) -eq
                [IO.Path]::GetFullPath($Fallout4Path)
        })
    if ($processes.Count -ne 1) { throw '需要且只能有一个精确路径的 Fallout4.exe 进程。' }
    $process = $processes[0]
    $liveProcess = Get-Process -Id ([uint32]$process.ProcessId) -ErrorAction Stop
    $startFileTime = $liveProcess.StartTime.ToUniversalTime().ToFileTimeUtc()
    $resident = @($liveProcess.Modules | Where-Object { Test-FalloutProbeModuleName $_.ModuleName })
    if ($resident.Count -ne 0) {
        throw '开始独立取证前请完全重启 Fallout4；旧探针不能被新 DLL 覆盖，也不能跨会话混证据。'
    }

    $dll = Join-Path $ArtifactRoot 'dm_depth_probe.dll'
    $injector = Join-Path $ArtifactRoot 'dm-depth-inject.exe'
    foreach ($artifact in @($dll, $injector)) {
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "缺 release 产物：$artifact" }
    }
    $probeHash = 'sha256:' + (Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.Directory]::CreateDirectory($OutputDir) | Out-Null
    $backup = Join-Path $OutputDir 'probe-config.before.json'
    if (Test-Path -LiteralPath $configPath -PathType Leaf) {
        $prior = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ([bool]$prior.enabled) { throw '已有启用中的 probe 配置；拒绝覆盖。' }
        Copy-Item -LiteralPath $configPath -Destination $backup
    }
    if (-not $SessionId) {
        $SessionId = "fo4-camera-evidence-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))"
    }
    $config = [ordered]@{
        enabled = $true
        output_dir = [IO.Path]::GetFullPath($OutputDir)
        unbind_from_end = 5
        max_frames = $null
        target_depth_hz = 1.0
        camera_probe = $true
        camera_discovery = [ordered]@{
            schema = 'camera-discovery-config/v2'
            phase_id = $InitialPhase
            executable_sha256 = "sha256:$actualHash"
            render = [ordered]@{
                width = $RenderWidth
                height = $RenderHeight
                sample_count = 1
                formats = @('r24_g8_typeless', 'd24_unorm_s8_uint')
            }
            sample_every_n_presents = $SampleEveryNPresents
            max_main_passes_per_present = $MaxMainPassesPerPresent
            draw_window_start = $DrawWindowStart
            draw_window_count = $DrawWindowCount
            comparison_camera_reference = $binding.game_camera
        }
        camera_production = $null
        depth_profile_path = $null
        depth_profile_sha256 = $null
        session_id = $SessionId
    }
    Write-AtomicUtf8Json -Path $configPath -Value $config
    try {
        & $injector --pid ([uint32]$process.ProcessId) --dll $dll `
            --expected-sha256 $probeHash --expected-exe ([IO.Path]::GetFullPath($Fallout4Path)) `
            --expected-exe-sha256 "sha256:$actualHash" --expected-start-time-filetime $startFileTime
        if ($LASTEXITCODE -ne 0) {
            throw "probe 注入失败，exit=$LASTEXITCODE"
        }
        $active = Wait-ActivePhaseReport -Root $OutputDir -ExpectedPhase $InitialPhase `
            -NotBeforeUtc ([DateTime]::UtcNow.AddSeconds(-$SealTimeoutSeconds))
    } catch {
        $config.enabled = $false
        Write-AtomicUtf8Json -Path $configPath -Value $config
        if (Test-Path -LiteralPath $backup -PathType Leaf) {
            Copy-Item -LiteralPath $backup -Destination $configPath -Force
        }
        throw
    }
    [ordered]@{
        action = 'Start'
        session_id = $SessionId
        phase_id = $InitialPhase
        pid = [uint32]$process.ProcessId
        output_dir = [IO.Path]::GetFullPath($OutputDir)
        comparison_samples = @($active.comparison_camera_samples).Count
        operator_instruction = if ($InitialPhase -eq 'PROJECTION_FOV75_A') {
            'Projection-only evidence: hold still at FOV 75; seal into PROJECTION_HOLD90 before changing FOV. Never analyze these as WALK/TURN.'
        } elseif ($InitialPhase -eq 'POSE_TURN') {
            '仅补原地转身段；不要走动，样本够后执行 Stop。'
        } else {
            '保持完全静止；样本够后再执行 SetPhase -PhaseId POSE_WALK。'
        }
    } | ConvertTo-Json -Depth 5
    exit 0
}

$active = Read-ActiveConfig
$activeRoot = [string]$active.output_dir
if ($OutputDir -and -not [IO.Path]::GetFullPath($OutputDir).Equals(
    [IO.Path]::GetFullPath($activeRoot),
    [StringComparison]::OrdinalIgnoreCase
)) {
    throw "指定输出目录不是当前活动取证：requested=$OutputDir active=$activeRoot"
}
$activePhase = [string]$active.camera_discovery.phase_id

if ($Action -eq 'Abort') {
    $active.enabled = $false
    Write-AtomicUtf8Json -Path $configPath -Value $active
    $sealedPath = Wait-SealedReport -Root $activeRoot -SealedPhase $activePhase
    $backup = Join-Path $activeRoot 'probe-config.before.json'
    if (Test-Path -LiteralPath $backup -PathType Leaf) {
        Copy-Item -LiteralPath $backup -Destination $configPath -Force
    }
    [ordered]@{
        action = 'Abort'
        sealed = $sealedPath
        reason = 'operator_requested_fail_closed_abort'
    } | ConvertTo-Json -Depth 4
    exit 0
}

if ($Action -eq 'SetPhase') {
    $expectedNext = Get-NextEvidencePhase $activePhase
    if ($PhaseId -ne $expectedNext) {
        throw "Evidence phase order mismatch: current=$activePhase expected=$expectedNext."
    }
    $active.camera_discovery.phase_id = $PhaseId
    $switchAt = [DateTime]::UtcNow
    Write-AtomicUtf8Json -Path $configPath -Value $active
    $sealedPath = Wait-SealedReport -Root $activeRoot -SealedPhase $activePhase
    Assert-PhaseReady -Report (Read-Report -Path $sealedPath) `
        -ExpectedPhase $activePhase -RequireSealed $true
    $newReport = Wait-ActivePhaseReport -Root $activeRoot -ExpectedPhase $PhaseId `
        -NotBeforeUtc $switchAt
    [ordered]@{
        action = 'SetPhase'
        sealed = $sealedPath
        active_phase_id = $PhaseId
        comparison_samples = @($newReport.comparison_camera_samples).Count
        operator_instruction = if ($PhaseId -eq 'PROJECTION_HOLD90') {
            'The initial 75-degree phase is sealed. Change FOV to 90, close the console, then select PROJECTION_FOV90.'
        } elseif ($PhaseId -eq 'PROJECTION_FOV90') {
            'Hold still at FOV 90; seal into PROJECTION_HOLD75 before restoring FOV.'
        } elseif ($PhaseId -eq 'PROJECTION_HOLD75') {
            'The 90-degree phase is sealed. Restore FOV to 75, close the console, then select PROJECTION_FOV75_B.'
        } elseif ($PhaseId -eq 'PROJECTION_FOV75_B') {
            'Hold still at FOV 75; Stop after the required sample count.'
        } elseif ($PhaseId -eq 'POSE_WALK') {
            '直线行走，不转视角；样本够后切 POSE_TURN。'
        } else {
            '原地转身，不走动；样本够后执行 Stop。'
        }
    } | ConvertTo-Json -Depth 5
    exit 0
}

if ($activePhase -notin @('POSE_TURN', 'PROJECTION_FOV75_B')) {
    throw "Stop requires POSE_TURN or PROJECTION_FOV75_B; current=$activePhase."
}
$active.enabled = $false
Write-AtomicUtf8Json -Path $configPath -Value $active
$sealedPath = Wait-SealedReport -Root $activeRoot -SealedPhase $activePhase
Assert-PhaseReady -Report (Read-Report -Path $sealedPath) `
    -ExpectedPhase $activePhase -RequireSealed $true
$backup = Join-Path $activeRoot 'probe-config.before.json'
if (Test-Path -LiteralPath $backup -PathType Leaf) {
    Copy-Item -LiteralPath $backup -Destination $configPath -Force
}
[ordered]@{
    action = 'Stop'
    sealed = $sealedPath
    next = if ($activePhase -eq 'PROJECTION_FOV75_B') {
        'Audit the sealed 75/90/75 projection reports separately; these are not STATIC/WALK/TURN evidence or E-geo authorization.'
    } else {
        "运行同一脚本 -Action Analyze -OutputDir '$activeRoot'"
    }
} | ConvertTo-Json -Depth 4

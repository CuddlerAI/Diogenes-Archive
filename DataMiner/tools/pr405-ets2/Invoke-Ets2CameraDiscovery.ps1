<#
.SYNOPSIS
  在已运行的 ETS2 精确构建上启动、切换或停止 D3D11 camera/depth discovery。

.DESCRIPTION
  本脚本只生成不可交付的 discovery 证据。它钉住 ETS2 EXE SHA-256、显式渲染尺寸、
  单采样以及 ETS2 实测的 64-bit D32+S8 深度格式；配置以原子替换方式写入
  %ProgramData%\DataMiner\depth-lab\probe-config.json。

  Start 会拒绝复用非空输出目录并注入 probe；SetPhase 会在同一进程内封口上一 phase，
  并等待新 phase 的 mutable 报告实际处理 draw 后才返回成功；Stop 先令 probe Flush/封口，
  再恢复启动前的 disabled 配置（若有）。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Start', 'SetPhase', 'Stop')]
    [string]$Action,

    [string]$OutputDir = '',
    [string]$PhaseId = 'A1',
    [ValidateRange(1, 16384)]
    [uint32]$RenderWidth = 1920,
    [ValidateRange(1, 16384)]
    [uint32]$RenderHeight = 1080,
    [uint32]$DrawWindowStart = 0,
    [ValidateRange(1, 256)]
    [uint32]$DrawWindowCount = 128,
    [ValidateRange(1, 16)]
    [uint32]$MaxMainPassesPerPresent = 4,
    [ValidateRange(1, 600)]
    [uint32]$SampleEveryNPresents = 30,
    [string]$SessionId = '',
    [string]$Ets2Path = '',
    [string]$ArtifactRoot = '',
    [string]$ExpectedEts2Hash = 'b7dffe6b27402c7db6dfd52cf982cd5bf292584138b35e3eb8efb311814ab3f8',
    [ValidateRange(1, 120)]
    [uint32]$SealTimeoutSeconds = 30
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$configPath = Join-Path $env:ProgramData 'DataMiner\depth-lab\probe-config.json'
$configDir = Split-Path -Parent $configPath
if (-not $Ets2Path) {
    $Ets2Path = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) `
        'Steam\steamapps\common\Euro Truck Simulator 2\bin\win_x64\eurotrucks2.exe'
}
if (-not $ArtifactRoot) { $ArtifactRoot = Join-Path $dataMinerRoot 'target\release' }

function Write-AtomicUtf8Json {
    param([string]$Path, [object]$Value)
    $dir = Split-Path -Parent $Path
    [IO.Directory]::CreateDirectory($dir) | Out-Null
    $tmp = "$Path.tmp"
    $json = $Value | ConvertTo-Json -Depth 12
    [IO.File]::WriteAllText($tmp, $json, [Text.UTF8Encoding]::new($false))
    if (Test-Path -LiteralPath $Path) {
        $replaceBackup = Join-Path $dir ([IO.Path]::GetRandomFileName())
        try {
            [IO.File]::Replace($tmp, $Path, $replaceBackup)
        } finally {
            Remove-Item -LiteralPath $replaceBackup -Force -ErrorAction SilentlyContinue
        }
    } else {
        Move-Item -LiteralPath $tmp -Destination $Path
    }
}

function Read-ActiveConfig {
    if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
        throw "probe 配置不存在：$configPath"
    }
    $config = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not [bool]$config.enabled -or -not [bool]$config.camera_probe) {
        throw '当前 probe 配置不是启用中的 camera discovery；拒绝修改其他采集任务。'
    }
    return $config
}

function Wait-SealedReport {
    param([string]$Root, [string]$SealedPhase)
    $path = Join-Path $Root "camera-candidates-$SealedPhase.json"
    $deadline = [DateTime]::UtcNow.AddSeconds($SealTimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $path -PathType Leaf) { return $path }
        Start-Sleep -Milliseconds 250
    }
    throw "等待 discovery Flush/封口超时：$path"
}

function Wait-ActivePhaseReport {
    param(
        [string]$Root,
        [string]$ExpectedSessionId,
        [string]$ExpectedPhaseId,
        [DateTime]$NotBeforeUtc
    )
    $path = Join-Path $Root 'camera-candidates.json'
    $deadline = [DateTime]::UtcNow.AddSeconds($SealTimeoutSeconds)
    $lastObservation = '尚未看到新 phase 的 mutable 报告'
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $liveConfig = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 |
                ConvertFrom-Json
        } catch {
            $lastObservation = "probe 配置暂时不可读：$($_.Exception.Message)"
            Start-Sleep -Milliseconds 250
            continue
        }
        if (-not [bool]$liveConfig.enabled) {
            throw "等待新 phase 激活时 probe 已被停用：phase=$ExpectedPhaseId"
        }
        if ([string]$liveConfig.session_id -ne $ExpectedSessionId) {
            throw "等待新 phase 激活时 session 已被替换：expected=$ExpectedSessionId actual=$($liveConfig.session_id)"
        }
        if ([string]$liveConfig.camera_discovery.phase_id -ne $ExpectedPhaseId) {
            throw "等待新 phase 激活时配置已切到其他 phase：expected=$ExpectedPhaseId actual=$($liveConfig.camera_discovery.phase_id)"
        }

        try {
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                $reportFile = Get-Item -LiteralPath $path
                if ($reportFile.LastWriteTimeUtc -ge $NotBeforeUtc) {
                    $report = Get-Content -LiteralPath $path -Raw -Encoding UTF8 |
                        ConvertFrom-Json
                    $reportPhase = [string]$report.phase_id
                    $reportConfigPhase = [string]$report.config.phase_id
                    $processedDraws = [uint64]$report.processed_draws
                    if ($report.schema -ne 'camera-discovery-report/v1') {
                        $lastObservation = "mutable 报告 schema 不匹配：$($report.schema)"
                    } elseif ($reportPhase -ne $ExpectedPhaseId -or $reportConfigPhase -ne $ExpectedPhaseId) {
                        $lastObservation = "mutable 报告仍属于 phase=$reportPhase"
                    } elseif ($processedDraws -eq 0) {
                        $lastObservation = "新 phase 已生成报告，但 processed_draws=0"
                    } else {
                        return [pscustomobject]@{
                            path = $path
                            processed_draws = $processedDraws
                            observed_presents = [uint64]$report.sampling.observed_presents
                            submitted_draws = [uint64]$report.sampling.submitted_draws
                        }
                    }
                }
            }
        } catch {
            # mutable 报告通过临时文件替换；轮询可能刚好撞到删除/重命名窗口。
            $lastObservation = "mutable 报告暂时不可读：$($_.Exception.Message)"
        }
        Start-Sleep -Milliseconds 250
    }
    throw "等待新 discovery phase 激活超时：phase=$ExpectedPhaseId path=$path last=$lastObservation"
}

if ($Action -eq 'Start') {
    if (-not $OutputDir) { throw 'Start 需要 -OutputDir。' }
    if ($DrawWindowStart + $DrawWindowCount -gt 512) {
        throw 'DrawWindowStart + DrawWindowCount 不得超过 512。'
    }
    if ($MaxMainPassesPerPresent * $DrawWindowCount -gt 512) {
        throw 'MaxMainPassesPerPresent × DrawWindowCount 不得超过 512。'
    }
    if (-not (Test-Path -LiteralPath $Ets2Path -PathType Leaf)) {
        throw "ETS2 可执行文件不存在：$Ets2Path"
    }
    $actualHash = (Get-FileHash -LiteralPath $Ets2Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $ExpectedEts2Hash) {
        throw "ETS2 build 不匹配：expected=$ExpectedEts2Hash actual=$actualHash"
    }
    if (Test-Path -LiteralPath $OutputDir) {
        throw "discovery 输出目录已存在，拒绝混入旧证据：$OutputDir"
    }
    [IO.Directory]::CreateDirectory($OutputDir) | Out-Null

    $process = Get-CimInstance Win32_Process -Filter "Name='eurotrucks2.exe'" |
        Where-Object {
            $_.ExecutablePath -and
            [IO.Path]::GetFullPath([string]$_.ExecutablePath) -eq [IO.Path]::GetFullPath($Ets2Path)
        } |
        Select-Object -First 1
    if ($null -eq $process) {
        throw '未找到该精确路径的 eurotrucks2.exe；请先进入 1920x1080 驾驶场景。'
    }

    $dll = Join-Path $ArtifactRoot 'dm_depth_probe.dll'
    $injector = Join-Path $ArtifactRoot 'dm-depth-inject.exe'
    foreach ($artifact in @($dll, $injector)) {
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "缺 release 产物：$artifact" }
    }

    $backup = Join-Path $OutputDir 'probe-config.before.json'
    if (Test-Path -LiteralPath $configPath -PathType Leaf) {
        $prior = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ([bool]$prior.enabled) { throw '已有启用中的 probe 配置；拒绝覆盖另一个采集任务。' }
        Copy-Item -LiteralPath $configPath -Destination $backup
    }
    if (-not $SessionId) { $SessionId = "ets2-discovery-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))" }
    $config = [ordered]@{
        enabled = $true
        output_dir = [IO.Path]::GetFullPath($OutputDir)
        unbind_from_end = 4
        max_frames = $null
        target_depth_hz = 1.0
        camera_probe = $true
        camera_discovery = [ordered]@{
            schema = 'camera-discovery-config/v1'
            phase_id = $PhaseId
            executable_sha256 = "sha256:$actualHash"
            render = [ordered]@{
                width = $RenderWidth
                height = $RenderHeight
                sample_count = 1
                formats = @('r32_g8_x24_typeless', 'd32_float_s8_x24_uint')
            }
            sample_every_n_presents = $SampleEveryNPresents
            max_main_passes_per_present = $MaxMainPassesPerPresent
            draw_window_start = $DrawWindowStart
            draw_window_count = $DrawWindowCount
        }
        camera_production = $null
        depth_profile_path = $null
        session_id = $SessionId
    }
    Write-AtomicUtf8Json -Path $configPath -Value $config
    & $injector --pid ([uint32]$process.ProcessId) --dll $dll
    if ($LASTEXITCODE -ne 0) {
        $config.enabled = $false
        Write-AtomicUtf8Json -Path $configPath -Value $config
        throw "probe 注入失败，exit=$LASTEXITCODE"
    }
    [ordered]@{
        action = 'Start'
        session_id = $SessionId
        phase_id = $PhaseId
        pid = [uint32]$process.ProcessId
        executable_sha256 = "sha256:$actualHash"
        output_dir = [IO.Path]::GetFullPath($OutputDir)
        config_path = $configPath
    } | ConvertTo-Json -Depth 4
    exit 0
}

$active = Read-ActiveConfig
$activeRoot = [string]$active.output_dir
$activePhase = [string]$active.camera_discovery.phase_id

if ($Action -eq 'SetPhase') {
    if ($DrawWindowStart + $DrawWindowCount -gt 512) {
        throw 'DrawWindowStart + DrawWindowCount 不得超过 512。'
    }
    if ($MaxMainPassesPerPresent * $DrawWindowCount -gt 512) {
        throw 'MaxMainPassesPerPresent × DrawWindowCount 不得超过 512。'
    }
    if ($PhaseId -eq $activePhase) { throw "新 PhaseId 与当前相同：$PhaseId" }
    $targetSealed = Join-Path $activeRoot "camera-candidates-$PhaseId.json"
    if (Test-Path -LiteralPath $targetSealed -PathType Leaf) {
        throw "目标 PhaseId 已有固定报告，拒绝把旧证据误认成新激活：$targetSealed"
    }
    $active.camera_discovery.phase_id = $PhaseId
    $active.camera_discovery.sample_every_n_presents = $SampleEveryNPresents
    $active.camera_discovery.max_main_passes_per_present = $MaxMainPassesPerPresent
    $active.camera_discovery.draw_window_start = $DrawWindowStart
    $active.camera_discovery.draw_window_count = $DrawWindowCount
    $switchRequestedAtUtc = [DateTime]::UtcNow
    Write-AtomicUtf8Json -Path $configPath -Value $active
    $sealed = Wait-SealedReport -Root $activeRoot -SealedPhase $activePhase
    $activated = Wait-ActivePhaseReport `
        -Root $activeRoot `
        -ExpectedSessionId ([string]$active.session_id) `
        -ExpectedPhaseId $PhaseId `
        -NotBeforeUtc $switchRequestedAtUtc
    [ordered]@{
        action = 'SetPhase'
        sealed = $sealed
        active_phase_id = $PhaseId
        active_report = $activated.path
        processed_draws = $activated.processed_draws
        observed_presents = $activated.observed_presents
        submitted_draws = $activated.submitted_draws
    } |
        ConvertTo-Json -Depth 3
    exit 0
}

$active.enabled = $false
Write-AtomicUtf8Json -Path $configPath -Value $active
$sealed = Wait-SealedReport -Root $activeRoot -SealedPhase $activePhase
$backup = Join-Path $activeRoot 'probe-config.before.json'
if (Test-Path -LiteralPath $backup -PathType Leaf) {
    Copy-Item -LiteralPath $backup -Destination $configPath -Force
}
[ordered]@{ action = 'Stop'; sealed = $sealed; restored_prior_config = (Test-Path -LiteralPath $backup) } |
    ConvertTo-Json -Depth 3

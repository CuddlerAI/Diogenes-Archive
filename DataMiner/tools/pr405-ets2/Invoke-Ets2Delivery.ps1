<#
.SYNOPSIS
  ETS2 采集目录 → 68 号五件套。一条命令跑完 pair → ets2-pose → deliver。

.DESCRIPTION
  每一步失败就当场停，并保留已产出的中间件供排障。**不会**为了让流程走完
  而放宽任何门限——放宽的代价是一批看起来正常、实际错位的数据。

  产物落在 -OutRoot/<ClipId>/：
    video.mp4 · depth.zip · action_camera.json · action_camera_30fps.json
    systeminfo.json · delivery-manifest.json · ets2-pose-report.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CaptureDir,
    [Parameter(Mandatory = $true)][string]$TelemetryPath,
    [Parameter(Mandatory = $true)][string]$FramesPath,
    [Parameter(Mandatory = $true)][string]$WatermarkSamplesPath,
    [Parameter(Mandatory = $true)][string]$ClipId,
    [Parameter(Mandatory = $true)][string]$OutRoot,
    [string]$VideoPath = '',
    [string]$PoseConfigPath = '',
    [string]$DepthLabExe = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $PoseConfigPath) { $PoseConfigPath = Join-Path $PSScriptRoot 'ets2-pose-config.json' }
if (-not $DepthLabExe) { $DepthLabExe = Join-Path $dataMinerRoot 'target\release\depth-lab.exe' }
if (-not (Test-Path -LiteralPath $DepthLabExe -PathType Leaf)) {
    throw "找不到 depth-lab.exe：$DepthLabExe（先 cargo build --release -p depth-lab --features ets2）"
}

$work = Join-Path $OutRoot "_work\$ClipId"
New-Item -ItemType Directory -Path $work -Force | Out-Null
$pairsPath = Join-Path $work 'frame-pairs.jsonl'
$pairReportPath = Join-Path $work 'e2e-report.json'
$posePath = Join-Path $work 'pose.jsonl'
# 15fps 侧按 present_seq 去重（CFR duplicate 帧合法共用一个 Present）。
$poseIndexPath = Join-Path $work 'pose.index.jsonl'
$poseReportPath = Join-Path $work 'ets2-pose-report.json'

function Invoke-Step {
    param([string]$Name, [string[]]$Arguments)
    Write-Host "==> $Name"
    & $DepthLabExe @Arguments
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        # 退出码 1 = 门没过（产物已写出，可据此排障）；2 = 工具/用法错。
        # 两者都不许继续，但错因不同，所以原样带出来。
        throw "$Name 失败（exit $code）。中间件保留在 $work"
    }
}

# ---- 1. 逐帧配对 ----
# 水印解码给出 present_seq，这是后面两步共同的键。
Invoke-Step '配对（pair）' @(
    'pair'
    '--frames', $FramesPath
    '--samples', $WatermarkSamplesPath
    '--depth', (Join-Path $CaptureDir 'depth-index.jsonl')
    '--out', $pairsPath
    '--report', $pairReportPath
    '--camera-mode', 'player'
)

# ---- 2. 遥测 → 位姿旁车 ----
Invoke-Step '位姿对齐（ets2-pose）' @(
    'ets2-pose'
    '--telemetry', $TelemetryPath
    '--pairs', $pairsPath
    '--config', $PoseConfigPath
    '--out', $posePath
    '--out-index', $poseIndexPath
    '--report', $poseReportPath
)

# ---- 3. 五件套 ----
$deliverArgs = @(
    'deliver'
    '--capture', $CaptureDir
    '--pairs', $pairsPath
    '--out', $OutRoot
    '--clip-id', $ClipId
    '--pose', $poseIndexPath
    '--pose30', $posePath
)
if ($VideoPath) { $deliverArgs += @('--video', $VideoPath) }
Invoke-Step '交付（deliver）' $deliverArgs

$clipDir = Join-Path $OutRoot $ClipId
Copy-Item -LiteralPath $poseReportPath -Destination (Join-Path $clipDir 'ets2-pose-report.json') -Force

# systeminfo.json 描述的是**录制当时**的窗口与缩放，不能事后重建。
# 这里只校验它已经随采集一起产出——缺了就是这条数据不完整。
$systemInfo = Join-Path $CaptureDir 'systeminfo.json'
if (Test-Path -LiteralPath $systemInfo -PathType Leaf) {
    Copy-Item -LiteralPath $systemInfo -Destination (Join-Path $clipDir 'systeminfo.json') -Force
} else {
    Write-Warning "缺 systeminfo.json（68 号 §6.2）：$systemInfo —— 这条数据按五件套口径不完整"
}

Write-Host ""
Write-Host "交付目录：$clipDir"
Get-ChildItem -LiteralPath $clipDir | Select-Object Name, Length | Format-Table | Out-String | Write-Host
Write-Host "位姿报告（skew 统计与标定状态）：$(Join-Path $clipDir 'ets2-pose-report.json')"

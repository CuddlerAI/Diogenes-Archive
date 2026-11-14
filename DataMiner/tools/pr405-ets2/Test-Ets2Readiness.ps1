<#
.SYNOPSIS
  ETS2 PR 405 采集前自检。**fail-closed**：任何一条不过就 exit 2，不要开采。

.DESCRIPTION
  与 tools/pr405-fallout4/Test-Pr405Readiness.ps1 同形，但检的是 ETS2 这条链
  独有的前置：SCS 遥测插件是否装好、相机基线是否有证据、驾驶舱抖动是否关掉、
  游戏帧率是否锁 30。

  每一条都是「采完就补不回来」的：帧率没锁 30 会让对齐率掉到 50% 上下（#501 的
  教训，61 GiB 全废）；驾驶舱抖动开着会让相机位姿带上未标定的复合次序；
  基线缺证据则整批内参说不清来源。
#>
[CmdletBinding()]
param(
    [string]$Ets2Path = '',
    [string]$ExpectedEts2Hash = 'b7dffe6b27402c7db6dfd52cf982cd5bf292584138b35e3eb8efb311814ab3f8',
    [string]$PoseConfigPath = '',
    [string]$ArtifactRoot = '',
    [string]$PluginPath = '',
    [string]$Ets2ConfigCfgPath = '',
    [string]$PerfCertificationManifestPath = '',
    [string]$ProbeRuntimeManifestPath = '',
    [string]$PerfCertificationKeyPath = '',
    [string]$PerfHardwarePath = '',
    [uint32]$MinFreeGB = 60
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$probeConfigPath = Join-Path $env:ProgramData 'DataMiner\depth-lab\probe-config.json'

if (-not $Ets2Path) {
    $Ets2Path = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Steam\steamapps\common\Euro Truck Simulator 2\bin\win_x64\eurotrucks2.exe'
}
if (-not $PluginPath) {
    $PluginPath = Join-Path (Split-Path -Parent $Ets2Path) 'plugins\ets2_telemetry_plugin.dll'
}
if (-not $Ets2ConfigCfgPath) {
    $Ets2ConfigCfgPath = Join-Path $env:USERPROFILE 'Documents\Euro Truck Simulator 2\config.cfg'
}
if (-not $PoseConfigPath) { $PoseConfigPath = Join-Path $PSScriptRoot 'ets2-pose-config.json' }
if (-not $ArtifactRoot) { $ArtifactRoot = Join-Path $dataMinerRoot 'target\release' }
$checks = [System.Collections.Generic.List[object]]::new()
function Add-Check {
    param([string]$Name, [bool]$Passed, [string]$Detail)
    $checks.Add([ordered]@{ name = $Name; passed = $Passed; detail = $Detail })
}

# `uset g_cam_physics "0"` 这类行的取值。取不到返回 $null——
# 「没这一行」与「值是 0」必须能分开：前者是默认值未知，后者是显式关掉。
function Get-UsetValue {
    param([string]$Path, [string]$Name)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $match = Select-String -LiteralPath $Path -Pattern "^uset\s+$([regex]::Escape($Name))\s+`"?([^`"]*)`"?\s*$" |
        Select-Object -Last 1
    if ($null -eq $match) { return $null }
    return $match.Matches[0].Groups[1].Value.Trim()
}

# ---- 1. 构建产物 ----
foreach ($artifact in @('dataminer-client.exe', 'dm_depth_probe.dll', 'dm-depth-inject.exe', 'depth-lab.exe')) {
    $path = Join-Path $ArtifactRoot $artifact
    Add-Check "release:$artifact" (Test-Path -LiteralPath $path -PathType Leaf) $path
}

# ---- 2. 游戏本体身份 ----
$ets2Hash = if (Test-Path -LiteralPath $Ets2Path -PathType Leaf) {
    (Get-FileHash -LiteralPath $Ets2Path -Algorithm SHA256).Hash.ToLowerInvariant()
} else { '' }
Add-Check 'ets2-executable-pinned' ($ets2Hash -eq $ExpectedEts2Hash) "sha256:$ets2Hash"

# ---- 3. 遥测插件 ----
# 插件不在位 = 整批没有 player_*/camera_*，而 depth 照样会采出来，
# 到转换那一步才发现。这条必须在开采前挡住。
$pluginPresent = Test-Path -LiteralPath $PluginPath -PathType Leaf
Add-Check 'scs-telemetry-plugin-installed' $pluginPresent $PluginPath

$outEnv = [Environment]::GetEnvironmentVariable('DM_ETS2_TELEMETRY_OUT', 'User')
Add-Check 'telemetry-output-path-configured' ([bool]$outEnv) "DM_ETS2_TELEMETRY_OUT=$outEnv"

# ---- 4. 相机相关的游戏设置 ----
# g_cam_physics/steering/blinker：驾驶舱抖动。开着的话 cabin/head offset 会持续非零，
# 而复合次序尚未标定（ets2_player_telemetry::OffsetComposition），转换会整段拒绝。
foreach ($name in @('g_cam_physics', 'g_cam_steering', 'g_cam_blinker')) {
    $value = Get-UsetValue $Ets2ConfigCfgPath $name
    Add-Check "camera-shake-disabled:$name" ($value -eq '0') "$name=$(if ($null -eq $value) { '<缺这一行>' } else { $value })"
}

# 分辨率与全屏：68 号 §7.1 要求系统与游戏都是 1920×1080 且全屏。
#
# 取不到时 detail 会显示 `<缺这一行>` 而不是空——这三个 cvar 的名字是按 ETS2
# config.cfg 的通行写法写的，**首次上机请核对一次**：名字错了的表现是三格全空，
# 与「设置没改」看起来完全一样，但处置方式完全不同。
$width = Get-UsetValue $Ets2ConfigCfgPath 'r_mode_width'
$height = Get-UsetValue $Ets2ConfigCfgPath 'r_mode_height'
$fullscreen = Get-UsetValue $Ets2ConfigCfgPath 'r_fullscreen'
function Show-Uset { param($Value) if ($null -eq $Value) { '<缺这一行>' } else { $Value } }
Add-Check 'ets2-1920x1080-fullscreen' (($width -eq '1920') -and ($height -eq '1080') -and ($fullscreen -eq '1')) `
    "r_mode_width=$(Show-Uset $width); r_mode_height=$(Show-Uset $height); r_fullscreen=$(Show-Uset $fullscreen)"

# 控制台：标定与现场取证都要用它（g_cam_* 的当场核对、FOV override）。
$console = Get-UsetValue $Ets2ConfigCfgPath 'g_console'
$developer = Get-UsetValue $Ets2ConfigCfgPath 'g_developer'
Add-Check 'developer-console-enabled' (($console -eq '1') -and ($developer -eq '1')) `
    "g_console=$console; g_developer=$developer"

$display = Get-CimInstance Win32_VideoController |
    Where-Object { $null -ne $_.CurrentHorizontalResolution } |
    Select-Object -First 1
$displayOk = $null -ne $display -and
    $display.CurrentHorizontalResolution -ge 1920 -and
    $display.CurrentVerticalResolution -ge 1080
Add-Check 'system-display-supports-1920x1080' $displayOk `
    $(if ($null -eq $display) { 'no active display reported' } else { "$($display.CurrentHorizontalResolution)x$($display.CurrentVerticalResolution)" })

# ---- 5. 位姿转换配置（相机基线 + 墙钟锚点）----
# 这一条不是「文件在不在」，是「里面的数是不是真的填过」。
# 模板里的占位值必须被显式拒绝，否则一份没填的配置能一路跑到交付。
$poseConfigOk = $false
$poseDetail = 'not supplied'
# StrictMode 下引用未赋值的变量会抛异常，而下面的频率检查在配置缺失时仍会读到它。
# 先给一个显式的 $null，比依赖 `-and` 的短路行为可靠。
$poseConfig = $null
if (Test-Path -LiteralPath $PoseConfigPath -PathType Leaf) {
    $poseDetail = $PoseConfigPath
    $poseConfig = Get-Content -LiteralPath $PoseConfigPath -Raw | ConvertFrom-Json
    $baseline = $poseConfig.baseline
    $placeholders = @(
        [string]$baseline.baseline_id
        [string]$baseline.truck_id
        [string]$baseline.evidence_path
    ) | Where-Object { $_ -match 'FILL_ME|TODO|<' }
    $poseConfigOk = $poseConfig.schema -eq 'ets2-pose-config/v1' -and
        $placeholders.Count -eq 0 -and
        [string]::IsNullOrWhiteSpace([string]$baseline.evidence_path) -eq $false -and
        (Test-Path -LiteralPath (Join-Path $dataMinerRoot $baseline.evidence_path)) -and
        [double]$baseline.horizontal_fov_degrees -gt 1 -and
        [double]$baseline.horizontal_fov_degrees -lt 180 -and
        [uint32]$poseConfig.intrinsics_width -eq 1920 -and
        [uint32]$poseConfig.intrinsics_height -eq 1080 -and
        [int64]$poseConfig.qpc_frequency_hz -gt 0 -and
        [int64]$poseConfig.wall_clock_anchor.qpc -gt 0 -and
        [int64]$poseConfig.wall_clock_anchor.unix_millis -gt 0
}
Add-Check 'pose-config-filled-with-evidence' $poseConfigOk $poseDetail

# QPC 频率必须与本机一致——配错了对齐结果整体偏移，且看不出来。
$localFrequency = [Diagnostics.Stopwatch]::Frequency
$configuredFrequency = if ($null -eq $poseConfig) { 0 } else { [int64]$poseConfig.qpc_frequency_hz }
Add-Check 'pose-config-qpc-frequency-matches-host' ($configuredFrequency -eq $localFrequency) `
    "host=$localFrequency; config=$configuredFrequency"

# ---- 6. 性能认证 ----
$perfOk = $false
if ($PerfCertificationManifestPath -and $ProbeRuntimeManifestPath -and
    $PerfCertificationKeyPath -and $PerfHardwarePath -and
    (Test-Path -LiteralPath $PerfCertificationManifestPath -PathType Leaf) -and
    (Test-Path -LiteralPath $ProbeRuntimeManifestPath -PathType Leaf) -and
    (Test-Path -LiteralPath $PerfCertificationKeyPath -PathType Leaf) -and
    (Test-Path -LiteralPath $PerfHardwarePath -PathType Leaf)) {
    $depthLabPath = Join-Path $ArtifactRoot 'depth-lab.exe'
    $probeDllPath = Join-Path $ArtifactRoot 'dm_depth_probe.dll'
    if ((Test-Path -LiteralPath $depthLabPath -PathType Leaf) -and
        (Test-Path -LiteralPath $probeDllPath -PathType Leaf)) {
        $probeDllSha = 'sha256:' + (Get-FileHash -LiteralPath $probeDllPath -Algorithm SHA256).Hash.ToLowerInvariant()
        & $depthLabPath perf-certification-check `
            --manifest $PerfCertificationManifestPath `
            --runtime $ProbeRuntimeManifestPath `
            --dll-sha256 $probeDllSha `
            --key $PerfCertificationKeyPath `
            --hardware $PerfHardwarePath | Out-Null
        $perfOk = $LASTEXITCODE -eq 0
    }
}
$perfDetail = "manifest=$PerfCertificationManifestPath; runtime=$ProbeRuntimeManifestPath; key=$PerfCertificationKeyPath; hardware=$PerfHardwarePath"
Add-Check 'matching-performance-certification' $perfOk $perfDetail

# ---- 7. 磁盘 ----
# 79 号：30fps 档约 80–140 GB/时。一条 3 分钟 clip 连同 RGB 与余量按 60 GB 起算。
$drive = Get-PSDrive -Name ((Get-Location).Drive.Name)
$freeGB = [math]::Round($drive.Free / 1GB, 1)
Add-Check 'free-disk-space' ($freeGB -ge $MinFreeGB) "free=${freeGB}GB; required=${MinFreeGB}GB"

# ---- 8. 采集闸仍关着 ----
$captureDisabled = $true
if (Test-Path -LiteralPath $probeConfigPath -PathType Leaf) {
    $probeConfig = Get-Content -LiteralPath $probeConfigPath -Raw | ConvertFrom-Json
    $captureDisabled = -not [bool]$probeConfig.enabled
}
Add-Check 'formal-capture-remains-disabled-until-green' $captureDisabled $probeConfigPath

$failed = @($checks | Where-Object { -not $_.passed })
$report = [ordered]@{
    schema = 'ets2-pr405-readiness/v1'
    checked_at = (Get-Date).ToString('o')
    ready_for_formal_capture = $failed.Count -eq 0
    checks = $checks
    blockers = @($failed | ForEach-Object { $_.name })
    # 脚本查不到的两条，留给采集员当场确认（见 README「开采前口头三问」）。
    operator_must_confirm = @(
        'game frame rate is locked to 30 (driver or in-game limiter, verified on the FPS overlay)'
        'this run stays in one camera view: no head-look, no camera switch, no photo mode'
    )
}
$report | ConvertTo-Json -Depth 6
if ($failed.Count -ne 0) { exit 2 }

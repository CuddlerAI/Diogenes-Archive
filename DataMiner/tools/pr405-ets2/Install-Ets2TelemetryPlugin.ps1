<#
.SYNOPSIS
  把 SCS 遥测插件装进 ETS2，并设好采集所需的环境变量。

.DESCRIPTION
  插件是**官方 Telemetry SDK**（MIT）的正常用法：游戏自带 `plugins/` 目录并主动
  加载它。这与 depth probe 的注入不是一回事——插件这一侧没有账号风险
  （79 号 §1.2/§1.3 说的是注入那一侧，仍然适用于 depth）。

  装完请按 crates/ets2-telemetry-plugin/README.md 的「首次上机检查单」逐条验。
  **本脚本只负责放文件和设变量，验不了插件在游戏里到底有没有加载成功。**
#>
[CmdletBinding()]
param(
    [string]$DllPath = '',
    [string]$Ets2BinDir = '',
    [string]$TelemetryOutPath = '',
    [ValidateSet('unvalidated', 'forward', 'local_z_backward')]
    [string]$RollConvention = 'unvalidated'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $DllPath) { $DllPath = Join-Path $dataMinerRoot 'target\release\ets2_telemetry_plugin.dll' }
if (-not $Ets2BinDir) {
    $Ets2BinDir = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Steam\steamapps\common\Euro Truck Simulator 2\bin\win_x64'
}
if (-not $TelemetryOutPath) {
    $TelemetryOutPath = Join-Path $env:LOCALAPPDATA 'DataMiner\ets2-telemetry\dm-ets2-telemetry.jsonl'
}

if (-not (Test-Path -LiteralPath $DllPath -PathType Leaf)) {
    throw "找不到插件 DLL：$DllPath（在 Windows 上跑 cargo build --release -p ets2-telemetry-plugin）"
}
if (-not (Test-Path -LiteralPath $Ets2BinDir -PathType Container)) {
    throw "找不到 ETS2 的 bin 目录：$Ets2BinDir"
}

# 现代 ETS2/ATS 都是 x64。若真装到 win_x86，导出符号名会被 __stdcall 修饰，
# 游戏按未修饰名查找会静默加载失败——那种失败在游戏里完全看不出来。
if ((Split-Path -Leaf $Ets2BinDir) -ne 'win_x64') {
    Write-Warning "目标不是 win_x64（实为 $(Split-Path -Leaf $Ets2BinDir)）：32 位构建的导出符号名尚未验证，见 crate README 的「已知未验证项」"
}

$pluginDir = Join-Path $Ets2BinDir 'plugins'
New-Item -ItemType Directory -Path $pluginDir -Force | Out-Null
$destination = Join-Path $pluginDir 'ets2_telemetry_plugin.dll'
Copy-Item -LiteralPath $DllPath -Destination $destination -Force

New-Item -ItemType Directory -Path (Split-Path -Parent $TelemetryOutPath) -Force | Out-Null
[Environment]::SetEnvironmentVariable('DM_ETS2_TELEMETRY_OUT', $TelemetryOutPath, 'User')
[Environment]::SetEnvironmentVariable('DM_ETS2_ROLL_CONVENTION', $RollConvention, 'User')

[ordered]@{
    schema = 'ets2-telemetry-plugin-install/v1'
    installed_at = (Get-Date).ToString('o')
    plugin = $destination
    plugin_sha256 = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    telemetry_out = $TelemetryOutPath
    roll_convention = $RollConvention
    next_steps = @(
        '环境变量是 User 级：ETS2 必须在设完之后重新启动（Steam 也要重启，它会把旧环境传给子进程）'
        '进游戏后看 game.log.txt 是否出现 `dm-ets2: 已就绪`；没有就是 DLL 位置或位数不对'
        '进主菜单十秒，该段 JSONL 应全是 skip_reason="paused"；若仍有 player 载荷立刻停下上报'
        'roll 符号标定前保持 unvalidated（见 crates/ets2-player-telemetry/README.md）'
    )
} | ConvertTo-Json -Depth 5

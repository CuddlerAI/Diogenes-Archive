<#
.SYNOPSIS
  从解包出的扇区文件列出 68 号 §6.1 的地图四角边界。

.DESCRIPTION
  两步：
    1. 用官方 `scs_extractor.exe` 解 `base_map.scs`（本脚本不代做——它需要游戏本体，
       且解包产物几个 GB，不该混进采集流程）；
    2. 把 `map/europe/` 下的 `sec*.base` 文件名喂给 depth-lab，算出四角。

  边界随交付带出时**必须连同 `confidence` 与扇区集合说明**（82 号 §5 的纪律）：
  base 与 base+DLC 的扇区集合不同，边界跟着变，只给数字的话对方无从判断它覆盖了什么。

.EXAMPLE
  # 先解包（一次就够，除非游戏更新）
  & "<Steam>\steamapps\common\Euro Truck Simulator 2\bin\win_x64\scs_extractor.exe" `
      "<Steam>\steamapps\common\Euro Truck Simulator 2\base_map.scs" C:\ets2-extract

  .\Export-Ets2MapBounds.ps1 -SectorDir C:\ets2-extract\map\europe -Out .\evidence\ets2-map-bounds.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SectorDir,
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$DepthLabExe = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $DepthLabExe) { $DepthLabExe = Join-Path $dataMinerRoot 'target\release\depth-lab.exe' }

$names = @(Get-ChildItem -LiteralPath $SectorDir -Filter 'sec*.base' -File | Select-Object -ExpandProperty Name)
if ($names.Count -eq 0) {
    throw "$SectorDir 下没有 sec*.base——解包目录不对，或这一版把扇区放在了别处"
}

$listing = [IO.Path]::GetTempFileName()
try {
    [IO.File]::WriteAllLines($listing, $names)
    & $DepthLabExe ets2-map-bounds --sectors $listing --out $Out
    if ($LASTEXITCODE -ne 0) { throw "depth-lab ets2-map-bounds 失败（exit $LASTEXITCODE）" }
} finally {
    Remove-Item -LiteralPath $listing -Force -ErrorAction SilentlyContinue
}

Write-Host "扇区 $($names.Count) 个 → $Out"
Write-Host "把这份 json 的四角与 metric_scale 填进元信息 excel 的「地图边界」栏。"

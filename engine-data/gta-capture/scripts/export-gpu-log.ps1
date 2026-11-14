# Logs the GPU once a second while lanes export, for tools/export_timing.py --gpu-log (2026-10-13: an RTX
# 4060 8 GB "stuttered" exporting the 4K-supersampled profile; this says whether it ran out of memory, ran
# hot, or was held down). Read only: it starts nvidia-smi and nothing else. Stop it with Ctrl+C.
#
#   powershell -ExecutionPolicy Bypass -File scripts\export-gpu-log.ps1 -Out C:\workspace\mc4\tmp\gpu-<date>.csv
param(
    [Parameter(Mandatory = $true)][string]$Out,
    [int]$IntervalS = 1
)
$ErrorActionPreference = 'Stop'
$smi = Get-Command nvidia-smi -ErrorAction SilentlyContinue
if (-not $smi) { $smi = Get-Item 'C:\Windows\System32\nvidia-smi.exe' -ErrorAction SilentlyContinue }
if (-not $smi) { throw 'nvidia-smi not found (NVIDIA driver)' }
$fields = 'timestamp,utilization.gpu,memory.used,memory.total,temperature.gpu,clocks.current.graphics,power.draw,clocks_throttle_reasons.active'
Write-Host "logging $fields every $IntervalS s to $Out (Ctrl+C to stop)"
& $smi.Source "--query-gpu=$fields" --format=csv -l $IntervalS -f $Out

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Int64]$WindowHandle,
    [ValidateRange(1, 600)]
    [int]$DurationSeconds = 300,
    [Parameter(Mandatory = $true)]
    [string]$LogPath,
    [string]$StartSignalPath = ''
)

$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Pr405TrajectoryInput {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
}
'@

$keyUp = 0x0002
$mouseMove = 0x0001
$keys = @{
    W = [byte]0x57
    A = [byte]0x41
    S = [byte]0x53
    D = [byte]0x44
}
$down = [System.Collections.Generic.HashSet[string]]::new()
$handle = [IntPtr]$WindowHandle
$logDir = Split-Path -Parent $LogPath
if ($logDir) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }
$signalDeadline = (Get-Date).AddMinutes(3)
while ($StartSignalPath -and -not (Test-Path -LiteralPath $StartSignalPath)) {
    if ((Get-Date) -ge $signalDeadline) { throw "trajectory start signal timed out: $StartSignalPath" }
    Start-Sleep -Milliseconds 100
}
$writer = [IO.StreamWriter]::new($LogPath, $false, [Text.UTF8Encoding]::new($false))
$writer.AutoFlush = $true
$clock = [Diagnostics.Stopwatch]::StartNew()
$nextMouseMs = 0L
$nextFocusMs = 0L

function Write-Event([string]$Type, [hashtable]$Fields) {
    $row = [ordered]@{
        elapsed_ms = [long]$clock.ElapsedMilliseconds
        qpc = [Diagnostics.Stopwatch]::GetTimestamp()
        type = $Type
    }
    foreach ($entry in $Fields.GetEnumerator()) { $row[$entry.Key] = $entry.Value }
    $writer.WriteLine(($row | ConvertTo-Json -Compress))
}

function Set-Movement([string[]]$Desired) {
    $desiredSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$Desired)
    foreach ($name in @($down)) {
        if (-not $desiredSet.Contains($name)) {
            [Pr405TrajectoryInput]::keybd_event($keys[$name], 0, $keyUp, [UIntPtr]::Zero)
            [void]$down.Remove($name)
            Write-Event 'key_up' @{ key = $name }
        }
    }
    foreach ($name in $Desired) {
        if ($down.Add($name)) {
            [Pr405TrajectoryInput]::keybd_event($keys[$name], 0, 0, [UIntPtr]::Zero)
            Write-Event 'key_down' @{ key = $name }
        }
    }
}

try {
    [Pr405TrajectoryInput]::ShowWindow($handle, 9) | Out-Null
    [Pr405TrajectoryInput]::SetForegroundWindow($handle) | Out-Null
    Write-Event 'start' @{ duration_seconds = $DurationSeconds; window_handle = $WindowHandle }
    while ($clock.Elapsed.TotalSeconds -lt $DurationSeconds) {
        $phase = [int]([Math]::Floor($clock.Elapsed.TotalSeconds) % 24)
        if ($phase -lt 6) {
            Set-Movement @('W')
            $mouseDx = 1
        } elseif ($phase -lt 10) {
            Set-Movement @('D')
            $mouseDx = 2
        } elseif ($phase -lt 16) {
            Set-Movement @('S')
            $mouseDx = -1
        } elseif ($phase -lt 20) {
            Set-Movement @('A')
            $mouseDx = -2
        } else {
            Set-Movement @('W')
            $mouseDx = 1
        }

        if ($clock.ElapsedMilliseconds -ge $nextMouseMs) {
            $mouseDy = if (($phase % 8) -eq 0) { 1 } elseif (($phase % 8) -eq 4) { -1 } else { 0 }
            [Pr405TrajectoryInput]::mouse_event($mouseMove, $mouseDx, $mouseDy, 0, [UIntPtr]::Zero)
            Write-Event 'mouse_move' @{ dx = $mouseDx; dy = $mouseDy }
            $nextMouseMs = $clock.ElapsedMilliseconds + 200
        }
        if ($clock.ElapsedMilliseconds -ge $nextFocusMs) {
            [Pr405TrajectoryInput]::SetForegroundWindow($handle) | Out-Null
            $nextFocusMs = $clock.ElapsedMilliseconds + 5000
        }
        Start-Sleep -Milliseconds 25
    }
} finally {
    Set-Movement @()
    Write-Event 'stop' @{}
    $writer.Dispose()
}

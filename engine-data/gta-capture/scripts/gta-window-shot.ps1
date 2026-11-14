# A picture of the GTA V window only -- never the rest of the desktop -- for an operator working over SSH: the
# window's rectangle as it is on the screen (the game runs borderless, so the screen holds its frame). Taken only
# while GTA is the window in front, so no other window lies over it; else nothing is saved and the note says why.
# Runs as the scheduled task "Diogenes-mc4 GtaShot" (an SSH session has no screen to copy):
#   Start-ScheduledTask -TaskName "Diogenes-mc4 GtaShot"   ->  D:\workspace\mc4\screens\gta-latest.png (+ .txt)
# Each picture overwrites the one before.
$ErrorActionPreference = "Stop"
$out = "D:\workspace\mc4\screens\gta-latest.png"
$note = [IO.Path]::ChangeExtension($out, ".txt")
New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class GtaWin {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
# the window's rectangle and the screen copy both in real pixels: on a scaled display (win-pc: 150%) a process that
# is not DPI aware gets the rectangle scaled down and copies the wrong part of the screen
[GtaWin]::SetProcessDPIAware() | Out-Null
$gta = Get-Process -Name GTA5 -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
if (-not $gta) { "$(Get-Date -Format o) no GTA5 window: nothing saved" | Set-Content -Encoding UTF8 $note; exit 1 }
$hwnd = $gta.MainWindowHandle
if ([GtaWin]::IsIconic($hwnd)) { "$(Get-Date -Format o) GTA5 is minimized: nothing saved" | Set-Content -Encoding UTF8 $note; exit 2 }
if ([GtaWin]::GetForegroundWindow() -ne $hwnd) {
    "$(Get-Date -Format o) GTA5 is not the window in front: nothing saved" | Set-Content -Encoding UTF8 $note
    exit 3
}
$r = New-Object GtaWin+RECT
[GtaWin]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
try { $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size) } finally { $g.Dispose() }
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"$(Get-Date -Format o) saved ${w}x${h} at $($r.Left),$($r.Top)" | Set-Content -Encoding UTF8 $note

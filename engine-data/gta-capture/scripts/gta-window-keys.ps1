# Presses keys in the GTA V window for an operator working over SSH: the start page (before Story Mode, where the
# take program does not run yet), the game's own prompts and menus. Keys go in only while GTA is the window in
# front -- else nothing is pressed and the note says why -- the way the take program presses them (SendInput with
# the virtual key and its scan code). Runs as the scheduled task "Diogenes-mc4 GtaKeys":
#   write D:\workspace\mc4\screens\keys.txt, one per line:  UP DOWN LEFT RIGHT ENTER ESC BACK SPACE TAB Q E
#   (or WAIT <ms>, MOUSE <dx> <dy>, CENTER), then  Start-ScheduledTask -TaskName "Diogenes-mc4 GtaKeys"
# MOUSE moves the pointer a few pixels (at most 50 each way), CENTER puts it in the middle of the GTA window and
# moves it 2 pixels there; neither clicks anything. They exist because the Editor's frontend runs its
# pointer update -- where RE+ publishes the Export page's state and starts the frame pass -- only once the mouse
# has moved in a menu, so an Editor driven by keys alone never authorizes an armed export.
# The file is emptied once its keys are pressed; the note lists what was pressed.
$ErrorActionPreference = "Stop"
$file = "D:\workspace\mc4\screens\keys.txt"
$note = "D:\workspace\mc4\screens\keys-note.txt"
New-Item -ItemType Directory -Force (Split-Path $file) | Out-Null
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class GtaKeys {
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public MOUSEINPUT mi; }
    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION u; }
    [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint code, uint mapType);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    public static string Where() { POINT p; GetCursorPos(out p); return p.X + "," + p.Y; }
    public static void Key(ushort vk, bool down) {
        INPUT[] i = new INPUT[1];
        i[0].type = 1;   // INPUT_KEYBOARD
        i[0].u.ki.wVk = vk;
        i[0].u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
        uint ext = (vk >= 0x21 && vk <= 0x28) ? 1u : 0u;   // the arrows and page keys are extended keys
        i[0].u.ki.dwFlags = (down ? 0u : 2u) | ext;
        SendInput(1, i, Marshal.SizeOf(typeof(INPUT)));
    }
    public static void Move(int dx, int dy) {
        INPUT[] i = new INPUT[1];
        i[0].type = 0;   // INPUT_MOUSE
        i[0].u.mi.dx = dx;
        i[0].u.mi.dy = dy;
        i[0].u.mi.dwFlags = 1u;   // MOUSEEVENTF_MOVE, relative: no button
        SendInput(1, i, Marshal.SizeOf(typeof(INPUT)));
    }
}
"@
[GtaKeys]::SetProcessDPIAware() | Out-Null   # window and pointer in real pixels on a scaled display (win-pc: 150%)
$vk = @{ UP = 0x26; DOWN = 0x28; LEFT = 0x25; RIGHT = 0x27; ENTER = 0x0D; ESC = 0x1B; BACK = 0x08; SPACE = 0x20; TAB = 0x09; Q = 0x51; E = 0x45; V = 0x56 }   # Q / E: the pause menu's tabs
$lines = @(Get-Content -ErrorAction SilentlyContinue $file | Where-Object { $_.Trim() })
if ($lines.Count -eq 0) { "$(Get-Date -Format o) no keys to press" | Set-Content -Encoding UTF8 $note; exit 0 }
$gta = Get-Process -Name GTA5 -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
if (-not $gta -or [GtaKeys]::GetForegroundWindow() -ne $gta.MainWindowHandle) {
    "$(Get-Date -Format o) GTA5 is not the window in front: nothing pressed" | Set-Content -Encoding UTF8 $note
    exit 3
}
$done = @()
foreach ($line in $lines) {
    $w = $line.Trim().ToUpper() -split '\s+'
    if ($w[0] -eq "WAIT") { Start-Sleep -Milliseconds ([int]$w[1]); $done += $line.Trim(); continue }
    if ($w[0] -notin @("MOUSE", "CENTER") -and -not $vk.ContainsKey($w[0])) { $done += "($($w[0]): not a key here, skipped)"; continue }
    if ([GtaKeys]::GetForegroundWindow() -ne $gta.MainWindowHandle) { $done += "(GTA5 left the front: stopped)"; break }
    if ($w[0] -eq "MOUSE") {
        $dx = [Math]::Max(-50, [Math]::Min(50, [int]$w[1])); $dy = [Math]::Max(-50, [Math]::Min(50, [int]$w[2]))
        $from = [GtaKeys]::Where()
        [GtaKeys]::Move($dx, $dy); Start-Sleep -Milliseconds 250
        $done += "MOUSE $dx $dy ($from -> $([GtaKeys]::Where()))"
        continue
    }
    if ($w[0] -eq "CENTER") {
        # the pointer to the middle of the GTA window, then a 2-pixel move there so the game sees mouse input
        $r = New-Object GtaKeys+RECT
        [GtaKeys]::GetWindowRect($gta.MainWindowHandle, [ref]$r) | Out-Null
        $from = [GtaKeys]::Where()
        [GtaKeys]::SetCursorPos([int](($r.Left + $r.Right) / 2), [int](($r.Top + $r.Bottom) / 2)) | Out-Null
        Start-Sleep -Milliseconds 100
        [GtaKeys]::Move(2, 0); Start-Sleep -Milliseconds 100; [GtaKeys]::Move(-2, 0); Start-Sleep -Milliseconds 250
        $done += "CENTER (window $($r.Left),$($r.Top)-$($r.Right),$($r.Bottom); $from -> $([GtaKeys]::Where()))"
        continue
    }
    [GtaKeys]::Key([uint16]$vk[$w[0]], $true); Start-Sleep -Milliseconds 80
    [GtaKeys]::Key([uint16]$vk[$w[0]], $false); Start-Sleep -Milliseconds 250
    $done += $w[0]
}
Set-Content -Encoding UTF8 $file ""
"$(Get-Date -Format o) pressed: $($done -join ' ')" | Set-Content -Encoding UTF8 $note

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Int64]$WindowHandle,
    [Parameter(Mandatory = $true)]
    [int]$ScreenX,
    [Parameter(Mandatory = $true)]
    [int]$ScreenY
)

$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Pr405ElevatedInput {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
'@

$handle = [IntPtr]$WindowHandle
[Pr405ElevatedInput]::ShowWindow($handle, 9) | Out-Null
[Pr405ElevatedInput]::SetForegroundWindow($handle) | Out-Null
Start-Sleep -Milliseconds 400
[Pr405ElevatedInput]::SetCursorPos($ScreenX, $ScreenY) | Out-Null
Start-Sleep -Milliseconds 150
[Pr405ElevatedInput]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 100
[Pr405ElevatedInput]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)

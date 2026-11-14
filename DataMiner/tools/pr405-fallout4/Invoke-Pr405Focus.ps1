[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Int64]$WindowHandle
)

$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Pr405ElevatedFocus {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
}
'@

$handle = [IntPtr]$WindowHandle
[Pr405ElevatedFocus]::ShowWindow($handle, 9) | Out-Null
[Pr405ElevatedFocus]::SetForegroundWindow($handle) | Out-Null

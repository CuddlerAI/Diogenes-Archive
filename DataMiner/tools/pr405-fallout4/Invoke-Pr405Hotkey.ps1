[CmdletBinding()]
param(
    [ValidateSet('PauseResume', 'Marker', 'Stop', 'Calibration')]
    [string]$Action = 'Stop'
)

$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Pr405ElevatedHotkey {
    [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
}
'@

$functionKey = switch ($Action) {
    'PauseResume' { 0x77 } # F8
    'Marker'      { 0x78 } # F9
    'Stop'        { 0x79 } # F10
    'Calibration' { 0x7A } # F11
}

$keyUp = 0x0002
[Pr405ElevatedHotkey]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero) # Ctrl down
[Pr405ElevatedHotkey]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero) # Alt down
[Pr405ElevatedHotkey]::keybd_event([byte]$functionKey, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 100
[Pr405ElevatedHotkey]::keybd_event([byte]$functionKey, 0, $keyUp, [UIntPtr]::Zero)
[Pr405ElevatedHotkey]::keybd_event(0x12, 0, $keyUp, [UIntPtr]::Zero)
[Pr405ElevatedHotkey]::keybd_event(0x11, 0, $keyUp, [UIntPtr]::Zero)

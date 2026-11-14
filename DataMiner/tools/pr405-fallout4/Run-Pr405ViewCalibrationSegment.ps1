[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [Int64]$WindowHandle,
    [ValidateSet('cw', 'ccw')]
    [string]$Direction,
    [ValidateSet('slow', 'normal', 'fast')]
    [string]$Speed
)

$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;

public static class Pr405ViewCalibrationInput {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);

    public static void Rotate(IntPtr hWnd, int events, int dx, int intervalMicros) {
        ShowWindow(hWnd, 9);
        SetForegroundWindow(hWnd);
        var clock = Stopwatch.StartNew();
        long nextTicks = 0;
        long intervalTicks = Math.Max(1, Stopwatch.Frequency * intervalMicros / 1000000);
        for (int i = 0; i < events; i++) {
            mouse_event(0x0001, dx, 0, 0, UIntPtr.Zero);
            nextTicks += intervalTicks;
            while (clock.ElapsedTicks < nextTicks) { }
        }
    }
}
'@

$plan = switch ($Speed) {
    'slow'   { @{ Events = 6000; Delta = 9;  IntervalMicros = 1000 } }
    'normal' { @{ Events = 3000; Delta = 18; IntervalMicros = 1000 } }
    'fast'   { @{ Events = 1000; Delta = 54; IntervalMicros = 1000 } }
}
$signedDelta = if ($Direction -eq 'cw') { $plan.Delta } else { -$plan.Delta }
[Pr405ViewCalibrationInput]::Rotate(
    [IntPtr]$WindowHandle,
    $plan.Events,
    $signedDelta,
    $plan.IntervalMicros
)

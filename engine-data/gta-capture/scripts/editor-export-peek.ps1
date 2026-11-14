# Where an armed Rockstar Editor export stands, read from outside the game: the export plan the controller
# published, the editor identity RE+ publishes, and the capture add-on's heartbeat -- the three shared-memory blocks
# the export's authorization runs on -- twice, 0.7 s apart, so a stalled publisher shows. Read only; nothing in the
# game changes. Inside the Editor the controller's pipe commands wait on a stopped ScriptHook fiber, so this is
# the one way to see why an export is not authorized yet.
#   powershell -ExecutionPolicy Bypass -File scripts\editor-export-peek.ps1      (an SSH session of the same user works)
# The blocks are named in the GTA session's Local namespace, opened here as Session\<id>\<name>.
#   plan state      1 armed, 2 triggered (authorized), 3 exporting, 4 done, 5 failed
#   identity        menu = the Export page is open, startArmed = its Start row is focused with a valid config,
#                   seq / hb advance only while RE+ publishes (the Editor's pointer update, or playback)
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Peek {
    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    public static extern IntPtr OpenFileMappingW(uint access, bool inherit, string name);
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern IntPtr MapViewOfFile(IntPtr map, uint access, uint hi, uint lo, UIntPtr bytes);
    [DllImport("kernel32.dll")] public static extern bool UnmapViewOfFile(IntPtr view);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
    public static byte[] Read(string name, int size, out int err) {
        err = 0;
        IntPtr m = OpenFileMappingW(4, false, name);   // FILE_MAP_READ
        if (m == IntPtr.Zero) { err = Marshal.GetLastWin32Error(); return null; }
        IntPtr v = MapViewOfFile(m, 4, 0, 0, (UIntPtr)size);
        if (v == IntPtr.Zero) { err = Marshal.GetLastWin32Error(); CloseHandle(m); return null; }
        byte[] b = new byte[size];
        Marshal.Copy(v, b, 0, size);
        UnmapViewOfFile(v); CloseHandle(m);
        return b;
    }
}
"@
$gta = Get-Process -Name GTA5 -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $gta) { "no GTA5 process"; exit 1 }
$sid = $gta.SessionId
"gta pid=$($gta.Id) session=$sid"
function U32($b, $o) { [BitConverter]::ToUInt32($b, $o) }
# offsets: DiogenesExportPlanHeader / DiogenesEditorIdentityBlock / FxCaptureBlock (patches/replus-v19-sliding.patch)
function Snap {
    $e = 0
    $r = [ordered]@{}
    $p = [Peek]::Read("Session\$sid\DiogenesGtaExportPlan", 672, [ref]$e)
    if ($p) {
        $r.plan = "magic=0x{0:X8} ver={1} seq={2} state={3} pid={4} lane={5} frames={6} clips={7}" -f (U32 $p 0), (U32 $p 4), (U32 $p 8), (U32 $p 20), (U32 $p 24), (U32 $p 28), (U32 $p 32), (U32 $p 640)
    } else { $r.plan = "not open (error $e): nothing armed in this GTA process" }
    $i = [Peek]::Read("Session\$sid\DiogenesGtaEditorIdentity", 960, [ref]$e)
    if ($i) {
        $name = [Text.Encoding]::UTF8.GetString($i, 32, 256).Split([char]0)[0]
        $r.identity = "ver={0} seq={1} hb={2} pid={3} editor={4} menu={5} startArmed={6} configValid={7} render={8} clips={9} clipIdx={10} clipId={11:x16} project='{12}'" -f (U32 $i 4), (U32 $i 8), (U32 $i 292), (U32 $i 288), (U32 $i 296), (U32 $i 300), (U32 $i 304), (U32 $i 308), (U32 $i 312), (U32 $i 16), ([BitConverter]::ToInt32($i, 20)), ([BitConverter]::ToUInt64($i, 24)), $name
    } else { $r.identity = "not open (error $e)" }
    $c = [Peek]::Read("Session\$sid\SimpleCameraFxCapture", 688, [ref]$e)
    if ($c) {
        $r.capture = "ver={0} req={1} ack={2} status={3} addonHb={4}" -f (U32 $c 4), (U32 $c 8), (U32 $c 12), (U32 $c 16), (U32 $c 44)
    } else { $r.capture = "not open (error $e)" }
    $r
}
"--- now"; (Snap).GetEnumerator() | ForEach-Object { "$($_.Key): $($_.Value)" }
Start-Sleep -Milliseconds 700
"--- 0.7 s later"; (Snap).GetEnumerator() | ForEach-Object { "$($_.Key): $($_.Value)" }

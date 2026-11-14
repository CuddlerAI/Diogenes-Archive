# The scheduled tasks an operator working over SSH uses to start GTA V, see its window and press keys in it
# (gta-desktop-start.ps1, gta-window-shot.ps1, gta-window-keys.ps1): on demand only (no trigger), as the logged-on user, in that user's
# desktop (interactive logon: they run only while the user is logged on), with no window (hidden-run.vbs).
#   scripts/gta-desktop-tasks.ps1 -Install    register them all (again: replaces them)
#   scripts/gta-desktop-tasks.ps1 -Remove     unregister them all
param([switch]$Install, [switch]$Remove)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$tasks = [ordered]@{ "Diogenes-mc4 StartGTA" = "gta-desktop-start.ps1"; "Diogenes-mc4 GtaShot" = "gta-window-shot.ps1";
                     "Diogenes-mc4 GtaKeys" = "gta-window-keys.ps1" }
if ($Remove) {
    foreach ($name in $tasks.Keys) {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
        "removed $name"
    }
    exit 0
}
if (-not $Install) { throw "give -Install or -Remove" }
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name   # as Windows resolves it (an SSH session may lack USERDOMAIN)
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 2) -MultipleInstances IgnoreNew
$launcher = Join-Path $here "hidden-run.vbs"
foreach ($name in $tasks.Keys) {
    $script = Join-Path $here $tasks[$name]
    $action = New-ScheduledTaskAction -Execute "wscript.exe" -Argument "`"$launcher`" `"$script`""
    Register-ScheduledTask -TaskName $name -Action $action -Principal $principal -Settings $settings -Force | Out-Null
    "registered $name -> $script (as $user, interactive, on demand)"
}

# Starts GTA V (Steam, Legacy: app 271590) on the logged-on desktop, for an operator working over SSH: a program
# started from an SSH session runs off the desktop, so this runs as the scheduled task "Diogenes-mc4 StartGTA"
# (interactive logon: only while the user is logged on, in that user's desktop). Nothing happens when GTA already runs.
#   install (once, as the desktop user):  scripts/gta-desktop-tasks.ps1 -Install
#   run:                                  Start-ScheduledTask -TaskName "Diogenes-mc4 StartGTA"
$ErrorActionPreference = "Stop"
$log = "D:\workspace\mc4\screens\start-gta.txt"
New-Item -ItemType Directory -Force (Split-Path $log) | Out-Null
if (Get-Process -Name GTA5 -ErrorAction SilentlyContinue) {
    "$(Get-Date -Format o) GTA5 already runs: nothing started" | Set-Content -Encoding UTF8 $log
    exit 0
}
Start-Process "steam://rungameid/271590"
"$(Get-Date -Format o) asked Steam to start app 271590" | Set-Content -Encoding UTF8 $log

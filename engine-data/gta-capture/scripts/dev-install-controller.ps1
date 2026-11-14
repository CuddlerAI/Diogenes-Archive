# Puts a freshly built DiogenesGtaController.asi into the game folder for day-to-day iteration, backing up
# the one it replaces (copy the backup back to roll back). The game loads .asi files only at start-up:
# it must be closed now and started again afterwards. A formal deployment of the whole stack, with
# receipts, is scripts/deploy-storymode-stack.ps1.
#
#   powershell -ExecutionPolicy Bypass -File scripts\dev-install-controller.ps1 `
#     -GameDir "D:\SteamLibrary\steamapps\common\Grand Theft Auto V" -Built D:\workspace\mc4\build-native\Release\DiogenesGtaController.asi `
#     -BackupRoot D:\workspace\mc4
param(
    [Parameter(Mandatory = $true)][string]$GameDir,
    [Parameter(Mandatory = $true)][string]$Built,
    [Parameter(Mandatory = $true)][string]$BackupRoot     # a game-backup-<timestamp> folder is made under it
)
$ErrorActionPreference = "Stop"
if (Get-Process GTA5 -ErrorAction SilentlyContinue) { throw "GTA is running: close it first (the .asi is loaded at start-up)" }
$target = Join-Path $GameDir "DiogenesGtaController.asi"
if (-not (Test-Path -LiteralPath $Built)) { throw "built file not found: $Built" }
$backup = Join-Path $BackupRoot ("game-backup-" + (Get-Date -Format yyyyMMdd-HHmmss))
New-Item -ItemType Directory -Force $backup | Out-Null
if (Test-Path -LiteralPath $target) {
    Copy-Item -LiteralPath $target (Join-Path $backup "DiogenesGtaController.asi")
    "backed up the installed one ({0}) to {1}" -f (Get-FileHash $target).Hash.Substring(0, 16), $backup
}
Copy-Item -Force -LiteralPath $Built $target
if ((Get-FileHash $Built).Hash -ne (Get-FileHash $target).Hash) { throw "the copy differs from the built file" }
"installed DiogenesGtaController.asi  {0} bytes  {1}; start the game now" -f (Get-Item $target).Length, (Get-FileHash $target).Hash.Substring(0, 16)

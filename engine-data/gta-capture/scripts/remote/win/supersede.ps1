# supersede.ps1 -Delivery <dir> -Pattern <name pattern> -Tag <.superseded-why> [-QcDir <dir>]: set a delivered package
# aside before it is delivered again -- the folder and its .manifest.json, .README.md, .zip and .sha256.txt beside it
# (and its QC folder) get the tag appended; nothing is deleted. The pattern finds the folder by wildcard, e.g.
# 2026_10_18_*_113 (a name with Chinese in it does not survive the ssh command line). ASCII only: Windows
# PowerShell 5 reads a script without a BOM as the ANSI code page.
param([Parameter(Mandatory)][string]$Delivery, [Parameter(Mandatory)][string]$Pattern,
      [Parameter(Mandatory)][string]$Tag, [string]$QcDir)
$ErrorActionPreference = 'Stop'
$pkg = @(Get-Item (Join-Path $Delivery $Pattern) | Where-Object { $_.PSIsContainer })
if ($pkg.Count -ne 1) { throw "want exactly one package folder for $Pattern, found $($pkg.Count)" }
$base = $pkg[0].Name
foreach ($ext in @('', '.manifest.json', '.README.md', '.zip', '.sha256.txt')) {
    $p = Join-Path $Delivery ($base + $ext)
    if (Test-Path -LiteralPath $p) {
        if (Test-Path -LiteralPath (Join-Path $Delivery ($base + $Tag + $ext))) { throw "$base$Tag$ext exists already" }
        Rename-Item -LiteralPath $p -NewName ($base + $Tag + $ext); "renamed '$ext'"
    }
}
if ($QcDir -and (Test-Path -LiteralPath $QcDir)) { Rename-Item -LiteralPath $QcDir -NewName ((Split-Path $QcDir -Leaf) + $Tag); 'renamed the QC folder' }

# zip_package.ps1 -Delivery <dir> -Pattern <name pattern>: <package>.zip (the folder itself inside) and
# <package>.sha256.txt (one line per file: sha256, two spaces, <package>/<file>), then the zip's size and SHA-256.
param([Parameter(Mandatory)][string]$Delivery, [Parameter(Mandatory)][string]$Pattern)
$ErrorActionPreference = 'Stop'
$pkg = @(Get-Item (Join-Path $Delivery $Pattern) | Where-Object { $_.PSIsContainer })
if ($pkg.Count -ne 1) { throw "want exactly one package folder for $Pattern, found $($pkg.Count)" }
$name = $pkg[0].Name
$zip = Join-Path $Delivery "$name.zip"
$sum = Join-Path $Delivery "$name.sha256.txt"
if (Test-Path -LiteralPath $zip) { throw "$zip exists already" }
$lines = foreach ($f in Get-ChildItem -LiteralPath $pkg[0].FullName -File | Sort-Object Name) {
    (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash.ToLower() + '  ' + $name + '/' + $f.Name
}
[System.IO.File]::WriteAllLines($sum, $lines, (New-Object System.Text.UTF8Encoding($false)))
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($pkg[0].FullName, $zip, [System.IO.Compression.CompressionLevel]::Optimal, $true)
$a = [System.IO.Compression.ZipFile]::OpenRead($zip)
$entries = @($a.Entries | Where-Object { $_.Name -ne '' }).Count
$a.Dispose()
"entries $entries"
"zip bytes " + (Get-Item -LiteralPath $zip).Length
"zip sha256 " + (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower()
"sum lines " + (Get-Content -LiteralPath $sum).Count

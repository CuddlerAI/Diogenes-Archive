[CmdletBinding()]
param(
    [string]$GameRoot = 'C:\Program Files (x86)\Steam\steamapps\common\Mount & Blade II Bannerlord'
)

$ErrorActionPreference = 'Stop'
$toolRoot = $PSScriptRoot
$gameBin = Join-Path $GameRoot 'bin\Win64_Shipping_Client'
$moduleRoot = Join-Path $GameRoot 'Modules\DataMinerBannerlordTelemetry'
$moduleBin = Join-Path $moduleRoot 'bin\Win64_Shipping_Client'
$compiler = 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
$netstandard = 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\netstandard.dll'
$output = Join-Path $moduleBin 'DataMiner.BannerlordTelemetry.dll'

foreach ($required in @($gameBin, $compiler, $netstandard)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Required path is missing: $required"
    }
}

New-Item -ItemType Directory -Path $moduleBin -Force | Out-Null
$references = @(
    'TaleWorlds.Library.dll',
    'TaleWorlds.ObjectSystem.dll',
    'TaleWorlds.Core.dll',
    'TaleWorlds.Engine.dll',
    'TaleWorlds.MountAndBlade.dll',
    'TaleWorlds.CampaignSystem.dll'
) | ForEach-Object {
    $path = Join-Path $gameBin $_
    if (-not (Test-Path -LiteralPath $path)) { throw "Game reference is missing: $path" }
    '/reference:' + $path
}

$arguments = @(
    '/nologo',
    '/target:library',
    '/optimize+',
    ('/out:' + $output)
) + $references + @(
    ('/reference:' + $netstandard),
    (Join-Path $toolRoot 'DataMinerBannerlordTelemetry.cs')
)

& $compiler $arguments
if ($LASTEXITCODE -ne 0) { throw "Telemetry compilation failed with exit code $LASTEXITCODE" }
Copy-Item -LiteralPath (Join-Path $toolRoot 'SubModule.xml') -Destination $moduleRoot -Force

$hash = (Get-FileHash -LiteralPath $output -Algorithm SHA256).Hash.ToLowerInvariant()
[pscustomobject]@{
    ModuleRoot = $moduleRoot
    Dll = $output
    Sha256 = $hash
}

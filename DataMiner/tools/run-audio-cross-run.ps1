[CmdletBinding()]
param(
    [ValidateRange(5, 20)]
    [int]$Runs = 5,
    [ValidateRange(1, 30)]
    [int]$Segments = 6,
    [ValidateRange(2, 60)]
    [int]$Seconds = 10,
    [int]$RequiredBuild = 0,
    [string]$OutputRoot = "tools/capture-lab/out/audio-cross-run-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))",
    [string]$Python = "python"
)

$ErrorActionPreference = "Stop"
if ($Segments * $Seconds -gt 60) {
    throw "capture-lab process-loopback probes must be 60 seconds or shorter"
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$absoluteOutput = [IO.Path]::GetFullPath((Join-Path $repositoryRoot $OutputRoot))
if (Test-Path -LiteralPath $absoluteOutput) {
    $existing = @(Get-ChildItem -LiteralPath $absoluteOutput -Force)
    if ($existing.Count -ne 0) {
        throw "output directory must be empty: $absoluteOutput"
    }
} else {
    New-Item -ItemType Directory -Path $absoluteOutput | Out-Null
}

$reports = @()
Push-Location $repositoryRoot
try {
    for ($index = 1; $index -le $Runs; $index++) {
        Write-Host "audio cross-run $index/$Runs"
        $before = @{}
        Get-ChildItem -LiteralPath $absoluteOutput -Recurse -Filter report.json | ForEach-Object {
            $before[$_.FullName] = $true
        }
        & cargo run --release -p capture-lab -- --audio-loopback --segments $Segments --seconds $Seconds --out $absoluteOutput
        $runExit = $LASTEXITCODE
        $newReports = @(Get-ChildItem -LiteralPath $absoluteOutput -Recurse -Filter report.json | Where-Object {
            -not $before.ContainsKey($_.FullName)
        })
        if ($newReports.Count -ne 1) {
            throw "run $index produced $($newReports.Count) new report.json files; expected exactly one"
        }
        $reports += $newReports[0].FullName
        if ($runExit -ne 0) {
            Write-Warning "capture-lab run $index failed; the final verdict will retain its evidence"
        }
    }

    $arguments = @(
        "tools/audio_release_acceptance.py"
    ) + $reports + @(
        "--out",
        (Join-Path $absoluteOutput "verdict.json")
    )
    if ($RequiredBuild -gt 0) {
        $arguments += @("--required-build", "$RequiredBuild")
    }
    & $Python @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "audio cross-run hard gate failed; see $absoluteOutput\verdict.json"
    }
    Write-Host "audio cross-run hard gate passed: $absoluteOutput\verdict.json"
} finally {
    Pop-Location
}

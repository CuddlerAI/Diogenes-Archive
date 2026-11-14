param(
    [Parameter(Mandatory = $true)][string]$Manifest,
    [Parameter(Mandatory = $true)][string]$Scenario,
    [Parameter(Mandatory = $true)][string]$Tape,
    [Parameter(Mandatory = $true)][string]$RunDir,
    [string]$Python = "",
    [switch]$Resume
)

$ErrorActionPreference = "Stop"
$moduleRoot = Split-Path -Parent $PSScriptRoot
if (-not $Python) { $Python = $env:DIOGENES_PYTHON }
if (-not $Python) {
    $cmd = Get-Command py -ErrorAction SilentlyContinue
    if (-not $cmd) { $cmd = Get-Command python -ErrorAction SilentlyContinue }
    if ($cmd) { $Python = $cmd.Source }
}
if (-not $Python) { throw "Python 3.10+ not found. Pass -Python or set DIOGENES_PYTHON." }
$env:PYTHONPATH = $moduleRoot

& $Python -m gta_capture preflight --manifest $Manifest --mode real `
    --out (Join-Path $RunDir "preflight-before-run.json")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$args = @(
    "-m", "gta_capture", "run",
    "--manifest", $Manifest,
    "--scenario", $Scenario,
    "--tape", $Tape,
    "--run-dir", $RunDir,
    "--backend", "pipe"
)
if ($Resume) { $args += "--resume" }
& $Python @args
exit $LASTEXITCODE

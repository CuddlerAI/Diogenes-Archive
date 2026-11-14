param(
    [string]$OutRoot = "",
    [double]$Duration = 10.0,
    [string]$Python = ""
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
if (-not $OutRoot) {
    $OutRoot = Join-Path $moduleRoot "out/dryrun"
}
$tape = Join-Path $OutRoot "camera-tape.json"
$run = Join-Path $OutRoot "run"
$manifest = Join-Path $moduleRoot "examples/dependency-manifest.local.example.json"
$scenario = Join-Path $moduleRoot "examples/scenario.poc-10s.json"

if ([math]::Abs($Duration - 10.0) -gt 1e-9) {
    throw "The checked-in POC scenario is 10 seconds; edit a scenario copy before changing Duration."
}

New-Item -ItemType Directory -Force -Path $OutRoot | Out-Null
$env:PYTHONPATH = $moduleRoot
& $Python -m gta_capture make-demo-tape --out $tape --duration $Duration
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $Python -m gta_capture validate --manifest $manifest --scenario $scenario --tape $tape
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $Python -m gta_capture run --manifest $manifest --scenario $scenario --tape $tape `
    --run-dir $run --backend dry-run
exit $LASTEXITCODE

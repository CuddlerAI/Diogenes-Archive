param(
    [string]$Python = "",
    [string]$CMake = "",
    [string]$BuildDir = ""
)

$ErrorActionPreference = "Stop"
$moduleRoot = Split-Path -Parent $PSScriptRoot

function Resolve-Python([string]$Explicit) {
    $candidates = @()
    if ($Explicit) { $candidates += $Explicit }
    if ($env:DIOGENES_PYTHON) { $candidates += $env:DIOGENES_PYTHON }
    foreach ($name in @("py", "python")) {
        $cmd = Get-Command $name -ErrorAction SilentlyContinue
        if ($cmd) { $candidates += $cmd.Source }
    }
    foreach ($candidate in $candidates | Select-Object -Unique) {
        & $candidate -c "import sys; assert sys.version_info >= (3, 10)" 2>$null
        if ($LASTEXITCODE -eq 0) { return $candidate }
    }
    throw "Python 3.10+ not found. Pass -Python or set DIOGENES_PYTHON."
}

function Resolve-CMake([string]$Explicit) {
    if ($Explicit -and (Test-Path -LiteralPath $Explicit -PathType Leaf)) { return $Explicit }
    $cmd = Get-Command cmake -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $vswhere = "C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
    if (Test-Path -LiteralPath $vswhere) {
        $vs = & $vswhere -latest -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($vs) {
            $candidate = Join-Path $vs "Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe"
            if (Test-Path -LiteralPath $candidate) { return $candidate }
        }
    }
    throw "CMake not found. Pass -CMake or install Visual Studio C++ Build Tools."
}

$pythonExe = Resolve-Python $Python
$cmakeExe = Resolve-CMake $CMake
$ctestExe = Join-Path (Split-Path -Parent $cmakeExe) "ctest.exe"
if (-not $BuildDir) {
    $BuildDir = Join-Path $env:LOCALAPPDATA "DiogenesBuild/gta-controller"
}

Push-Location $moduleRoot
try {
    & $pythonExe -m unittest discover -s tests -v
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $cmakeExe -S native -B $BuildDir
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $cmakeExe --build $BuildDir --config Release
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $ctestExe --test-dir $BuildDir -C Release --output-on-failure
    exit $LASTEXITCODE
} finally {
    Pop-Location
}

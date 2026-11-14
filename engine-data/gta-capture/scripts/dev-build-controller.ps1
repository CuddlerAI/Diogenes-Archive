# Builds the controller (DiogenesGtaController.asi) and the native self-tests on a capture machine, for
# day-to-day iteration; the receipted build for a formal deployment is scripts/deploy-storymode-stack.ps1
# and README「构建 ASI 与构建收据」. Prints only warnings, errors, the built files and the test results.
#
#   powershell -ExecutionPolicy Bypass -File scripts\dev-build-controller.ps1 `
#     -BuildDir D:\workspace\mc4\build-native -SdkRoot D:\workspace\ScriptHookV_SDK
param(
    [Parameter(Mandatory = $true)][string]$BuildDir,       # out-of-tree build folder (kept between builds)
    [Parameter(Mandatory = $true)][string]$SdkRoot,        # ScriptHookV SDK root (inc/, lib/)
    [string]$Source = "",                                  # default: native\ beside this script's folder
    [string]$Cmake = ""                                    # default: cmake on PATH, else Visual Studio's
)
$ErrorActionPreference = "Continue"
# $PSScriptRoot came out empty in the param defaults when run over ssh with powershell -File (win-pc, 2026-10-16)
if (-not $Source) { $Source = Join-Path (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)) "native" }
if (-not $Cmake) {
    $onPath = Get-Command cmake.exe -ErrorAction SilentlyContinue
    if ($onPath) { $Cmake = $onPath.Source }
    else {
        $vs = Get-ChildItem "C:\Program Files\Microsoft Visual Studio\2022\*\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $vs) { throw "cmake not found: pass -Cmake" }
        $Cmake = $vs.FullName
    }
}
$sdk = ($SdkRoot -replace '\\', '/')
& $Cmake -S $Source -B $BuildDir -G "Visual Studio 17 2022" -A x64 -DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON "-DSCRIPT_HOOK_V_SDK_ROOT=$sdk" *> $null
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed (exit $LASTEXITCODE)" }
& $Cmake --build $BuildDir --config Release 2>&1 | Select-String -Pattern ' error |warning C|\.asi|\.exe' | ForEach-Object { $_.Line.Trim() }
if ($LASTEXITCODE -ne 0) { throw "build failed (exit $LASTEXITCODE)" }
Push-Location $BuildDir
try {
    $ctest = Join-Path (Split-Path $Cmake) "ctest.exe"
    & $ctest -C Release --output-on-failure 2>&1 | Select-String -Pattern 'tests passed|tests failed|Passed|Failed|\*\*\*' | ForEach-Object { $_.Line.Trim() }
    if ($LASTEXITCODE -ne 0) { throw "self-tests failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }
$asi = Join-Path $BuildDir "Release\DiogenesGtaController.asi"
"built {0}  {1}  {2}" -f $asi, (Get-Item $asi).LastWriteTime.ToString("s"), (Get-FileHash $asi).Hash.Substring(0, 16)

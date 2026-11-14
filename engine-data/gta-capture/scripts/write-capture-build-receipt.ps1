param(
    [Parameter(Mandatory = $true)][string]$RePlusSource,
    [Parameter(Mandatory = $true)][string]$RePlusBuildDir,
    [Parameter(Mandatory = $true)][string]$SimpleCameraSource,
    [Parameter(Mandatory = $true)][string]$ControllerBuildDir,
    [string]$RenderOutputFolder = "",
    [Parameter(Mandatory = $true)][string]$Output
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "render-profile.ps1")
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\..\..")).Path
$expectedRePlusCommit = "c6eba57160ca444817b149b3304ed1454e0ba2d9"
$expectedSimpleCameraCommit = "13c92ef2565403ffcc2501f779ca7cd935e5d598"
$expectedRePlusPatchSha = "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa"
$expectedIgcsPatchSha = "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5"
$patchRoot = Join-Path $PSScriptRoot "..\patches"

function Resolve-RequiredFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label not found: $Path" }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Resolve-RequiredDirectory([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { throw "$Label not found: $Path" }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Assert-PatchedSource([string]$Root, [string]$Commit, [string]$Patch,
                              [string]$ExpectedPatchSha) {
    $resolved = Resolve-RequiredDirectory $Root "source tree"
    $actual = (& git -C $resolved rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $actual -ne $Commit) {
        throw "source commit mismatch at ${resolved}: expected $Commit, found $actual"
    }
    $untracked = @(& git -C $resolved ls-files --others --exclude-standard)
    if ($LASTEXITCODE -ne 0 -or $untracked.Count -ne 0) {
        throw "source tree contains untracked files: $($untracked -join ', ')"
    }
    $diffPath = Join-Path ([IO.Path]::GetTempPath()) `
        ("diogenes-source-diff-" + [Guid]::NewGuid().ToString("N") + ".patch")
    try {
        & git -C $resolved diff HEAD --binary --no-ext-diff --output=$diffPath
        if ($LASTEXITCODE -ne 0) { throw "cannot calculate exact source diff at $resolved" }
        $actualDiffSha = (Get-FileHash -LiteralPath $diffPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $approvedPatchSha = (Get-FileHash -LiteralPath $Patch -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($approvedPatchSha -ne $ExpectedPatchSha -or $actualDiffSha -ne $ExpectedPatchSha) {
            throw "source diff at $resolved is not byte-identical to the approved patch"
        }
    } finally {
        if (Test-Path -LiteralPath $diffPath -PathType Leaf) {
            Remove-Item -LiteralPath $diffPath -Force
        }
    }
    return $resolved
}

function Resolve-MSBuild {
    $fromPath = Get-Command MSBuild.exe -ErrorAction SilentlyContinue
    if ($fromPath) { return $fromPath.Source }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $found = (& $vswhere -latest -products * -requires Microsoft.Component.MSBuild `
            -find "MSBuild\**\Bin\MSBuild.exe" | Select-Object -First 1)
        if ($found) { return (Resolve-Path -LiteralPath $found).Path }
    }
    throw "MSBuild.exe was not found in PATH or the latest Visual Studio installation"
}

function Read-CMakeValue([string]$BuildDir, [string]$Name) {
    $cache = Resolve-RequiredFile (Join-Path $BuildDir "CMakeCache.txt") "CMake cache"
    $line = Get-Content -LiteralPath $cache | Where-Object {
        $_ -match ('^' + [Regex]::Escape($Name) + ':[^=]+=(.*)$')
    } | Select-Object -First 1
    if (-not $line -or $line -notmatch '^[^=]+=(.*)$') { throw "$Name missing from $cache" }
    return $Matches[1]
}

function Assert-CMakeBuild([string]$BuildDir, [string]$ExpectedSource) {
    $resolvedBuild = Resolve-RequiredDirectory $BuildDir "CMake build directory"
    $actualSource = [IO.Path]::GetFullPath((Read-CMakeValue $resolvedBuild "CMAKE_HOME_DIRECTORY"))
    if (-not $actualSource.Equals($ExpectedSource, [StringComparison]::OrdinalIgnoreCase)) {
        throw "CMake build directory belongs to another source: $actualSource"
    }
    $cmake = Resolve-RequiredFile (Read-CMakeValue $resolvedBuild "CMAKE_COMMAND") "CMake executable"
    return [ordered]@{ build_dir = $resolvedBuild; cmake = $cmake }
}

function Assert-FreshArtifact([string]$Path, [datetime]$StartedAt, [string]$Label) {
    $resolved = Resolve-RequiredFile $Path $Label
    $item = Get-Item -LiteralPath $resolved
    if ($item.Length -le 0 -or $item.LastWriteTimeUtc -lt $StartedAt.AddSeconds(-2)) {
        throw "$Label was not freshly produced by this rebuild"
    }
    return $resolved
}

$rePlusPatch = Resolve-RequiredFile (Join-Path $patchRoot "replus-v18-evidence-fence.patch") `
    "RE+ v18 patch"
$igcsPatch = Resolve-RequiredFile (Join-Path $patchRoot "igcsconnector-v18-evidence-fence.patch") `
    "Igcs v18 patch"
$resolvedRePlus = Assert-PatchedSource $RePlusSource $expectedRePlusCommit $rePlusPatch `
    $expectedRePlusPatchSha
$resolvedSimpleCamera = Assert-PatchedSource $SimpleCameraSource $expectedSimpleCameraCommit `
    $igcsPatch $expectedIgcsPatchSha

$controllerSource = (Resolve-Path -LiteralPath (Join-Path $repositoryRoot `
    "engine-data\gta-capture\native")).Path
$controllerStatus = @(& git -C $repositoryRoot status --porcelain --untracked-files=all -- `
    engine-data/gta-capture/native)
if ($LASTEXITCODE -ne 0 -or $controllerStatus.Count -ne 0) {
    throw "controller native source is not committed and clean: $($controllerStatus -join '; ')"
}

$rePlusBuild = Assert-CMakeBuild $RePlusBuildDir $resolvedRePlus
$controllerBuild = Assert-CMakeBuild $ControllerBuildDir $controllerSource
$solution = Resolve-RequiredFile (Join-Path $resolvedSimpleCamera `
    "external\IgcsConnector-main\src\IgcsConnector.sln") "IgcsConnector solution"
$msbuild = Resolve-MSBuild
$startedAt = (Get-Date).ToUniversalTime()
# The receipt must record the exact argv that ran, not a paraphrase; build the
# argument lists once and reuse them for both execution and evidence.
$rePlusBuildArgs = @("--build", $rePlusBuild.build_dir, "--config", "Release",
    "--clean-first", "--parallel", "4")
$msbuildArgs = @($solution, "/t:Rebuild", "/m", "/p:Configuration=Release",
    "/p:Platform=x64", "/nologo")
$controllerBuildArgs = @("--build", $controllerBuild.build_dir, "--config", "Release",
    "--clean-first", "--parallel", "4")
$commands = @(
    ((@($rePlusBuild.cmake) + $rePlusBuildArgs) -join ' '),
    ((@($msbuild) + $msbuildArgs) -join ' '),
    ((@($controllerBuild.cmake) + $controllerBuildArgs) -join ' ')
)
$invocations = @(
    [ordered]@{ executable = $rePlusBuild.cmake; argv = $rePlusBuildArgs },
    [ordered]@{ executable = $msbuild; argv = $msbuildArgs },
    [ordered]@{ executable = $controllerBuild.cmake; argv = $controllerBuildArgs }
)

& $rePlusBuild.cmake @rePlusBuildArgs
if ($LASTEXITCODE -ne 0) { throw "RE+ clean Release rebuild failed" }
& $msbuild @msbuildArgs
if ($LASTEXITCODE -ne 0) { throw "IgcsConnector clean x64 Release rebuild failed" }
& $controllerBuild.cmake @controllerBuildArgs
if ($LASTEXITCODE -ne 0) { throw "Diogenes controller clean Release rebuild failed" }
$rePlusArtifact = Assert-FreshArtifact (Join-Path $rePlusBuild.build_dir `
    "BIN\Release\RockstarEditorPlus.asi") $startedAt "RockstarEditorPlus.asi"
$addonArtifact = Assert-FreshArtifact (Join-Path $resolvedSimpleCamera `
    "external\IgcsConnector-main\src\x64\Release\IgcsConnector.addon64") `
    $startedAt "IgcsConnector.addon64"
$controllerArtifact = Assert-FreshArtifact (Join-Path $controllerBuild.build_dir `
    "Release\DiogenesGtaController.asi") $startedAt "DiogenesGtaController.asi"
$rePlusMainIni = Resolve-RequiredFile (Join-Path $rePlusBuild.build_dir `
    "BIN\Release\RockstarEditorPlus.ini") "RockstarEditorPlus.ini"
$rePlusRenderIni = Resolve-RequiredFile (Join-Path $rePlusBuild.build_dir `
    "BIN\Release\Render.ini") "Render.ini"
$renderProfileTransform = Set-DiogenesRePlusRenderProfile $rePlusRenderIni `
    $RenderOutputFolder
$rePlusLightsIni = Resolve-RequiredFile (Join-Path $rePlusBuild.build_dir `
    "BIN\Release\Lights.ini") "Lights.ini"
$rePlusSettings = Assert-FreshArtifact (Join-Path $rePlusBuild.build_dir `
    "BIN\Release\RE+ Render Settings.exe") $startedAt "RE+ Render Settings.exe"
$igcsDofShader = Resolve-RequiredFile (Join-Path $resolvedSimpleCamera `
    "external\IgcsConnector-main\src\Shader\IgcsDof.fx") "IgcsDof.fx"
$finishedAt = (Get-Date).ToUniversalTime()

$receipt = [ordered]@{
    schema = "dm-gta-capture-build-receipt/1"
    started_at = $startedAt.ToString("o")
    finished_at = $finishedAt.ToString("o")
    sources = [ordered]@{
        replus_commit = $expectedRePlusCommit
        replus_patch_sha256 = $expectedRePlusPatchSha
        igcs_fork_commit = $expectedSimpleCameraCommit
        igcs_patch_sha256 = $expectedIgcsPatchSha
        controller_git_commit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
    }
    toolchain = [ordered]@{
        cmake_path = $rePlusBuild.cmake
        cmake_version = (& $rePlusBuild.cmake --version | Select-Object -First 1)
        msbuild_path = $msbuild
        msbuild_version = (& $msbuild -version -nologo | Select-Object -Last 1).Trim()
        commands = $commands
        invocations = $invocations
        configuration = "Release"
        platform = "x64"
    }
    configuration_transforms = [ordered]@{
        replus_render_ini = $renderProfileTransform
    }
    artifacts = [ordered]@{
        replus_asi = [ordered]@{ path = $rePlusArtifact; bytes = (Get-Item $rePlusArtifact).Length; sha256 = (Get-FileHash $rePlusArtifact -Algorithm SHA256).Hash.ToLowerInvariant() }
        replus_capture_addon = [ordered]@{ path = $addonArtifact; bytes = (Get-Item $addonArtifact).Length; sha256 = (Get-FileHash $addonArtifact -Algorithm SHA256).Hash.ToLowerInvariant() }
        diogenes_controller_asi = [ordered]@{ path = $controllerArtifact; bytes = (Get-Item $controllerArtifact).Length; sha256 = (Get-FileHash $controllerArtifact -Algorithm SHA256).Hash.ToLowerInvariant() }
        replus_main_ini = [ordered]@{ path = $rePlusMainIni; bytes = (Get-Item $rePlusMainIni).Length; sha256 = (Get-FileHash $rePlusMainIni -Algorithm SHA256).Hash.ToLowerInvariant() }
        replus_render_ini = [ordered]@{ path = $rePlusRenderIni; bytes = (Get-Item $rePlusRenderIni).Length; sha256 = (Get-FileHash $rePlusRenderIni -Algorithm SHA256).Hash.ToLowerInvariant() }
        replus_lights_ini = [ordered]@{ path = $rePlusLightsIni; bytes = (Get-Item $rePlusLightsIni).Length; sha256 = (Get-FileHash $rePlusLightsIni -Algorithm SHA256).Hash.ToLowerInvariant() }
        replus_settings_exe = [ordered]@{ path = $rePlusSettings; bytes = (Get-Item $rePlusSettings).Length; sha256 = (Get-FileHash $rePlusSettings -Algorithm SHA256).Hash.ToLowerInvariant() }
        igcs_dof_shader = [ordered]@{ path = $igcsDofShader; bytes = (Get-Item $igcsDofShader).Length; sha256 = (Get-FileHash $igcsDofShader -Algorithm SHA256).Hash.ToLowerInvariant() }
    }
}
$parent = Split-Path -Parent $Output
if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
$temporary = "$Output.writing"
$json = ($receipt | ConvertTo-Json -Depth 8) + [Environment]::NewLine
[IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
Move-Item -LiteralPath $temporary -Destination $Output -Force
Write-Host "Sealed clean-rebuild capture receipt: $Output"

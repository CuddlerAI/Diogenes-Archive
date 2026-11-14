[CmdletBinding()]
param(
    [switch]$RequireReleaseBinary,
    [switch]$AllowDefaultDev,
    [switch]$AllowAllGamesDev,
    [switch]$AllowDeliveredCatalog,
    [string]$ReleaseBinaryPath,
    [string]$ExpectedTargetRoot
)

$ErrorActionPreference = 'Stop'
$allowedFlavorSwitches = @($AllowDefaultDev, $AllowAllGamesDev, $AllowDeliveredCatalog)
if (@($allowedFlavorSwitches | Where-Object { [bool]$_ }).Count -gt 1) {
    throw '-AllowDefaultDev, -AllowAllGamesDev, and -AllowDeliveredCatalog are mutually exclusive.'
}
$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'windows-build-helpers.ps1')
$clientCargoPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\Cargo.toml'
$clientBuildPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\build.rs'
$acceptanceHelperManifestPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\windows\acceptance-helper.manifest'
$windowCatalogPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\platform\windows\catalog.rs'
$windowTrackerPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\platform\windows\window_tracker.rs'
$clientLibPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\lib.rs'
$commandsModPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\commands\mod.rs'
$servicesModPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\mod.rs'
$depthLabServicePath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\depth_lab.rs'
$earlyLaunchServicePath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\early_launch.rs'
$cp2077TrialServicePath = Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\cp2077_trial.rs'
$depthLabPanelPath = Join-Path $dataMinerRoot 'apps\client\src\DepthLabPanel.tsx'
$devPackagePath = Join-Path $dataMinerRoot 'apps\client\package.json'
$tauriConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.conf.json'
$installerBuildPath = Join-Path $dataMinerRoot 'tools\build-windows-installer.ps1'
$allGamesDevConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.all-games-dev.conf.json'
$releaseWorkflowPath = Join-Path $dataMinerRoot '..\.github\workflows\dataminer-release.yml'
$depthProbeReleaseWorkflowPath = Join-Path $dataMinerRoot '..\.github\workflows\depth-probe-release.yml'
$dataMinerCiWorkflowPath = Join-Path $dataMinerRoot '..\.github\workflows\dataminer-ci.yml'
$productionPaths = @(
    (Join-Path $dataMinerRoot 'Cargo.toml'),
    (Join-Path $dataMinerRoot 'apps\client\src-tauri\Cargo.toml'),
    (Join-Path $dataMinerRoot 'apps\client\src-tauri\src'),
    (Join-Path $dataMinerRoot 'crates\capture-core\Cargo.toml'),
    (Join-Path $dataMinerRoot 'crates\capture-core\src'),
    (Join-Path $dataMinerRoot 'crates\windows-capture-wgc\Cargo.toml'),
    (Join-Path $dataMinerRoot 'crates\windows-capture-wgc\src'),
    (Join-Path $dataMinerRoot 'tools\capture-lab\Cargo.toml'),
    (Join-Path $dataMinerRoot 'tools\capture-lab\src')
)
$forbidden = @(
    'GraphicsCapturePicker',
    'CreateForMonitor',
    'IDXGIOutputDuplication',
    'DuplicateOutput',
    'D3D_DRIVER_TYPE_SOFTWARE',
    'MFT_ENUM_FLAG_SYNCMFT',
    'libx264',
    'ffmpeg',
    'ffprobe',
    # depth-lab 的真正注入入口（docs/77 §10.3）不得进入客户端生产源码。
    # DLL/工具名本身会合法出现在 feature-gated 胶水与 workspace member 中，
    # 因此不能作为全源码禁词；下方另行锁定默认 feature 与正式安装脚本。
    'CreateRemoteThread',
    'hooked_present',
    # 原神本地测试车道（bundle + debug-only 开关 + 进程名）随 issue #855 PR 2 删除。
    # 禁词留下来，语义翻面：从前"只许出现在 debug 构建"，现在"哪个构建里都不许出现"。
    'DATAMINER_ALLOW_UNGRANTED_REAL_GAME_TEST',
    'dataminer-yuanshen-ungranted-dev',
    'yuanshen-ungranted',
    'YuanShen.exe'
)
# 原神本地测试 bundle 与它的 debug-only 开关随 issue #855 PR 2 一并删除，因此这一组
# 从"只在 release 二进制里禁"升级成 $forbidden 里的常驻禁词（见上）：源码与二进制
# 两面都不许再出现。这个数组保留是因为下方 production 口味还要往里追加游戏身份 token。
$releaseBinaryOnlyForbidden = @()
$codSteamReleaseTestTokens = @(
    'dataminer-call-of-duty-steam-release-test',
    'call-of-duty-steam-release-test',
    'cod.exe',
    'cod22-cod.exe',
    'cod23-cod.exe',
    'cod24-cod.exe',
    'sp22-cod.exe',
    'ModernWarfare.exe'
)
$bioshockInfiniteTokens = @(
    'dataminer-bioshock-infinite-steam-e2e',
    'bioshock-infinite-steam-e2e',
    'BioShockInfinite.exe',
    'LaunchUnrealUWindowsClient'
)
$wolfenstein2Tokens = @(
    'dataminer-wolfenstein2-steam-dev',
    'wolfenstein2-steam',
    'NewColossus_x64vk.exe',
    'Wolfenstein II The New Colossus'
)
$ageOfEmpires4Tokens = @(
    'dataminer-age-of-empires-4-steam-dev',
    'age-of-empires-4-steam',
    'RelicCardinal.exe',
    'Plat::Window {DB3DC0D7-BBA3-4d06-BCD8-40CD448B4AE3}'
)
$wuchangTokens = @(
    'dataminer-wuchang-fallen-feathers-steam-dev',
    'wuchang-fallen-feathers-steam',
    'Project_Plague-Win64-Shipping.exe',
    'UnrealWindow'
)
$haloInfiniteTokens = @(
    'dataminer-halo-infinite-steam-release-test',
    'halo-infinite-steam-release-test',
    'HaloInfinite.exe'
)
$euroTruckSimulator2Tokens = @(
    'dataminer-euro-truck-simulator-2-steam-dev',
    'euro-truck-simulator-2-steam',
    'eurotrucks2.exe',
    'prism3d'
)
$localSteamCatalogTokens = @(
    'dataminer-a-plague-tale-innocence-steam-release-test',
    'a-plague-tale-innocence-steam-release-test',
    'APlagueTaleInnocence_x64.exe',
    'dataminer-fallout4-steam-release-test',
    'fallout4-steam-release-test',
    'Fallout4.exe',
    'dataminer-hades-ii-steam-release-test',
    'hades-ii-steam-release-test',
    'Hades2.exe',
    'dataminer-cyberpunk-2077-steam-dev',
    'cyberpunk-2077-steam',
    'Cyberpunk2077.exe',
    'dataminer-total-war-warhammer-iii-steam-release-test',
    'total-war-warhammer-iii-steam-release-test',
    'Warhammer3.exe',
    'dataminer-nioh2-steam-dev',
    'nioh2-steam',
    'nioh2.exe',
    'NIOH',
    'dataminer-mount-and-blade-2-bannerlord-steam-dev',
    'mount-and-blade-2-bannerlord-steam',
    'Bannerlord.exe',
    'TWApplication'
)
$preflightOnlyCatalogTokens = @(
    'dataminer-halo-reach-mcc-s0',
    'halo-reach-mcc-s0',
    'MCC-Win64-Shipping.exe',
    'mcclauncher.exe',
    'dataminer-elden-ring-nightreign-steam-release-test',
    'elden-ring-nightreign-steam-release-test',
    'nightreign.exe'
)
$nonCodAllGamesTokens = @(
    $bioshockInfiniteTokens
    $wolfenstein2Tokens
    $ageOfEmpires4Tokens
    $wuchangTokens
    $haloInfiniteTokens
    $euroTruckSimulator2Tokens
    $localSteamCatalogTokens
    $preflightOnlyCatalogTokens
)
# 手抄组之外，再从 shipped-games.json 与各 game-profile.json 派生 gameId 与
# 进程名 token 并集进来：14 款 pc-local 游戏在手抄组里零覆盖（#822 起没人往
# 这里加过），派生集让「新游戏该在时缺席 / production 不该在时泄漏」两个方向
# 对未来登记的游戏自动生效。手抄组保留——窗口类等特殊 token 只有人工知道。
$shippedGamesManifestPath = Join-Path $dataMinerRoot 'config\catalogs\shipped-games.json'
$shippedGamesManifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $shippedGamesManifestPath | ConvertFrom-Json
$shippedGameDerivedTokens = foreach ($shippedGame in @($shippedGamesManifest.games)) {
    [string]$shippedGame.gameId
    $shippedProfilePath = Join-Path $dataMinerRoot "config\catalogs\dev\$([string]$shippedGame.catalogDir)\game-profile.json"
    $shippedProfile = Get-Content -Raw -Encoding UTF8 -LiteralPath $shippedProfilePath | ConvertFrom-Json
    foreach ($processName in @($shippedProfile.executable_names)) { [string]$processName }
}
$allGamesTokens = @(
    @($codSteamReleaseTestTokens) + $nonCodAllGamesTokens + $shippedGameDerivedTokens |
        Sort-Object -Unique
)
if ($AllowAllGamesDev -or $AllowDefaultDev) {
    # 两个 dev 口味都随 dev-catalog 内嵌全部游戏目录（client build.rs 从
    # shipped-games.json 派生），下方的二进制扫描要求全部 token 在场。
} else {
    # Keep the strict identity scan for capture-lab. The explicitly requested
    # online client below additionally distinguishes formal runtime identities.
    $releaseBinaryOnlyForbidden += $allGamesTokens
}

$sourceFiles = foreach ($path in $productionPaths) {
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        Get-Item -LiteralPath $path
    } else {
        Get-ChildItem -LiteralPath $path -Recurse -File
    }
}
$violations = $sourceFiles | Select-String -SimpleMatch -Pattern $forbidden
if ($violations) {
    $violations | ForEach-Object { Write-Error "Forbidden release surface: $($_.Path):$($_.LineNumber): $($_.Line.Trim())" }
    throw 'Release surface contains a forbidden capture or encoder entry point.'
}

$sourceOnlyForbidden = @(
    'GetAsyncKeyState',
    'GetCursorPos'
)
$inputPollingViolations = $sourceFiles | Select-String -SimpleMatch -Pattern $sourceOnlyForbidden
if ($inputPollingViolations) {
    $inputPollingViolations | ForEach-Object { Write-Error "Forbidden input polling surface: $($_.Path):$($_.LineNumber): $($_.Line.Trim())" }
    throw 'Release source contains a forbidden input polling entry point.'
}

$required = @(
    @{ Token = 'CreateForWindow'; Path = 'crates\windows-capture-wgc\src\window.rs' },
    @{ Token = 'CopyResource'; Path = 'crates\capture-core\src\capture.rs' },
    @{ Token = 'VideoProcessorBlt'; Path = 'crates\capture-core\src\gpu.rs' },
    @{ Token = 'MF_READWRITE_USE_ONLY_HARDWARE_TRANSFORMS'; Path = 'crates\capture-core\src\mf.rs' },
    @{ Token = 'MF_SINK_WRITER_DISABLE_THROTTLING'; Path = 'crates\capture-core\src\mf.rs' },
    @{ Token = 'MF_LOW_LATENCY'; Path = 'crates\capture-core\src\mf.rs' },
    @{ Token = 'hardware_capability_validated_best_effort_controls'; Path = 'crates\capture-core\src\mf.rs' }
)
foreach ($entry in $required) {
    $path = Join-Path $dataMinerRoot $entry.Path
    if (-not (Select-String -LiteralPath $path -SimpleMatch -Quiet -Pattern $entry.Token)) {
        throw "Required release contract '$($entry.Token)' is absent from $path"
    }
}

$clientCargo = Get-Content -Raw -Encoding UTF8 -LiteralPath $clientCargoPath
if ($clientCargo -notmatch '(?ms)^\[features\].*?^default\s*=\s*\[\].*?^dev-catalog\s*=\s*\[\]') {
    throw 'dataminer-client must keep dev-catalog explicit and out of default Cargo features.'
}
if ($clientCargo -notmatch '(?m)^depth-lab\s*=\s*\["dep:depth-lab",\s*"dep:zip",\s*"dep:time"\]' -or
    $clientCargo -notmatch '(?m)^depth-lab\s*=\s*\{[^\r\n]*optional\s*=\s*true') {
    throw 'Depth capture must remain an explicit optional client feature and dependency.'
}
$installerBuild = Get-Content -Raw -Encoding UTF8 -LiteralPath $installerBuildPath
$allGamesDevConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $allGamesDevConfigPath
$productionBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor production
$deliveredCatalogBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor delivered_catalog
$defaultDevBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor default_dev
$allGamesBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor all_games_dev
$productionFeatures = [string[]]@($productionBuildCase.Features)
$deliveredCatalogFeatures = [string[]]@($deliveredCatalogBuildCase.Features)
$defaultDevFeatures = [string[]]@($defaultDevBuildCase.Features)
$allGamesFeatures = [string[]]@($allGamesBuildCase.Features)
$clientLib = Get-Content -Raw -Encoding UTF8 -LiteralPath $clientLibPath
$commandsMod = Get-Content -Raw -Encoding UTF8 -LiteralPath $commandsModPath
$servicesMod = Get-Content -Raw -Encoding UTF8 -LiteralPath $servicesModPath
$depthLabService = Get-Content -Raw -Encoding UTF8 -LiteralPath $depthLabServicePath
$earlyLaunchService = Get-Content -Raw -Encoding UTF8 -LiteralPath $earlyLaunchServicePath
$cp2077TrialService = Get-Content -Raw -Encoding UTF8 -LiteralPath $cp2077TrialServicePath
$depthLabPanel = Get-Content -Raw -Encoding UTF8 -LiteralPath $depthLabPanelPath
# Evaluate the actual command definitions, imports and generate_handler entries
# with cfg(test)=false; ordinary Rust unit builds expose test-only commands and
# cannot prove that Delivered Catalog has a usable formal launch/preflight IPC.
& node (Join-Path $PSScriptRoot 'check-depth-command-surface.mjs') $dataMinerRoot
if ($LASTEXITCODE -ne 0) {
    throw 'Depth Tauri command feature matrix does not match the shipped release surface.'
}
& node --test (Join-Path $PSScriptRoot 'tests\check-depth-command-surface.test.mjs')
if ($LASTEXITCODE -ne 0) {
    throw 'Depth Tauri command surface negative regression suite failed.'
}
# 这里**曾经**是一条硬闸：安装脚本只要提到 depth-lab 就 throw。
#
# 2026-08-09 裁决 R-1（docs/109 §10）采纳「乙」——正式包带 depth 开关——之后那条闸
# 与裁决直接冲突，所以拆掉。**但不是无条件放行**：拆一道闸必须说清替代保护是什么，
# 否则下一个人只会看到「闸没了」。
#
# 替代保护有两层，都在闸之外、且都不是这个脚本能给的：
#
#   1. 运行时五重门（depth-lab `evaluate_gates`）里的两道**数据驱动**门——
#      `game_whitelisted`（逐游戏 profile 标定）与 `fps_gate_certified`
#      （按 backend generation + 显式硬件覆盖名单的中央签名性能认证）。两者都是「默认关、必须有人做过
#      标定/实测才打开」，所以正式包人人都有二进制之后，开关在每台机器上**仍然是灰的**。
#   2. 采集员侧的逐条风险确认 + 开关每次启动重置为关（77 号 §6.3）。
#
# 这个脚本继续守住的是**另外两件事**，它们与 R-1 无关、不因裁决而放松：
#
#   - 注入入口（`CreateRemoteThread` / `hooked_present`）不得出现在客户端生产源码里
#     （上方 `$forbidden`）——注入实现留在独立的 probe crate，客户端只起进程；
#   - `depth-lab` 必须保持**显式可选** feature 与可选依赖（下方两条断言）——
#     它可以被安装脚本显式打开，但**永远不能进 `default`**。
#
# 换句话说：闸从「不许进安装包」收窄成「不许悄悄进」。正式 Depth 必须同时
# 出现在两个线上口味与 All Games DEV；默认 DEV 仍不带。
foreach ($formalDepthCase in @(
    @{ Name = 'production'; Features = $productionFeatures },
    @{ Name = 'delivered_catalog'; Features = $deliveredCatalogFeatures },
    @{ Name = 'all_games_dev'; Features = $allGamesFeatures }
)) {
    if (@($formalDepthCase.Features) -notcontains 'depth-lab') {
        throw "Shipped case '$($formalDepthCase.Name)' must enable depth-lab explicitly (docs/107 and docs/109 §10 R-1)."
    }
}
if ($depthLabService -notmatch '(?s)fn cp2077_discovery_available\(\).*?!crate::build_flavor::uses_delivered_catalog\(\)' -or
    $cp2077TrialService -notmatch '(?s)available:\s*!crate::build_flavor::uses_delivered_catalog\(\)' -or
    $cp2077TrialService -notmatch '(?s)pub fn prepare_and_launch\(.*?if crate::build_flavor::uses_delivered_catalog\(\)') {
    throw 'Server-delivered builds must keep CP2077 local trial/discovery disabled while formal Depth is enabled.'
}
$testableLocalCp2077FeatureGate = '#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]'
$gatedCp2077ModulePattern = [regex]::Escape($testableLocalCp2077FeatureGate) + '\s+pub mod cp2077_trial;'
if ($clientLib -notmatch ([regex]::Escape($testableLocalCp2077FeatureGate) + '\s+pub mod cp2077_raw_demo;') -or
    $commandsMod -notmatch $gatedCp2077ModulePattern -or
    $servicesMod -notmatch ([regex]::Escape($testableLocalCp2077FeatureGate) + '\s+pub mod cp2077_trial;') -or
    $earlyLaunchService -notmatch '(?s)fn from_local_trial_game_id\(.*?#\[cfg\(any\(feature = "dev-catalog", test\)\)\].*?Self::ALL' -or
    $earlyLaunchService -notmatch '(?s)fn from_formal_game_id\(.*?Self::ALL' -or
    $depthLabPanel -match 'cyberpunk-2077-steam') {
    throw 'CP2077 local trial/raw-demo early-launch must stay DEV/test-only.'
}
# Both allowlisted targets are formally promoted (CP2077 since 2026-09-18); the formal lookup
# still pins each one's exact executable.
if ($earlyLaunchService -notmatch 'KingdomCome2SteamBuild23914554' -or
    $earlyLaunchService -notmatch 'Cyberpunk2077SteamBuild20383525' -or
    $earlyLaunchService -notmatch '(?s)fn verify_executable_identity\(' -or
    $depthLabPanel -match '启动 CP2077（Depth）|CP2077 Depth 启动环境') {
    throw 'KCD2 and CP2077 formal early-launch targets must stay allowlisted with exact executables and use the configured game identity in UI text.'
}
foreach ($earlyTool in @(
    'dm-depth-launch.exe',
    'dm-depth-extract.exe',
    'dm_depth_probe.dll',
    'dm-depth-probe-runtime.json'
)) {
    if ($installerBuild -notmatch [regex]::Escape($earlyTool) -or
        $allGamesDevConfig -notmatch [regex]::Escape($earlyTool)) {
        throw "All Games DEV must validate and bundle locked CP2077 early-launch artifact '$earlyTool'."
    }
}
if ($installerBuild -notmatch 'Stage-LockedDepthProbeArtifact' -or
    $installerBuild -notmatch 'artifact-lock\.json' -or
    $installerBuild -match "(?s)Invoke-NativeChecked[^\r\n]*cargo[^\r\n]*dm-depth-probe") {
    throw 'Ordinary Depth-enabled packaging must validate a locked independent artifact and must not compile dm-depth-probe.'
}
foreach ($captureRecipe in @(
    'Set-Cp2077CaptureSettings.ps1',
    'Test-Cp2077CaptureSettings.ps1',
    'package-local-fivepiece.py'
)) {
    if ($installerBuild -notmatch [regex]::Escape($captureRecipe) -or
        $allGamesDevConfig -notmatch [regex]::Escape($captureRecipe)) {
        throw "All Games DEV must verify and bundle CP2077 capture recipe '$captureRecipe'."
    }
}
$requiredFormalDepthResourceDestinations = @(
    'depth-lab/dm_depth_probe.dll',
    'depth-lab/dm-depth-inject.exe',
    'depth-lab/dm-depth-launch.exe',
    'depth-lab/dm-depth-probe-runtime.json'
)
foreach ($destination in $requiredFormalDepthResourceDestinations) {
    if ($installerBuild -notmatch [regex]::Escape("Destination = '$destination'")) {
        throw "Depth-enabled online resources omit formal runtime destination '$destination'."
    }
}
$requiredLocalCp2077ResourceDestinations = @(
    'dm-depth-extract.exe',
    'depth-lab/Set-Cp2077CaptureSettings.ps1',
    'depth-lab/Test-Cp2077CaptureSettings.ps1',
    'depth-lab/package-local-fivepiece.py'
)
foreach ($destination in $requiredLocalCp2077ResourceDestinations) {
    if ($installerBuild -notmatch [regex]::Escape("Destination = '$destination'")) {
        throw "Generated All Games DEV resources omit local CP2077 destination '$destination'."
    }
}
$releaseWorkflow = Get-Content -Raw -Encoding UTF8 -LiteralPath $releaseWorkflowPath
if ($releaseWorkflow -notmatch [regex]::Escape("`$expectedCargoFeatures = @('depth-lab')") -or
    $releaseWorkflow -notmatch [regex]::Escape('depth_resource_destinations') -or
    $releaseWorkflow -notmatch [regex]::Escape('DM_DEPTH_PROBE_ARTIFACT_PREFIX') -or
    $releaseWorkflow -notmatch [regex]::Escape('DM_DEPTH_PROBE_ARTIFACT_DIR') -or
    $releaseWorkflow -notmatch [regex]::Escape('dm-depth-probe-runtime.json')) {
    throw 'The release workflow must download a selected locked Depth artifact and assert every packaged runtime file.'
}
$depthProbeReleaseWorkflow = Get-Content -Raw -Encoding UTF8 -LiteralPath $depthProbeReleaseWorkflowPath
if ($depthProbeReleaseWorkflow -notmatch [regex]::Escape('fromJSON(vars.DM_WIN_RUNNER') -or
    $depthProbeReleaseWorkflow -notmatch [regex]::Escape('["self-hosted","Windows","X64","dm-pr-1"]') -or
    $depthProbeReleaseWorkflow -match '(?m)^\s*runs-on:\s*windows-latest\s*$') {
    throw 'Depth probe artifacts must build on the configured physical Windows runner, never windows-latest.'
}
if ($depthProbeReleaseWorkflow -notmatch [regex]::Escape('cargo metadata --format-version 1 --no-deps --locked') -or
    $depthProbeReleaseWorkflow -notmatch [regex]::Escape('release_directory=$releaseDirectory') -or
    $depthProbeReleaseWorkflow -notmatch [regex]::Escape('-BuildDirectory $env:BUILD_DIRECTORY')) {
    throw 'Depth probe publication must resolve the self-hosted Cargo target directory instead of assuming DataMiner/target.'
}
if ($depthProbeReleaseWorkflow -notmatch '(?ms)^\s+- name: Publish immutable Depth probe artifact to R2\s+.*?^\s+working-directory:\s*\.\s*$') {
    throw 'Depth probe R2 publication must run from the workspace root where the artifact is downloaded.'
}
$lockSkipIndex = $depthProbeReleaseWorkflow.IndexOf(
    '[[ "$file" == */artifact-lock.json ]] && continue',
    [StringComparison]::Ordinal
)
$lockPublishIndex = $depthProbeReleaseWorkflow.IndexOf(
    '--file artifact/artifact-lock.json --content-type application/json --remote',
    [StringComparison]::Ordinal
)
if ($lockSkipIndex -lt 0 -or $lockPublishIndex -le $lockSkipIndex) {
    throw 'Depth probe publication must upload payloads first and artifact-lock.json last as the commit marker.'
}
$devCatalogGameFeatures = [string[]]@(
    [regex]::Matches(
        $clientCargo,
        '(?m)^([a-z0-9-]+)\s*=\s*\["dev-catalog"\]\s*$'
    ) | ForEach-Object { $_.Groups[1].Value }
)
# 逐游戏 cargo feature 已整体退役：游戏随 dev-catalog 由 build.rs 从
# shipped-games.json 派生内嵌。谁把单游戏特性写回 Cargo.toml，这里第一个红——
# 那种登记面在名单 B 漂移（14 个 pc-local 特性缺席 mut 门）上付过一次学费。
if ($devCatalogGameFeatures.Count -ne 0) {
    throw "Per-game cargo features are retired (games embed via shipped-games.json): $($devCatalogGameFeatures -join ', ')"
}
$expectedAllGamesFeatures = @('dev-catalog', 'local-catalog-only', 'depth-lab')
$allGamesFeatureDrift = @(Compare-Object `
    -ReferenceObject @($expectedAllGamesFeatures | Sort-Object) `
    -DifferenceObject @($allGamesFeatures | Sort-Object))
if ($allGamesFeatureDrift.Count -ne 0) {
    throw "All Games DEV features must be exactly dev-catalog + local-catalog-only + depth-lab: $($allGamesFeatureDrift | Out-String)"
}
$catalogSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $windowCatalogPath
# 批次二（docs/57 §3.3）把这条禁令翻了面，docs/100 又把它的对象收窄了。断代前
# certified 有一份编译内嵌的 `config/catalogs/certified/catalog.json`，这里断言它
# 必须被 include；断代后改成断言**绝不许**把任何 certified 载荷编进二进制；
# certified 车道整条废除（docs/100）之后那个目录已经不存在，这条禁令因此变成一条
# 防回归的锚：谁再把 certified 载荷写回来，这里第一个红。
# 禁令锚在**字符串字面量**上（前导双引号），不是裸路径：include_bytes! 的载荷
# 一律写成 concat!(env!(...), "/catalogs/...")，而散文里提到这条路径的地方
# （比如 empty() 的文档注释在解释它从前是个文件）不该把守卫打红。
# `fn empty` 是装机后、首次下发前的空基线（车道废除后由 empty_certified 更名）。
if ($catalogSource -notmatch '#\[cfg\(feature = "dev-catalog"\)\]' -or
    $catalogSource -notmatch 'DATAMINER_CONFIG_DIR' -or
    $catalogSource -match '"/catalogs/certified/' -or
    $catalogSource -notmatch 'fn empty\(\)') {
    throw 'Catalog source must gate dev payloads by feature, compile in no certified payload, and keep the empty catalog baseline.'
}
if ($catalogSource -match '(?s)#\[cfg_attr\(feature = "dev-catalog", allow\(dead_code\)\)\]\s*#\[must_use\]\s*pub fn empty\(\)') {
    throw 'The empty catalog baseline must use an exact reachability cfg instead of suppressing dev-catalog dead_code.'
}
$clientBuild = Get-Content -Raw -Encoding UTF8 -LiteralPath $clientBuildPath
if ($clientBuild -notmatch 'cargo:rustc-env=DATAMINER_CONFIG_DIR=' -or
    $clientBuild -notmatch '\.\./\.\./\.\./config') {
    throw 'Client build must pin DATAMINER_CONFIG_DIR to the repository config directory.'
}
$acceptanceHelperManifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $acceptanceHelperManifestPath
if ($clientBuild -notmatch 'DATAMINER_ACCEPTANCE_HELPER_BUILD' -or
    $clientBuild -notmatch 'ACCEPTANCE_HELPER_MANIFEST' -or
    $acceptanceHelperManifest -notmatch 'requestedExecutionLevel level="asInvoker"') {
    throw 'The console acceptance helper must keep its explicit asInvoker manifest without changing the main client UAC boundary.'
}
# 原神那条 debug-only 真实游戏测试车道整体退役（issue #855 PR 2）。断言因此从
# "这些定义必须带 debug gate"翻成"这些定义必须不存在"：一条被删掉的车道不需要
# 门，需要的是不许有人把门连同房间一起搬回来。
foreach ($definition in @(
    'YUANSHEN_CATALOG_BYTES',
    'YUANSHEN_DEVICE_BYTES',
    'YUANSHEN_RECORDING_BYTES',
    'YUANSHEN_GAME_BYTES',
    'YUANSHEN_MAPPING_BYTES',
    'UNGRANTED_REAL_GAME_TEST_ENV'
)) {
    if ($catalogSource -match "(?:pub )?const $definition") {
        throw "Retired real-game test definition '$definition' is back in catalog.rs; it was deleted with the bundle (issue #855)."
    }
}
# 游戏目录内嵌一律走 build.rs 生成的 embedded_dev_catalogs.rs；catalog.rs 源码
# 里现在只许 synthetic（dev 基线）一个目录字面量——原神那个 debug 专属例外随
# bundle 一起删除。谁往源码里加回手写的逐游戏 include_bytes!，这里第一个红。
if ($catalogSource -notmatch 'embedded_dev_catalogs\.rs') {
    throw 'CatalogStore must load game bundles through the generated embedded_dev_catalogs.rs include.'
}
$handWrittenCatalogDirs = @(
    [regex]::Matches($catalogSource, '"/catalogs/dev/([a-z0-9-]+)/') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
)
$allowedHandWrittenCatalogDirs = @('synthetic')
$unexpectedCatalogDirs = @($handWrittenCatalogDirs | Where-Object { $_ -notin $allowedHandWrittenCatalogDirs })
if ($unexpectedCatalogDirs.Count -ne 0) {
    throw "Hand-written per-game catalog embeds are retired; move these into shipped-games.json instead: $($unexpectedCatalogDirs -join ', ')"
}
# 下发信任锚必须是**一把**、且不看任何条件编译。
#
# 断代前这条断言读的是 `delivery_channel_for_build() -> CatalogMode::X`：certified
# 车道废除后（docs/100）渠道只剩一个值，所以它是个常量。issue #855 §A.1 把「渠道」
# 这个概念本身从下发链路上删掉了（客户端不再向服务端表达 dev/production/certified），
# 那个函数随之消失——但它守的那件事一个字没变，只是换了主语：
#
# 客户端的信任边界是「哪把钉死的公钥验得过」。这个选择一旦重新依赖任何 cfg
# （尤其 debug_assertions），就可能与 `CatalogStore::load` 编进去的那套目录脱钩——
# `-CodSteamReleaseTest` 那种 release + dev-catalog 的包正是这么坏过的：内嵌 dev
# 目录，却把自己当另一条渠道的客户端去拉生产目录。断言守在源码层，因为
# `cargo test` 是 debug 构建，Rust 单测在默认 profile 下看不出这个差别。
$deliverySource = Get-Content -Raw -Encoding UTF8 -LiteralPath (
    Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\catalog_delivery.rs')
if ($catalogSource -notmatch '(?ms)fn pinned_delivery_anchor\(\)[^\{]*\{\s*VerifyingKey::from_bytes\(&DEV_CATALOG_ANCHOR\)') {
    throw 'The signed-catalog trust anchor must stay a single unconditional pinned constant (issue #855 retired the channel argument).'
}
$anchorBody = [regex]::Match(
    $catalogSource,
    '(?ms)fn pinned_delivery_anchor\(\).*?\n\}').Value
if ($anchorBody -match '#\[cfg|cfg!\(') {
    throw 'The signed-catalog trust anchor must not depend on conditional compilation; the compiled-in catalog does not.'
}
foreach ($retired in @('delivery_channel_for_build', 'CatalogMode')) {
    if ($deliverySource -match [regex]::Escape($retired) -and
        $deliverySource -notmatch ('//[^\n]*' + [regex]::Escape($retired))) {
        throw "Catalog delivery must not reintroduce the retired channel vocabulary: $retired (issue #855 A.1)."
    }
}
if ($deliverySource -match 'cfg!\(all\(feature = "dev-catalog", debug_assertions\)\)') {
    throw 'Catalog delivery must not depend on debug_assertions; the compiled-in catalog does not.'
}
$windowTrackerSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $windowTrackerPath
# 下面两条禁令针对的是**整个客户端 crate**，不只是 window_tracker：越权的进程
# 访问位和废除的伪指纹格式在哪个文件里冒出来都一样危险。
$clientSources = (Get-ChildItem -Recurse -File -Filter '*.rs' `
        -LiteralPath (Join-Path $dataMinerRoot 'apps\client\src-tauri\src') |
    Get-Content -Raw -Encoding UTF8) -join "`n"
if ($windowTrackerSource -match 'include_ungranted_real_game_test') {
    throw 'The ungranted real-game test switch is retired (issue #855); window_tracker must not reintroduce it.'
}

# --- Every embedded catalog payload must be a committed file ------------------
#
# docs/54 recorded a collection box running a client whose game list contained
# six profiles that exist in no git ref. An untracked bundle compiled into the
# client produces capture sessions whose catalog has no source of truth: it
# cannot be reviewed, cannot be hash-checked by
# `ml/player/tests/test_new_game_catalogs.py`, and cannot be reconciled against
# the server roster. This gate closes the reproduction path — a build that
# embeds an untracked payload fails here rather than shipping.
#
# It checks tracking, not content. Authoring a new bundle stays a normal PR:
# commit the six files and this passes.
$embeddedPayloadSources = @(
    $windowCatalogPath,
    (Join-Path $dataMinerRoot 'apps\client\src-tauri\src\services\catalog_delivery.rs')
)
$embeddedPayloadPattern = 'include_bytes!\(\s*concat!\(\s*env!\("(?<env>[A-Z_]+)"\)\s*,\s*"(?<path>[^"]+)"\s*,?\s*\)\s*\)'
$embeddedPayloads = [System.Collections.Generic.List[string]]::new()
foreach ($payloadSource in $embeddedPayloadSources) {
    if (-not (Test-Path -LiteralPath $payloadSource -PathType Leaf)) {
        throw "Embedded-payload source is missing: $payloadSource"
    }
    $payloadText = Get-Content -Raw -Encoding UTF8 -LiteralPath $payloadSource
    foreach ($match in [regex]::Matches($payloadText, $embeddedPayloadPattern, 'Singleline')) {
        $literal = $match.Groups['path'].Value -replace '\\', '/'
        # Both spellings land in DataMiner/config, but by different routes:
        # DATAMINER_CONFIG_DIR *is* that directory, so any suffix resolves under
        # it. CARGO_MANIFEST_DIR sits at apps/client/src-tauri and reaches it via
        # ../../../config. Every embedded payload today lives under
        # config/catalogs/ as a self-contained bundle directory (docs/57 §3.1).
        switch ($match.Groups['env'].Value) {
            'DATAMINER_CONFIG_DIR' {
                $relative = $literal.TrimStart('/')
            }
            'CARGO_MANIFEST_DIR' {
                if ($literal -notmatch '(?:^|/)config/(?<rel>.+)$') {
                    throw "Embedded payload '$literal' in $payloadSource is CARGO_MANIFEST_DIR-relative but does not reach DataMiner/config; extend the release-surface check instead of bypassing it."
                }
                $relative = $Matches['rel']
            }
            default {
                # Fail closed: a payload shape this gate cannot resolve is exactly
                # the case it exists to catch. Teach it the new shape, don't skip.
                throw "Unrecognized embedded-payload root env!(""$($match.Groups['env'].Value)"") in $payloadSource; extend the release-surface check instead of bypassing it."
            }
        }
        if ([string]::IsNullOrWhiteSpace($relative)) {
            throw "Embedded payload in $payloadSource resolved to an empty path."
        }
        $embeddedPayloads.Add(("config/{0}" -f $relative)) | Out-Null
    }
}
# build.rs 从 shipped-games.json 派生的内嵌目录同样必须全部是已提交文件——
# 这正是 docs/54 那次「六份 profile 不在任何 git ref 里」事故的防复发闸在
# 新机制下的等价延伸。可选载荷按磁盘在场性入列，与生成器同一判定。
foreach ($shippedGame in @($shippedGamesManifest.games)) {
    # StrictMode 下缺失属性直接访问会先抛系统异常，定制报错不可达——经
    # PSObject.Properties 探测才能给出可归因的消息。
    $catalogDirProperty = $shippedGame.PSObject.Properties['catalogDir']
    $catalogDir = if ($null -ne $catalogDirProperty) { [string]$catalogDirProperty.Value } else { '' }
    if ([string]::IsNullOrWhiteSpace($catalogDir)) {
        throw "Shipped game '$($shippedGame.gameId)' has no catalogDir in shipped-games.json."
    }
    foreach ($bundleFile in @(
        'catalog.json', 'device-profile.json', 'recording-profile.json',
        'game-profile.json', 'mapping.json')) {
        $embeddedPayloads.Add("config/catalogs/dev/$catalogDir/$bundleFile") | Out-Null
    }
    foreach ($optionalFile in @('depth-profile.json', 'camera-binding.json')) {
        if (Test-Path -LiteralPath (Join-Path $dataMinerRoot "config\catalogs\dev\$catalogDir\$optionalFile") -PathType Leaf) {
            $embeddedPayloads.Add("config/catalogs/dev/$catalogDir/$optionalFile") | Out-Null
        }
    }
}
if ($embeddedPayloads.Count -eq 0) {
    throw 'No embedded catalog payloads were found; the release-surface check would silently pass.'
}
$uniquePayloads = @($embeddedPayloads | Sort-Object -Unique)
# One `git ls-files` for the whole config tree, then set membership. Per-path
# `--error-unmatch` would mean reading $LASTEXITCODE, whose interaction with
# $ErrorActionPreference='Stop' changed in PowerShell 7.3
# ($PSNativeCommandUseErrorActionPreference) — a non-zero git exit would surface
# as a raw git error instead of the message below.
$trackedOutput = & git -C $dataMinerRoot ls-files -- 'config'
if ($LASTEXITCODE -ne 0 -or $null -eq $trackedOutput) {
    throw 'Unable to list git-tracked files under DataMiner/config; the embedded-payload check cannot run.'
}
$trackedPaths = [System.Collections.Generic.HashSet[string]]::new(
    [string[]]@($trackedOutput | ForEach-Object { $_.Trim() -replace '\\', '/' }),
    [System.StringComparer]::OrdinalIgnoreCase)
$missingOnDisk = @()
$untracked = @()
foreach ($relative in $uniquePayloads) {
    $absolute = Join-Path $dataMinerRoot ($relative -replace '/', [System.IO.Path]::DirectorySeparatorChar)
    if (-not (Test-Path -LiteralPath $absolute -PathType Leaf)) {
        $missingOnDisk += $relative
    }
    elseif (-not $trackedPaths.Contains($relative)) {
        $untracked += $relative
    }
}
if ($missingOnDisk.Count -gt 0) {
    throw "Embedded catalog payload is absent from the working tree: $($missingOnDisk -join ', ')"
}
if ($untracked.Count -gt 0) {
    throw "Embedded catalog payload is not tracked by git (see docs/54 6.3): $($untracked -join ', ')"
}
Write-Host ("PASS: {0} embedded catalog payloads are committed files." -f $uniquePayloads.Count)
$windowsManifestPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\windows\app.manifest'
$buildScriptPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\build.rs'
if (-not (Test-Path -LiteralPath $windowsManifestPath -PathType Leaf)) {
    throw 'The Windows application manifest is missing.'
}
$windowsManifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $windowsManifestPath
$buildScript = Get-Content -Raw -Encoding UTF8 -LiteralPath $buildScriptPath
if ($windowsManifest -notmatch '<requestedExecutionLevel\s+level="requireAdministrator"\s+uiAccess="false"\s*/>' -or
    $buildScript -notmatch '(?s)else\s*\{\s*WINDOWS_APP_MANIFEST\s*\}' -or
    $buildScript -notmatch '\.app_manifest\(manifest\)') {
    throw 'DataMiner must embed a Windows manifest that requires administrator execution.'
}
if (-not $buildScript.Contains('cargo:rerun-if-env-changed=DATAMINER_RAW_DEMO_BUILD')) {
    throw 'The raw-demo execution-level switch must invalidate cached Cargo build-script output.'
}
$tauriConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $tauriConfigPath | ConvertFrom-Json
if ($tauriConfig.bundle.windows.nsis.installerIcon -ne 'icons/icon.ico') {
    throw 'The NSIS installer must explicitly embed the DataMiner icon.'
}
# 选窗谓词对所有游戏统一：一次只读的 Tool Help 进程快照给出 PID -> exe 名
# （docs/55 §3.1）。快照不碰任何目标进程，是整条身份阶梯的地板。
if ($windowTrackerSource -notmatch 'CreateToolhelp32Snapshot' -or
    $windowTrackerSource -notmatch 'TH32CS_SNAPPROCESS') {
    throw 'Window discovery must keep using the read-only Tool Help process snapshot.'
}
# 共识「只读观察面止于 QUERY_LIMITED」的机器强制（docs/55 §2.3 / §7.1）。
# 想越权拿内存读写或建线程的权限，CI 先红——这条断言就是那条边界本身。
foreach ($forbiddenAccessRight in @(
        'PROCESS_VM_READ', 'PROCESS_VM_WRITE', 'PROCESS_VM_OPERATION',
        'PROCESS_CREATE_THREAD', 'PROCESS_DUP_HANDLE')) {
    if ($clientSources -match [regex]::Escape($forbiddenAccessRight)) {
        throw "The client may only ever open game processes with PROCESS_QUERY_LIMITED_INFORMATION; found $forbiddenAccessRight."
    }
}
# `name-only:<exe>` 伪指纹已废除（docs/55 §1.3），但**不为它加静态断言**：
# 一条 "客户端源码里不得出现 name-only:" 的规则分不清「产生该格式」与「断言拒收
# 该格式」——catalog.rs 里正有一条故意保留的拒收测试用它做输入，实测这条断言
# 第一次跑就打在那条测试上。真正的保护是三层实物：产生它的函数已删除、
# catalog validator 拒收非 SHA-256 身份钉、服务端 sha256_digest 域同样拒收。
$devPackage = Get-Content -Raw -Encoding UTF8 -LiteralPath $devPackagePath
$installerBuild = Get-Content -Raw -Encoding UTF8 -LiteralPath $installerBuildPath
if ($devPackage -notmatch 'tauri dev --features dev-catalog' -or
    $productionFeatures.Count -ne 1 -or
    $productionFeatures[0] -ne 'depth-lab' -or
    $deliveredCatalogFeatures.Count -ne 1 -or
    $deliveredCatalogFeatures[0] -ne 'depth-lab' -or
    $defaultDevFeatures.Count -ne 1 -or
    $defaultDevFeatures[0] -ne 'dev-catalog') {
    throw 'Online builds must include only formal Depth; development launch and installer builds must opt in to dev-catalog explicitly.'
}
if ($installerBuild -notmatch '\$AllGamesDev' -or
    $installerBuild -notmatch '\$DeliveredCatalog' -or
    $installerBuild -notmatch 'Get-WindowsShippedClientBuildCase[^\r\n]*-Flavor default_dev' -or
    $installerBuild -notmatch 'Get-WindowsShippedClientBuildCase[^\r\n]*-Flavor production' -or
    $installerBuild -notmatch 'Get-WindowsShippedClientBuildCase[^\r\n]*-Flavor delivered_catalog' -or
    $installerBuild -notmatch 'Get-WindowsShippedClientBuildCase[^\r\n]*-Flavor all_games_dev') {
    throw 'The installer must consume every build flavor from the shared build matrix.'
}
if ($installerBuild -notmatch '(?s)\$duplicateAllGamesProcessNames\s*=.*?Group-Object.*?\.Count\s+-gt\s+1') {
    throw 'The All Games DEV installer must fail closed when supported process names are duplicated.'
}
if (-not $installerBuild.Contains("catalog_mode = if (`$Production -or `$DeliveredCatalog) { `$null } else { 'dev_non_trainable' }") -or
    -not $installerBuild.Contains("catalog_provenance = if (`$Production -or `$DeliveredCatalog) { 'delivered'") -or
    -not $installerBuild.Contains("session_contract = if (`$Production -or `$DeliveredCatalog) { 'dm-session-ingest'") -or
    -not $installerBuild.Contains("catalog_sync = if (`$Production -or `$DeliveredCatalog) { 'required'") -or
    -not $installerBuild.Contains('cargo_features = $cargoFeatures') -or
    -not $installerBuild.Contains('depth_resource_destinations = $depthResourceDestinations') -or
    -not $installerBuild.Contains('supported_process_names') -or
    -not $installerBuild.Contains("requested_execution_level = 'requireAdministrator'") -or
    -not $installerBuild.Contains('Assert-ReleaseApplicationManifest -Path $releaseBinary -ExpectedIdentity $config.identifier') -or
    -not $installerBuild.Contains("'read_only_observation_degrades_when_denied'")) {
    throw 'Installer publication manifests must disclose catalog provenance, session contract, sync policy, and Cargo features.'
}

foreach ($package in @('capture-lab', 'dataminer-client')) {
    $tree = & cargo tree --manifest-path (Join-Path $dataMinerRoot 'Cargo.toml') -p $package -e normal --prefix none
    if ($LASTEXITCODE -ne 0) {
        throw "cargo tree failed for $package."
    }
    $dependencyViolations = $tree | Select-String -SimpleMatch -Pattern @('ffmpeg', 'ffprobe', 'x264')
    if ($dependencyViolations) {
        throw "Forbidden release dependency in ${package}: $($dependencyViolations -join ', ')"
    }
}

# The recorder now compiles the same process-loopback primitive as capture-lab.
# Only explicitly authored profiles (currently COD DEV non-trainable) enable it.
# Pin the feature wiring and absence of a device/system-wide fallback: drift changes
# what the signed client can capture even before a catalog is updated.
$clientFeatureTree = & cargo tree --manifest-path (Join-Path $dataMinerRoot 'Cargo.toml') -p dataminer-client -e features --prefix none
if ($LASTEXITCODE -ne 0) {
    throw 'cargo feature tree failed for dataminer-client.'
}
$clientFeatureText = $clientFeatureTree -join "`n"
if (-not $clientFeatureText.Contains('capture-core feature "process-loopback-audio"') -or
    -not $clientFeatureText.Contains('windows feature "Win32_Media_Audio"') -or
    $clientFeatureText.Contains('capture-core feature "audio-lab"')) {
    throw 'dataminer-client must compile process-loopback-audio directly without the capture-lab feature.'
}
$audioSource = Get-Content -LiteralPath (Join-Path $dataMinerRoot 'crates\capture-core\src\audio.rs') -Raw
foreach ($required in @(
    'AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK',
    'PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE',
    'VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK'
)) {
    if (-not $audioSource.Contains($required)) {
        throw "Process-loopback release invariant is missing: $required"
    }
}
foreach ($forbidden in @('IMMDeviceEnumerator', 'GetDefaultAudioEndpoint', 'MMDeviceEnumerator')) {
    if ($audioSource.Contains($forbidden)) {
        throw "Device/system audio fallback is forbidden in the release client: $forbidden"
    }
}

$releaseBinary = Join-Path $dataMinerRoot 'target\release\capture-lab.exe'
if ($RequireReleaseBinary -and -not (Test-Path -LiteralPath $releaseBinary -PathType Leaf)) {
    throw "Release binary is required but missing: $releaseBinary"
}
$releaseBinaries = @()
$requestedReleaseBinary = $null
if (Test-Path -LiteralPath $releaseBinary -PathType Leaf) {
    $releaseBinaries += $releaseBinary
}
if ($ReleaseBinaryPath) {
    $resolvedBinary = (Resolve-Path -LiteralPath $ReleaseBinaryPath).Path
    $relativeReleaseRoot = if ($AllowAllGamesDev) {
        'target\all-games-dev\release'
    } elseif ($AllowDeliveredCatalog) {
        'target\delivered-catalog\release'
    } else {
        'target\release'
    }
    $releaseRoot = if ($ExpectedTargetRoot) {
        $resolvedTargetRoot = [System.IO.Path]::GetFullPath($ExpectedTargetRoot).TrimEnd('\')
        $resolvedTargetRoot = Join-Path $resolvedTargetRoot 'release'
        [System.IO.Path]::GetFullPath($resolvedTargetRoot).TrimEnd('\') + '\'
    } else {
        [System.IO.Path]::GetFullPath((Join-Path $dataMinerRoot $relativeReleaseRoot)).TrimEnd('\') + '\'
    }
    $requestedReleaseBinary = Assert-PathUnderDirectory `
        -Path $resolvedBinary `
        -Directory $releaseRoot `
        -Description 'Release binary'
    $releaseBinaries += $resolvedBinary
}

foreach ($binary in $releaseBinaries | Select-Object -Unique) {
    & (Join-Path $PSScriptRoot 'verify-nvidia-optimus-export.ps1') -BinaryPath $binary
    $bytes = [System.IO.File]::ReadAllBytes($binary)
    $ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
    $binaryForbiddenTokens = @($forbidden + $releaseBinaryOnlyForbidden)
    if ($binary -eq $requestedReleaseBinary -and
        [IO.Path]::GetFileName($binary) -eq 'dataminer-client.exe' -and
        -not ($AllowAllGamesDev -or $AllowDefaultDev)) {
        $binaryForbiddenTokens = @(Get-WindowsOnlineClientForbiddenCatalogTokens -Tokens $binaryForbiddenTokens)
    }
    foreach ($token in $binaryForbiddenTokens) {
        if ($ascii.Contains($token)) {
            throw "Release binary contains forbidden token '$token': $binary"
        }
    }
    if (($AllowAllGamesDev -or $AllowDefaultDev) -and $binary -eq $requestedReleaseBinary) {
        foreach ($token in $allGamesTokens) {
            if (-not $ascii.Contains($token)) {
                throw "Shipped DEV client binary is missing required token '$token': $binary"
            }
        }
    }
}

$genericDsvTargetValidator = Join-Path $dataMinerRoot 'tools\game-adaptation\Test-GenericD3D11DsvDiscoveryTargets.ps1'
$gta5StaticInstallSurfaceValidator = Join-Path $dataMinerRoot 'tools\game-adaptation\Test-Gta5LegacyStaticInstallSurface.ps1'
$mafia3D6LocalProfileValidator = Join-Path $dataMinerRoot 'tools\game-adaptation\Test-Mafia3D6LocalProfileEvidence.ps1'
$genericDsvSceneCreator = Join-Path $dataMinerRoot 'tools\game-adaptation\New-GenericD3D11RepresentativeSceneAttestation.ps1'
$genericDsvSceneValidator = Join-Path $dataMinerRoot 'tools\game-adaptation\Test-GenericD3D11RepresentativeSceneAttestation.ps1'
$genericDsvArmSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $dataMinerRoot 'tools\game-adaptation\Set-GenericD3D11DsvDiscovery.ps1')
$genericDsvInjectionGuard = Join-Path $dataMinerRoot 'tools\game-adaptation\Invoke-GenericD3D11DsvInjection.ps1'
$genericDsvInjectionSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $genericDsvInjectionGuard
$genericDsvSceneCreatorSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $genericDsvSceneCreator
$genericDsvSceneValidatorSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $genericDsvSceneValidator
$depthInjectorSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $dataMinerRoot 'tools\depth-lab-probe\src\bin\dm-depth-inject.rs')
$dataMinerCiWorkflowSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $dataMinerCiWorkflowPath
$vulkanDiscoveryContractPath = Join-Path $dataMinerRoot 'tools\depth-lab\src\vulkan_attachment_discovery.rs'
$vulkanDiscoveryContractSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $vulkanDiscoveryContractPath
foreach ($requiredBinding in @(
    "-GameId `$GameId",
    "-BuildId `$ExpectedBuildId",
    '$targetReport.selected_target',
    '$rights.records[0].catalog_game_ids',
    '$d6.build_id',
    '$target.protectionGate.runtimeEvidenceSchema',
    '$runtime.process.start_time_utc',
    '$liveStartTimeUtc',
    'RepresentativeSceneAttestationPath',
    'ExpectedRepresentativeSceneAttestationSha256',
    'Test-GenericD3D11RepresentativeSceneAttestation.ps1',
    'representative_scene_attestation_sha256'
)) {
    if (-not $genericDsvArmSource.Contains($requiredBinding)) {
        throw "Generic D3D11 discovery arm lost target-binding wiring: $requiredBinding"
    }
}
foreach ($requiredGuard in @(
    '-ExpectedCollectorSha256 $collectorHash',
    '-ExpectedConfigSha256 $expectedConfigHash',
    '-RequireNoLoadedAntiCheat',
    '-ExpectedStartTimeUtc $armStartTimeUtc',
    '--expected-start-time-filetime',
    'ExpectedInjectorSha256',
    '--print-guard-contract',
    'Assert-ArmedConfigIdentity',
    'RepresentativeSceneAttestationPath',
    'ExpectedRepresentativeSceneAttestationSha256',
    'Assert-RepresentativeSceneAttestation',
    'representative_scene_attestation_sha256'
)) {
    if (-not $genericDsvInjectionSource.Contains($requiredGuard)) {
        throw "Generic D3D11 injection guard lost a fail-closed binding: $requiredGuard"
    }
}
if ($genericDsvInjectionSource.Contains('--allow-existing')) {
    throw 'Generic D3D11 injection guard must never allow resident-probe reuse.'
}
foreach ($requiredSceneControl in @(
    'ConfirmRepresentativeOfflineSinglePlayer',
    '[IO.FileMode]::CreateNew',
    '[Security.Cryptography.RandomNumberGenerator]::Create()',
    'generic-d3d11-representative-scene-attestation/v1',
    'representative_offline_single_player'
)) {
    if (-not ($genericDsvSceneCreatorSource.Contains($requiredSceneControl) -or
        $genericDsvSceneValidatorSource.Contains($requiredSceneControl))) {
        throw "Generic D3D11 representative-scene gate lost a required control: $requiredSceneControl"
    }
}
foreach ($requiredSceneBinding in @(
    'ExpectedAttestationSha256',
    'ExpectedRuntimeEvidenceSha256',
    'Assert-BoundRuntimeEvidence',
    "'runtime.captured_utc'",
    "'runtime.process.process_id'",
    "'runtime.graphics.hits'",
    "'runtime.blocking'",
    'ExpectedDiscoveryId',
    'ExpectedOutputLeaf',
    'ExpectedProcessStartTimeUtc',
    'ExpectedWindowClass',
    'ExpectedClientWidth',
    'ExpectedClientHeight',
    'MaximumAgeSeconds',
    'Assert-ExactProperties'
)) {
    if (-not $genericDsvSceneValidatorSource.Contains($requiredSceneBinding)) {
        throw "Generic D3D11 representative-scene validator lost a fail-closed binding: $requiredSceneBinding"
    }
}
if ($genericDsvSceneValidatorSource.Contains('ExpectedRuntimeCapturedUtc')) {
    throw 'Generic D3D11 representative-scene validator must derive captured_utc from hash-bound runtime JSON.'
}
$freshRuntimeValidationIndex = $genericDsvInjectionSource.IndexOf("-Label 'fresh pre-injection runtime gate evidence'")
if ($freshRuntimeValidationIndex -lt 0) {
    throw 'Generic D3D11 injection guard lost fresh runtime evidence validation.'
}
$sceneRecheckAfterFreshIndex = $genericDsvInjectionSource.IndexOf('Assert-RepresentativeSceneAttestation', $freshRuntimeValidationIndex)
$configRecheckAfterFreshIndex = $genericDsvInjectionSource.IndexOf('Assert-ArmedConfigIdentity', $freshRuntimeValidationIndex)
if ($sceneRecheckAfterFreshIndex -lt 0 -or $configRecheckAfterFreshIndex -lt 0) {
    throw 'Generic D3D11 injection guard must recheck attestation and config after fresh runtime evidence.'
}
foreach ($requiredInjectorBinding in @(
    '--expected-start-time-filetime',
    'dm-depth-inject-guard/v2:start-filetime+loaded-anticheat-module-recheck+single-canonical-stdout',
    'validate_process_start_filetime(process, expected)?',
    'validate_no_loaded_anti_cheat_modules(pid)?'
)) {
    if (-not $depthInjectorSource.Contains($requiredInjectorBinding)) {
        throw "Depth injector lost exact-process-instance enforcement: $requiredInjectorBinding"
    }
}
foreach ($requiredCiExecution in @(
    'cargo test -p dm-depth-probe --bin dm-depth-inject',
    'cargo run --quiet --locked -p dm-depth-probe --bin dm-depth-inject -- --print-guard-contract',
    'test "$injector_contract" = "dm-depth-inject-guard/v2:start-filetime+loaded-anticheat-module-recheck+single-canonical-stdout"'
)) {
    if (-not $dataMinerCiWorkflowSource.Contains($requiredCiExecution)) {
        throw "DataMiner CI lost executable coverage for the guarded depth injector: $requiredCiExecution"
    }
}
if ($dataMinerCiWorkflowSource.Contains('./target/debug/dm-depth-inject.exe --print-guard-contract')) {
    throw 'DataMiner CI must not bypass an external CARGO_TARGET_DIR when executing the injector contract.'
}

# Vulkan remains an output-only, synthetic contract skeleton. Formal builds include depth-lab, so
# this guard must fail before a future layer runtime or client/probe reachability can ride that
# existing feature into a shipped artifact without a separate reviewed DEV-only boundary.
foreach ($requiredVulkanContractControl in @(
    'pub const VULKAN_DISCOVERY_BACKEND_STATE: &str = "unavailable/discovery_contract";',
    '#[derive(Debug, Clone, PartialEq, Eq, Serialize)]',
    'Vulkan discovery contract cannot claim runtime evidence, completion, authoring or readiness',
    'RuntimeBackendUnavailable',
    'pub fn observe_fixture',
    'validate_fixture_input_shape'
)) {
    if (-not $vulkanDiscoveryContractSource.Contains($requiredVulkanContractControl)) {
        throw "Vulkan discovery contract lost an unavailable/output-only control: $requiredVulkanContractControl"
    }
}
if ($vulkanDiscoveryContractSource -match '(?s)#\[derive\((?=[^\)]*Deserialize)[^\)]*\)\]\s*#\[serde\(deny_unknown_fields\)\]\s*pub struct VulkanAttachmentDiscoveryReport' -or
    $vulkanDiscoveryContractSource -match '(?s)#\[derive\((?=[^\)]*Deserialize)[^\)]*\)\]\s*#\[serde\(deny_unknown_fields\)\]\s*pub struct VulkanPresentObservation') {
    throw 'Vulkan discovery report/present observations must not expose unbounded direct Deserialize.'
}
$vulkanForbiddenReachabilityTokens = @(
    'vulkan_attachment_discovery',
    'VulkanAttachmentDiscovery',
    'VULKAN_DISCOVERY_BACKEND_STATE',
    'VK_LAYER_',
    'vkNegotiateLoaderLayerInterfaceVersion'
)
$vulkanReachabilityRoots = @(
    (Join-Path $dataMinerRoot 'apps\client\src-tauri\src'),
    (Join-Path $dataMinerRoot 'tools\depth-lab-probe\src')
)
foreach ($root in $vulkanReachabilityRoots) {
    foreach ($sourceFile in Get-ChildItem -LiteralPath $root -Recurse -File -Filter '*.rs') {
        $sourceText = Get-Content -Raw -Encoding UTF8 -LiteralPath $sourceFile.FullName
        foreach ($token in $vulkanForbiddenReachabilityTokens) {
            if ($sourceText.Contains($token)) {
                throw "Shipped client/probe source unexpectedly reaches Vulkan discovery runtime token '$token': $($sourceFile.FullName)"
            }
        }
    }
}
& $genericDsvTargetValidator -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Generic D3D11 discovery target negative regression suite failed.'
}
& $genericDsvTargetValidator | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Generic D3D11 discovery target registry/control-plane reconciliation failed.'
}
& $gta5StaticInstallSurfaceValidator -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'GTA V Legacy static install-surface negative regression suite failed.'
}
& $genericDsvInjectionGuard -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Generic D3D11 guarded injection negative regression suite failed.'
}
& $genericDsvSceneValidator -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Generic D3D11 representative-scene attestation negative regression suite failed.'
}
& $genericDsvSceneCreator -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Generic D3D11 representative-scene create-new regression suite failed.'
}
& $mafia3D6LocalProfileValidator -SelfTest | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Mafia III local D6 input/video profile negative regression suite failed.'
}

Write-Host 'PASS: HWND-only WGC, GPU-only conversion, and hardware-only encoder release surface verified.'

[CmdletBinding()]
param(
    [switch]$AllGamesDev,
    # 不内嵌开发目录，从生产服务验签下发 catalog，并把符合
    # neutral contract 的 session 送入线上 ingest。它仍未做 Authenticode，
    # 因此文案、文件名和 manifest 都必须明确标记 UNSIGNED。
    [switch]$DeliveredCatalog,
    # 生产（Authenticode 已签名）风味。目前**跑不通**——见下方 preflight：代码签名
    # 证书与可信时间戳策略都还没有（windows-unsigned-dev-installer.md §5）。这个开关
    # 存在的意义是让整条流水线的标识、断言与文案都由风味驱动，而不是把「未签名非
    # 生产」焊死在十几个地方；证书到位那天要补的只有 preflight 与签名步骤本身。
    [switch]$Production,
    # Directory produced by the independent Depth probe release workflow. It must contain
    # artifact-lock.json, the runtime manifest, and all four locked binaries. The historical
    # parameter name is retained for callers, but an arbitrary loose tools directory is rejected.
    [string]$PinnedDepthToolsDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$selectedFlavorSwitches = @($Production, $DeliveredCatalog, $AllGamesDev)
if (@($selectedFlavorSwitches | Where-Object { [bool]$_ }).Count -gt 1) {
    throw '-Production, -DeliveredCatalog, and -AllGamesDev are mutually exclusive.'
}
if ($PinnedDepthToolsDirectory -and -not ($Production -or $DeliveredCatalog -or $AllGamesDev)) {
    throw '-PinnedDepthToolsDirectory is only valid for a Depth-enabled bundle.'
}

# Assert-Unsigned below needs Get-AuthenticodeSignature; autoloading it is not
# dependable on hosted Windows runners. Fails closed if the cmdlet stays unavailable.
. (Join-Path $PSScriptRoot 'ensure-authenticode.ps1')
. (Join-Path $PSScriptRoot 'windows-build-helpers.ps1')

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$configPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.conf.json'
$releaseConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.release.conf.json'
$allGamesDevConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.all-games-dev.conf.json'
$deliveredCatalogConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.delivered-catalog.conf.json'
$generatedTauriConfigPath = $null
$hookPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\windows\installer-hooks.nsh'
$distRoot = Join-Path $dataMinerRoot 'apps\client\dist'
$configuredCargoTargetDir = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
$cargoTargetRoot = Resolve-WindowsCargoTargetDirectory `
    -DataMinerRoot $dataMinerRoot `
    -ConfiguredTargetDirectory $configuredCargoTargetDir `
    -Flavor default_dev
$releaseRoot = Join-Path $cargoTargetRoot 'release'
$releaseBinary = Join-Path $releaseRoot 'dataminer-client.exe'
$bundleRoot = Join-Path $releaseRoot 'bundle\nsis'
$releaseSurfaceScript = Join-Path $dataMinerRoot 'tools\verify-release-surface.ps1'
$productionConfigPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\tauri.production.conf.json'
$defaultDevBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor default_dev
$productionBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor production
$deliveredCatalogBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor delivered_catalog
$allGamesBuildCase = Get-WindowsShippedClientBuildCase -DataMinerRoot $dataMinerRoot -Flavor all_games_dev
$cargoFeatures = [string[]]@($defaultDevBuildCase.Features)
$allGamesGameIds = @(
    'call-of-duty-steam-release-test',
    'bioshock-infinite-steam-e2e',
    'wolfenstein2-steam',
    'age-of-empires-4-steam',
    'wuchang-fallen-feathers-steam',
    'halo-infinite-steam-release-test',
    'euro-truck-simulator-2-steam',
    'a-plague-tale-innocence-steam-release-test',
    'assassins-creed-iii-remastered-steam',
    'fallout4-steam-release-test',
    'hades-ii-steam-release-test',
    'cyberpunk-2077-steam',
    'total-war-warhammer-iii-steam-release-test',
    'halo-reach-mcc-s0',
    'elden-ring-nightreign-steam-release-test',
    'nioh2-steam',
    'mount-and-blade-2-bannerlord-steam',
    'deathloop-pc',
    'atomic-heart-pc',
    'doom-eternal-pc',
    'resident-evil-village-pc',
    'avatar-frontiers-of-pandora-pc',
    'dying-light-pc',
    'far-cry-5-pc',
    'forza-horizon-5-pc',
    'gta5-legacy-pc',
    'indiana-jones-great-circle-pc',
    'titanfall-2-pc',
    'prey-2017-pc',
    'red-dead-redemption-2-pc',
    'starfield-pc',
    'kingdom-come-deliverance-2-steam'
)
$allGamesProcessNames = @(
    'cod.exe'
    'cod22-cod.exe'
    'cod23-cod.exe'
    'cod24-cod.exe'
    'sp22-cod.exe'
    'ModernWarfare.exe'
    'BioShockInfinite.exe'
    'NewColossus_x64vk.exe'
    'RelicCardinal.exe'
    'Project_Plague-Win64-Shipping.exe'
    'HaloInfinite.exe'
    'eurotrucks2.exe'
    'APlagueTaleInnocence_x64.exe'
    'ACIII.exe'
    'Fallout4.exe'
    'Hades2.exe'
    'Cyberpunk2077.exe'
    'Warhammer3.exe'
    'MCC-Win64-Shipping.exe'
    'mcclauncher.exe'
    'nightreign.exe'
    'nioh2.exe'
    'Bannerlord.exe'
    'Deathloop.exe'
    'AtomicHeart-Win64-Shipping.exe'
    'DOOMEternalx64vk.exe'
    'DOOMSandBox64vk.exe'
    're8.exe'
    'afop.exe'
    'DyingLightGame.exe'
    'FarCry5.exe'
    'ForzaHorizon5.exe'
    'GTA5.exe'
    'TheGreatCircle.exe'
    'Titanfall2.exe'
    'Prey.exe'
    'RDR2.exe'
    'Starfield.exe'
    'KingdomCome.exe'
)
$duplicateAllGamesProcessNames = @($allGamesProcessNames |
    Group-Object |
    Where-Object { $_.Count -gt 1 } |
    ForEach-Object { $_.Name })
if ($duplicateAllGamesProcessNames.Count -gt 0) {
    throw "All Games DEV process names must be unique: $($duplicateAllGamesProcessNames -join ', ')"
}
# 必备 token 集从上面两份字面量数组派生——两份都被 tools/check-shipped-games.mjs
# 与 shipped-games.json / 各 game-profile.json 逐项对账，因此这里不可能再漂移
# （此前是第三份手抄 token 对名单：#822 的 4 款 pc-local 从未被加进来）。
$allGamesRequiredTokens = [string[]]@($allGamesGameIds + $allGamesProcessNames)
$bundleFlavor = 'default_dev'
$publishedLabel = 'UNSIGNED_NON_PRODUCTION'
# 前端徽章的来源（apps/client/src/build-flavor.ts）。构建末尾会断言产物真实的
# Authenticode 状态与它一致，所以界面不可能声称自己是它不是的东西。
$frontendFlavor = 'unsigned-dev'
if ($Production) {
    $bundleFlavor = 'production'
    $publishedLabel = ''
    $frontendFlavor = 'production'
    $cargoFeatures = [string[]]@($productionBuildCase.Features)
    $releaseConfigPath = $productionConfigPath
    $cargoTargetRoot = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $configuredCargoTargetDir `
        -Flavor production
    $releaseRoot = Join-Path $cargoTargetRoot 'release'
    $releaseBinary = Join-Path $releaseRoot 'dataminer-client.exe'
    $bundleRoot = Join-Path $releaseRoot 'bundle\nsis'
}
if ($DeliveredCatalog) {
    $bundleFlavor = 'delivered_catalog'
    $publishedLabel = 'DELIVERED_CATALOG_UNSIGNED'
    $frontendFlavor = 'unsigned-delivered'
    $cargoFeatures = [string[]]@($deliveredCatalogBuildCase.Features)
    $releaseConfigPath = $deliveredCatalogConfigPath
    $hookPath = Join-Path $dataMinerRoot 'apps\client\src-tauri\windows\installer-hooks-delivered.nsh'
    $cargoTargetRoot = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $configuredCargoTargetDir `
        -Flavor delivered_catalog
    $releaseRoot = Join-Path $cargoTargetRoot 'release'
    $releaseBinary = Join-Path $releaseRoot 'dataminer-client.exe'
    $bundleRoot = Join-Path $releaseRoot 'bundle\nsis'
}
if ($AllGamesDev) {
    # All Games DEV is still an unsigned, non-trainable package. The dedicated
    # target directory prevents its installer from overwriting the default DEV
    # artifacts. This case adds local-catalog-only and depth-lab on top of
    # dev-catalog (the game roster itself embeds with dev-catalog alone). The
    # shared helper is also consumed by CI, so changing the feature set changes
    # the release clippy coverage in the same commit.
    $cargoFeatures = [string[]]@($allGamesBuildCase.Features)
    $bundleFlavor = 'all_games_dev'
    $publishedLabel = 'ALL_GAMES_DEV_UNSIGNED_NON_PRODUCTION'
    # 专属 config 保留 All Games DEV 的构建 overlay；正式 Depth 的动态
    # bundle.resources 与另外两个线上口味共用下方同一段生成逻辑。
    $releaseConfigPath = $allGamesDevConfigPath
    $cargoTargetRoot = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $configuredCargoTargetDir `
        -Flavor all_games_dev
    $releaseRoot = Join-Path $cargoTargetRoot 'release'
    $releaseBinary = Join-Path $releaseRoot 'dataminer-client.exe'
    $bundleRoot = Join-Path $releaseRoot 'bundle\nsis'

}

# Resource source paths must follow the resolved flavor target (including a
# custom large-disk root). Generate the final overlay beside that target for
# every Depth-enabled bundle instead of hard-coding repository target paths.
#
# Online flavors ship only the formal runtime set. All Games DEV additionally
# carries the local-only CP2077 extraction/recipe tools; those must never leak
# into the fixed delivery channel.
$depthEnabledBundle = @($cargoFeatures) -contains 'depth-lab'
$depthResourceDestinations = [string[]]@()
if ($depthEnabledBundle) {
    $depthResourceEntries = @(
        @{ Source = (Join-Path $releaseRoot 'dm_depth_probe.dll'); Destination = 'depth-lab/dm_depth_probe.dll' },
        @{ Source = (Join-Path $releaseRoot 'dm-depth-inject.exe'); Destination = 'depth-lab/dm-depth-inject.exe' },
        @{ Source = (Join-Path $releaseRoot 'dm-depth-launch.exe'); Destination = 'depth-lab/dm-depth-launch.exe' },
        @{ Source = (Join-Path $releaseRoot 'dm-depth-probe-runtime.json'); Destination = 'depth-lab/dm-depth-probe-runtime.json' }
    )
    if ($AllGamesDev) {
        $depthResourceEntries += @(
            @{ Source = (Join-Path $releaseRoot 'dm-depth-extract.exe'); Destination = 'dm-depth-extract.exe' },
            @{ Source = (Join-Path $dataMinerRoot 'tools\pr735-cp2077\Set-Cp2077CaptureSettings.ps1'); Destination = 'depth-lab/Set-Cp2077CaptureSettings.ps1' },
            @{ Source = (Join-Path $dataMinerRoot 'tools\pr735-cp2077\Test-Cp2077CaptureSettings.ps1'); Destination = 'depth-lab/Test-Cp2077CaptureSettings.ps1' },
            @{ Source = (Join-Path $dataMinerRoot 'tools\pr735-cp2077\package-local-fivepiece.py'); Destination = 'depth-lab/package-local-fivepiece.py' }
        )
    }
    $depthResourceDestinations = [string[]]@(
        $depthResourceEntries | ForEach-Object { [string]$_.Destination }
    )
    $baseDepthConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $releaseConfigPath | ConvertFrom-Json
    $bundleResourceEntries = $depthResourceEntries + @(
        @{ Source = (Join-Path $dataMinerRoot 'tools\depth-lab\THIRD-PARTY-NOTICES.txt'); Destination = 'depth-lab/THIRD-PARTY-NOTICES.txt' }
    )
    $depthResources = New-TauriBundleResourceMap -Entries $bundleResourceEntries
    $generatedConfigName = "tauri.$($bundleFlavor.Replace('_', '-')).generated.conf.json"
    $generatedTauriConfigPath = Join-Path $cargoTargetRoot $generatedConfigName
    New-Item -ItemType Directory -Path $cargoTargetRoot -Force | Out-Null
    Write-TauriConfigWithBundleResources `
        -BaseConfig $baseDepthConfig `
        -ResourceMap $depthResources `
        -Path $generatedTauriConfigPath
    $releaseConfigPath = $generatedTauriConfigPath
}

function Invoke-NativeChecked {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $Command $($Arguments -join ' ')"
    }
}

# Keep direct script callers on the same Node/pnpm contract as the package
# lifecycle hooks, before Cargo or Tauri can start an expensive build.
Invoke-NativeChecked -Command 'node' -Arguments @(
    (Join-Path $dataMinerRoot 'tools\check-build-toolchain.mjs')
)

function Assert-Unsigned {
    param([Parameter(Mandatory = $true)][string]$Path)

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::NotSigned) {
        throw "Unsigned dev bundle expected NotSigned, got '$($signature.Status)' for $Path"
    }
}

function Assert-SignedAndTimestamped {
    param([Parameter(Mandatory = $true)][string]$Path)

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Production bundle expected a Valid Authenticode signature, got '$($signature.Status)' for $Path"
    }
    # 没有时间戳的签名会在证书过期当天全体失效。生产产物必须可验证到证书有效期
    # 之后，所以时间戳不是可选项。
    if ($null -eq $signature.TimeStamperCertificate) {
        throw "Production bundle is signed but not timestamped; it would stop validating when the certificate expires: $Path"
    }
}

# 产物与徽章的对账点。UI 上的风味是构建期烤进前端的，这里用产物的真实签名状态
# 去核对它——两者不符就让构建失败，而不是发一个自称生产的未签名包出去。
function Assert-AuthenticodeMatchesFlavor {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ($Production) { Assert-SignedAndTimestamped -Path $Path } else { Assert-Unsigned -Path $Path }
}

# StrictMode 下直接点属性访问缺失的键会抛错，而这里的整个目的就是**报告**缺什么。
function Get-JsonProperty {
    param($Object, [Parameter(Mandatory = $true)][string]$Name)

    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Assert-ProductionInputsAvailable {
    # 检查的是**内容**而不是文件存在：生产配置已经带着定好的时间戳策略进了仓库，
    # 缺的只有签名机制。若只判存在性，这个闸门从配置落库那天起就形同虚设了。
    $missing = New-Object System.Collections.Generic.List[string]

    if (-not (Test-Path -LiteralPath $productionConfigPath -PathType Leaf)) {
        $missing.Add("the production config itself: $productionConfigPath")
    } else {
        $productionConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $productionConfigPath | ConvertFrom-Json
        $windows = Get-JsonProperty (Get-JsonProperty $productionConfig 'bundle') 'windows'

        $thumbprint = Get-JsonProperty $windows 'certificateThumbprint'
        $signCommand = Get-JsonProperty $windows 'signCommand'
        if ([string]::IsNullOrWhiteSpace([string]$thumbprint) -and $null -eq $signCommand) {
            $missing.Add('bundle.windows.certificateThumbprint or bundle.windows.signCommand (the signing mechanism)')
        }
        if ([string]::IsNullOrWhiteSpace([string](Get-JsonProperty $windows 'timestampUrl'))) {
            $missing.Add('bundle.windows.timestampUrl')
        }
        if ([string]::IsNullOrWhiteSpace([string](Get-JsonProperty $windows 'digestAlgorithm'))) {
            $missing.Add('bundle.windows.digestAlgorithm')
        }
    }

    if ($missing.Count -eq 0) { return }

    throw @"
Production flavor is not buildable yet. Missing:
$(($missing | ForEach-Object { "  - $_" }) -join "`n")

The remaining blocker is the certificate itself (windows-unsigned-dev-installer.md 5).
Supply EITHER:
  - bundle.windows.certificateThumbprint, with the cert in the machine store, or
  - bundle.windows.signCommand, for a signing service (e.g. Azure Trusted Signing).
Never commit key material. The signing step reads it from the CI secret store or
an external signing service.

Already decided, do not re-litigate:
  - Timestamp policy: RFC3161 via DigiCert, SHA-256. Committed in the production
    config; rationale in windows-unsigned-dev-installer.md 5.2.
  - Fixed release channel and publishing infra: .github/workflows/dataminer-release.yml
    publishes immutably to R2 'dataminer-releases' behind a human approval gate.
  - Update trust root: minisign, verified by the client (docs/15 2.1). This is NOT
    a substitute for Authenticode.
"@
}

function Assert-NoTextToken {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string[]]$Tokens
    )

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
    $utf16 = [System.Text.Encoding]::Unicode.GetString($bytes)
    foreach ($token in $Tokens) {
        if ($ascii.IndexOf($token, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $utf16.IndexOf($token, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "Forbidden production token '$token' found in $Path"
        }
    }
}

function Assert-TextToken {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string[]]$Tokens
    )

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
    $utf16 = [System.Text.Encoding]::Unicode.GetString($bytes)
    foreach ($token in $Tokens) {
        if ($ascii.IndexOf($token, [System.StringComparison]::OrdinalIgnoreCase) -lt 0 -and
            $utf16.IndexOf($token, [System.StringComparison]::OrdinalIgnoreCase) -lt 0) {
            throw "Required bundle token '$token' is missing from $Path"
        }
    }
}

function Assert-WindowsGuiSubsystem {
    param([Parameter(Mandatory = $true)][string]$Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 256) {
        throw "Release PE is unexpectedly small: $Path"
    }
    $peOffset = [System.BitConverter]::ToInt32($bytes, 0x3c)
    $subsystemOffset = $peOffset + 24 + 68
    if ($peOffset -lt 0 -or $subsystemOffset + 2 -gt $bytes.Length) {
        throw "Release PE header is malformed: $Path"
    }
    $subsystem = [System.BitConverter]::ToUInt16($bytes, $subsystemOffset)
    if ($subsystem -ne 2) {
        throw "Windows GUI subsystem (2) required; got '$subsystem': $Path"
    }
}

function Assert-ReleaseApplicationManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedIdentity
    )

    $kitsBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (-not (Test-Path -LiteralPath $kitsBin -PathType Container)) {
        throw "Cannot audit the application manifest because the Windows SDK bin directory is missing: $kitsBin"
    }
    $mt = Get-ChildItem -LiteralPath $kitsBin -Directory |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName 'x64\mt.exe' } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1
    if (-not $mt) {
        throw "Cannot audit the application manifest because x64 mt.exe is missing under: $kitsBin"
    }

    $extractedManifest = "$Path.require-administrator-audit.manifest"
    try {
        if (Test-Path -LiteralPath $extractedManifest -PathType Leaf) {
            Remove-AuditManifestTemporaryFile -Path $extractedManifest
        }
        & $mt -nologo "-inputresource:$Path;#1" "-out:$extractedManifest"
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $extractedManifest -PathType Leaf)) {
            throw "mt.exe could not extract RT_MANIFEST #1 from: $Path"
        }
        [xml]$manifestXml = Get-Content -Raw -Encoding UTF8 -LiteralPath $extractedManifest
        $assemblyIdentity = $manifestXml.SelectSingleNode("//*[local-name()='assemblyIdentity' and @type='win32']")
        if ($null -eq $assemblyIdentity -or $assemblyIdentity.GetAttribute('name') -ne $ExpectedIdentity) {
            throw "Release client manifest identity does not match '$ExpectedIdentity': $Path"
        }
        $executionLevel = $manifestXml.SelectSingleNode("//*[local-name()='requestedExecutionLevel']")
        if ($null -eq $executionLevel -or
            $executionLevel.GetAttribute('level') -ne 'requireAdministrator' -or
            $executionLevel.GetAttribute('uiAccess') -ne 'false') {
            throw "Release client does not require administrator execution: $Path"
        }
    } finally {
        if (Test-Path -LiteralPath $extractedManifest -PathType Leaf) {
            Remove-AuditManifestTemporaryFile -Path $extractedManifest
        }
    }
}

function Get-PeDependencies {
    param([Parameter(Mandatory = $true)][string]$Path)

    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "Cannot audit PE dependencies because vswhere.exe is missing: $vswhere"
    }
    $visualStudio = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1)
    if (-not $visualStudio) {
        throw 'Cannot audit PE dependencies because no Visual Studio installation with the x64 VC toolchain was found.'
    }
    $toolsetRoot = Join-Path $visualStudio 'VC\Tools\MSVC'
    if (-not (Test-Path -LiteralPath $toolsetRoot -PathType Container)) {
        throw "Cannot audit PE dependencies because the MSVC toolset directory is missing: $toolsetRoot"
    }
    $toolset = Get-ChildItem -LiteralPath $toolsetRoot -Directory |
        Sort-Object Name -Descending |
        Select-Object -First 1
    if (-not $toolset) {
        throw "Cannot audit PE dependencies because no MSVC toolset was found under: $toolsetRoot"
    }
    $dumpbin = Join-Path $toolset.FullName 'bin\Hostx64\x64\dumpbin.exe'
    if (-not (Test-Path -LiteralPath $dumpbin -PathType Leaf)) {
        throw "Cannot audit PE dependencies because dumpbin.exe is missing: $dumpbin"
    }
    $output = & $dumpbin /dependents $Path
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin failed for release binary: $Path"
    }
    return @($output |
        ForEach-Object { $_.Trim() } |
        Where-Object { $_ -match '^[A-Za-z0-9._-]+\.dll$' } |
        Sort-Object -Unique)
}

$config = Get-Content -Raw -Encoding UTF8 -LiteralPath $configPath | ConvertFrom-Json
$targets = @($config.bundle.targets)
if ($config.productName -ne 'DataMiner') {
    throw 'The stable Windows product name must remain DataMiner; mark the dev flavor in artifact/UI metadata instead.'
}
if ($config.identifier -ne 'com.diogeneslab.dataminer') {
    throw 'The stable DataMiner app-data identifier changed unexpectedly.'
}
if (-not $config.bundle.active -or $targets.Count -ne 1 -or $targets[0] -ne 'nsis') {
    throw 'T13 requires bundle.active=true with NSIS as the only configured target.'
}
if ($config.bundle.windows.nsis.installMode -ne 'currentUser') {
    throw 'The unsigned dev installer must use currentUser mode and must not require elevation.'
}
if ($config.bundle.windows.webviewInstallMode.type -ne 'embedBootstrapper') {
    throw 'The Windows bundle must embed the official WebView2 bootstrapper.'
}
$releaseConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $releaseConfigPath | ConvertFrom-Json
if ($Production) {
    Assert-ProductionInputsAvailable
    if ($releaseConfig.bundle.shortDescription -match '(?i)unsigned|non-production') {
        throw 'Production bundle metadata must not describe the artifact as unsigned or non-production.'
    }
} elseif (-not $DeliveredCatalog -and $config.bundle.shortDescription -notmatch '(?i)unsigned.*non-production') {
    throw 'Bundle metadata must identify the artifact as unsigned and non-production.'
}
if ($DeliveredCatalog) {
    if ($releaseConfig.bundle.shortDescription -notmatch '(?i)unsigned.*online.*catalog') {
        throw 'Delivered Catalog metadata must identify the artifact as an unsigned online catalog collector.'
    }
    if ($releaseConfig.bundle.longDescription -notmatch '(?i)production account.*server-delivered catalog.*release channel.*ingest') {
        throw 'Delivered Catalog metadata must disclose every production service boundary it connects to.'
    }
}
if ($null -ne $releaseConfig.build.devUrl -or $null -ne $releaseConfig.build.beforeDevCommand) {
    throw 'Release config must remove both devUrl and beforeDevCommand from the compiled bundle context.'
}
if ($releaseConfig.build.beforeBuildCommand -ne 'pnpm build') {
    throw 'Release config beforeBuildCommand must be an audited production frontend build.'
}
$expectedFrontendDist = '../dist'
if ($releaseConfig.build.frontendDist -ne $expectedFrontendDist) {
    throw "Release config must explicitly embed '$expectedFrontendDist'; inherited frontendDist is not accepted."
}
$externalBinProperty = $config.bundle.PSObject.Properties['externalBin']
if ($null -ne $externalBinProperty -and @($externalBinProperty.Value).Count -gt 0) {
    throw 'The client installer must not bundle an external business/helper binary.'
}
if (-not (Test-Path -LiteralPath $hookPath -PathType Leaf)) {
    throw "NSIS warning hook is missing: $hookPath"
}
$hookExecutableLines = Get-Content -Encoding UTF8 -LiteralPath $hookPath |
    Where-Object { $_ -notmatch '^\s*;' }
if ($Production) {
    if (($hookExecutableLines -join "`n") -match 'UNSIGNED NON-PRODUCTION DEVELOPMENT BUILD') {
        throw 'A production installer must not show the unsigned non-production warning; point installerHooks at the production hook.'
    }
} elseif ($DeliveredCatalog) {
    if (($hookExecutableLines -join "`n") -notmatch 'UNSIGNED DELIVERED CATALOG RELEASE' -or
        ($hookExecutableLines -join "`n") -notmatch 'production account, catalog, and ingest services' -or
        ($hookExecutableLines -join "`n") -notmatch 'SmartScreen') {
        throw 'The Delivered Catalog installer warning must disclose unsigned status, production service access, and SmartScreen risk.'
    }
} elseif (($hookExecutableLines -join "`n") -notmatch 'UNSIGNED NON-PRODUCTION DEVELOPMENT BUILD') {
    throw 'The interactive NSIS warning is missing its unsigned non-production label.'
}
if (($hookExecutableLines -join "`n") -match '(?im)^\s*(Delete|RMDir)\b') {
    throw 'The NSIS hook must never delete app-local data.'
}

Assert-WindowsBuildDiskHeadroom -TargetPath $cargoTargetRoot
$previousCargoTargetDir = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
# Always use the resolved per-worktree target, including an explicitly configured
# large-disk path. The process value is restored before exit and is not shipped.
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $cargoTargetRoot, 'Process')
# depth 采集工具的锁定制品投放（issue #879）。
#
# 客户端用 `default_tools_dir()` = <exe 目录>\depth-lab\ 去找注入器与 probe DLL，
# 缺任一个就在运行时报「depth 工具不完整」——**只打开 cargo feature 是不够的**。
#
# 这一步必须在 `tauri build` 之前：Tauri 打包时会按 release config 的
# bundle.resources 去磁盘上取这些文件，取不到直接失败。
#
# 由共享 build matrix 决定哪些口味带 Depth。普通客户端构建绝不在这里编译 probe；
# Delivered Catalog、production 与 All Games DEV 都只消费独立工作流发布的锁定 artifact。
if ($depthEnabledBundle) {
    $depthArtifactDirectory = $PinnedDepthToolsDirectory
    if ([string]::IsNullOrWhiteSpace($depthArtifactDirectory)) {
        $depthArtifactDirectory = [Environment]::GetEnvironmentVariable('DM_DEPTH_PROBE_ARTIFACT_DIR', 'Process')
    }
    if ([string]::IsNullOrWhiteSpace($depthArtifactDirectory)) {
        throw @"
$bundleFlavor requires an independently published Depth probe artifact directory.
Set DM_DEPTH_PROBE_ARTIFACT_DIR or pass -PinnedDepthToolsDirectory. The directory
must contain artifact-lock.json and exact immutable bytes from depth-probe-release.yml.
Ordinary DataMiner packaging never compiles dm-depth-probe.
"@
    }
    $runtimeManifest = Stage-LockedDepthProbeArtifact `
        -ArtifactDirectory $depthArtifactDirectory `
        -DestinationDirectory $releaseRoot
    Write-Host "  locked Depth probe artifact staged: runtime=$($runtimeManifest.runtimeVersion) d3d11=$($runtimeManifest.d3d11Generation) d3d12=$($runtimeManifest.d3d12Generation)"
    $depthToolSources = @(
        (Join-Path $releaseRoot 'dm_depth_probe.dll'),
        (Join-Path $releaseRoot 'dm-depth-inject.exe'),
        (Join-Path $releaseRoot 'dm-depth-launch.exe'),
        (Join-Path $releaseRoot 'dm-depth-probe-runtime.json')
    )
    if ($AllGamesDev) {
        $depthToolSources += @(
            (Join-Path $releaseRoot 'dm-depth-extract.exe'),
            (Join-Path $dataMinerRoot 'tools\pr735-cp2077\Set-Cp2077CaptureSettings.ps1'),
            (Join-Path $dataMinerRoot 'tools\pr735-cp2077\Test-Cp2077CaptureSettings.ps1'),
            (Join-Path $dataMinerRoot 'tools\pr735-cp2077\package-local-fivepiece.py')
        )
    }
    foreach ($tool in $depthToolSources) {
        if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) {
            throw @"
depth-lab tool missing after locked artifact staging: $tool
The $bundleFlavor package ships depth capture (docs/107 and docs/109 §10 R-1), and the client
resolves these tools from <install dir>\depth-lab\. Shipping without them would
produce an installer whose depth switch fails at runtime with '深度工具不完整'.
"@
        }
    }
    Write-Host "  depth tools staged: $($depthToolSources -join ', ')"
}

$previousFrontendFlavor = [Environment]::GetEnvironmentVariable('DATAMINER_BUILD_FLAVOR', 'Process')
$previousEmbeddedAccountEnvironment = [Environment]::GetEnvironmentVariable('DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT', 'Process')
$previousEmbeddedAccountBaseUrl = [Environment]::GetEnvironmentVariable('DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL', 'Process')
[Environment]::SetEnvironmentVariable('DATAMINER_BUILD_FLAVOR', $frontendFlavor, 'Process')
if ($AllGamesDev -or $DeliveredCatalog) {
    # Shipped operator packages must be able to sign in on a clean game machine. These are public
    # routing values, not credentials; the user's session token still lives in Credential Manager.
    [Environment]::SetEnvironmentVariable('DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT', 'production', 'Process')
    [Environment]::SetEnvironmentVariable('DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL', 'https://console.diogenesmodel.com/', 'Process')
} else {
    Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT' -Value $null
    Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL' -Value $null
}
$tauriArgs = @('tauri', 'build')
if ($cargoFeatures.Count -gt 0) { $tauriArgs += @('--features', ($cargoFeatures -join ',')) }
$tauriArgs += @('--bundles', 'nsis', '--ci')
if (-not $Production) { $tauriArgs += '--no-sign' }
$tauriArgs += @('--config', $releaseConfigPath)
Push-Location $dataMinerRoot
try {
    Invoke-NativeChecked -Command 'pnpm' -Arguments $tauriArgs
} finally {
    Pop-Location
    Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_BUILD_FLAVOR' -Value $previousFrontendFlavor
    Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT' -Value $previousEmbeddedAccountEnvironment
    Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL' -Value $previousEmbeddedAccountBaseUrl
    Set-WindowsBuildProcessEnvironmentVariable -Name 'CARGO_TARGET_DIR' -Value $previousCargoTargetDir
}

if (-not (Test-Path -LiteralPath $releaseBinary -PathType Leaf)) {
    throw "Release client binary is missing: $releaseBinary"
}
if (-not (Test-Path -LiteralPath (Join-Path $distRoot 'index.html') -PathType Leaf)) {
    throw "Vite production index is missing: $distRoot\index.html"
}

$rawInstallerName = "DataMiner_$($config.version)_x64-setup.exe"
$rawInstaller = Join-Path $bundleRoot $rawInstallerName
if (-not (Test-Path -LiteralPath $rawInstaller -PathType Leaf)) {
    $candidates = @(Get-ChildItem -LiteralPath $bundleRoot -File -Filter '*-setup.exe' |
        Where-Object { $_.Name -notmatch 'UNSIGNED_NON_PRODUCTION' })
    throw "Expected exactly '$rawInstallerName'; found: $($candidates.Name -join ', ')"
}

$publishedName = if ($publishedLabel) {
    "DataMiner_$($config.version)_x64_${publishedLabel}-setup.exe"
} else {
    "DataMiner_$($config.version)_x64-setup.exe"
}
$publishedInstaller = Join-Path $bundleRoot $publishedName

$frontendFiles = @(Get-ChildItem -LiteralPath $distRoot -Recurse -File |
    Where-Object { $_.Extension -in @('.html', '.js', '.css', '.json') })
if ($frontendFiles.Count -lt 2) {
    throw 'Vite production output is incomplete.'
}
foreach ($frontendFile in $frontendFiles) {
    $text = Get-Content -Raw -Encoding UTF8 -LiteralPath $frontendFile.FullName
    foreach ($token in @('hakko', 'localhost:1421', '/@vite/client', 'react-refresh')) {
        if ($text.IndexOf($token, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "Development server token '$token' found in dist asset: $($frontendFile.FullName)"
        }
    }
}

$forbiddenBinaryTokens = @(
    'hakko',
    'localhost:1421',
    '/@vite/client',
    'data-demo-converter.exe',
    'ffmpeg.exe',
    'ffprobe.exe'
)
Assert-NoTextToken -Path $releaseBinary -Tokens $forbiddenBinaryTokens
Assert-TextToken -Path $releaseBinary -Tokens @(
    'hardware_capability_validated_best_effort_controls',
    'https://console.diogenesmodel.com/',
    'https://api.diogenesmodel.com/'
)
if ($Production -or $DeliveredCatalog) {
    # The formal EarlyLaunch rejection path shares a bare game ID with DEV;
    # distinguish that runtime identity from actual embedded catalog payloads.
    $onlineCatalogForbiddenTokens = @(Get-WindowsOnlineClientForbiddenCatalogTokens -Tokens $allGamesRequiredTokens)
    Assert-NoTextToken -Path $releaseBinary -Tokens $onlineCatalogForbiddenTokens
} else {
    # 两个 dev 口味（default_dev / all_games_dev）都随 dev-catalog 内嵌全部游戏。
    Assert-TextToken -Path $releaseBinary -Tokens $allGamesRequiredTokens
}
Assert-WindowsGuiSubsystem -Path $releaseBinary
Assert-ReleaseApplicationManifest -Path $releaseBinary -ExpectedIdentity $config.identifier
Assert-AuthenticodeMatchesFlavor -Path $releaseBinary
Assert-NoTextToken -Path $rawInstaller -Tokens @('hakko')
Assert-AuthenticodeMatchesFlavor -Path $rawInstaller

if ($AllGamesDev) {
    & $releaseSurfaceScript -ReleaseBinaryPath $releaseBinary -ExpectedTargetRoot $cargoTargetRoot -AllowAllGamesDev
} elseif ($DeliveredCatalog) {
    & $releaseSurfaceScript -ReleaseBinaryPath $releaseBinary -ExpectedTargetRoot $cargoTargetRoot -AllowDeliveredCatalog
} elseif ($Production) {
    & $releaseSurfaceScript -ReleaseBinaryPath $releaseBinary -ExpectedTargetRoot $cargoTargetRoot
} else {
    & $releaseSurfaceScript -ReleaseBinaryPath $releaseBinary -ExpectedTargetRoot $cargoTargetRoot -AllowDefaultDev
}
if ($LASTEXITCODE -ne 0) {
    throw 'Release-surface verification failed for dataminer-client.exe.'
}

$installerHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $rawInstaller).Hash.ToLowerInvariant()
$binaryHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $releaseBinary).Hash.ToLowerInvariant()
$dependencies = @(Get-PeDependencies -Path $releaseBinary)
if ($dependencies.Count -eq 0) {
    throw "dumpbin returned no PE dependencies for release binary: $releaseBinary"
}
$configuredTargetRoot = if ([string]::IsNullOrWhiteSpace($configuredCargoTargetDir)) {
    $null
} else {
    [IO.Path]::GetFullPath($cargoTargetRoot).Replace('\', '/')
}
$manifestReleaseRoot = if ($null -ne $configuredTargetRoot) {
    "$configuredTargetRoot/release"
} elseif ($AllGamesDev) {
    'target/all-games-dev/release'
} elseif ($DeliveredCatalog) {
    'target/delivered-catalog/release'
} else {
    'target/release'
}
$manifestInstallerPath = "$manifestReleaseRoot/bundle/nsis/$publishedName"
$manifestBinaryPath = "$manifestReleaseRoot/dataminer-client.exe"
$relativeFrontendDist = 'apps/client/dist'
$manifestSupportedGames = [string[]]@()
$manifestSupportedProcessNames = [string[]]@()
if (-not ($Production -or $DeliveredCatalog)) {
    $manifestSupportedGames = [string[]]$allGamesGameIds
    $manifestSupportedProcessNames = [string[]]$allGamesProcessNames
}
$manifest = [ordered]@{
    schema = if ($DeliveredCatalog) { 'dm-windows-delivered-catalog-bundle-1' } else { 'dm-windows-unsigned-dev-bundle-1' }
    product_name = $config.productName
    version = $config.version
    identifier = $config.identifier
    bundle_target = 'nsis'
    install_mode = 'currentUser'
    signing = if ($Production) { 'authenticode_timestamped' } elseif ($DeliveredCatalog) { 'unsigned_authenticode_absent' } else { 'unsigned_non_production' }
    catalog_mode = if ($Production -or $DeliveredCatalog) { $null } else { 'dev_non_trainable' }
    catalog_provenance = if ($Production -or $DeliveredCatalog) { 'delivered' } else { 'embedded_dev' }
    session_contract = if ($Production -or $DeliveredCatalog) { 'dm-session-ingest' } else { 'dm-local-dev' }
    catalog_sync = if ($Production -or $DeliveredCatalog) { 'required' } else { 'disabled_or_development' }
    account_environment = if ($Production -or $DeliveredCatalog -or $AllGamesDev) { 'production' } else { 'development' }
    account_api_base_url = if ($Production -or $DeliveredCatalog -or $AllGamesDev) { 'https://console.diogenesmodel.com/' } else { $null }
    ingest_api_base_url = if ($Production -or $DeliveredCatalog) { 'https://api.diogenesmodel.com/' } else { $null }
    build_flavor = $bundleFlavor
    cargo_profile = 'release'
    application_manifest_identity = $config.identifier
    requested_execution_level = 'requireAdministrator'
    cargo_features = $cargoFeatures
    depth_resource_destinations = $depthResourceDestinations
    encoder_control_policy = 'hardware_capability_validated_best_effort_controls'
    supported_game = $null
    supported_games = $manifestSupportedGames
    supported_process_names = $manifestSupportedProcessNames
    # docs/55：身份观察对所有游戏是同一条路径，因此这三项不再按口味分叉。
    # 约束一律由目录决定（#278 起指纹与窗口类全车道选填，COD 的目录只是两处都
    # 填了 null）；模式一律是"只读观察 + 拿不到就降级留痕"。
    executable_sha256_constraint = 'catalog_defined'
    executable_identity_mode = 'read_only_observation_degrades_when_denied'
    window_class_constraint = 'catalog_defined'
    installer_file = $publishedName
    installer_path = $manifestInstallerPath
    installer_sha256 = $installerHash
    release_binary_path = $manifestBinaryPath
    release_binary_sha256 = $binaryHash
    frontend_dist = $relativeFrontendDist
    dev_server_absent = $true
    external_business_binaries = @()
    dependency_scan = 'dumpbin_passed'
    pe_dependencies = $dependencies
    app_data_retention = 'default uninstall retains data; separate unchecked checkbox controls deletion'
}
$manifestPath = "$publishedInstaller.manifest.json"
$temporaryManifestPath = "$manifestPath.tmp"
if (Test-Path -LiteralPath $temporaryManifestPath) {
    Remove-Item -LiteralPath $temporaryManifestPath -Force
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 -LiteralPath $temporaryManifestPath
$stagedManifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $temporaryManifestPath | ConvertFrom-Json
if ($stagedManifest.installer_sha256 -ne $installerHash -or
    $stagedManifest.release_binary_sha256 -ne $binaryHash -or
    $stagedManifest.dependency_scan -ne 'dumpbin_passed') {
    throw 'Staged manifest failed publication consistency validation.'
}

# The marked installer plus its manifest are the publication boundary. Keep the
# previous pair intact until all validation and manifest staging have succeeded,
# then publish the manifest last so a failed build never leaves a stale manifest
# paired with a newly marked installer.
if (Test-Path -LiteralPath $publishedInstaller -PathType Leaf) {
    Remove-Item -LiteralPath $publishedInstaller -Force
}
if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
    Remove-Item -LiteralPath $manifestPath -Force
}
try {
    Move-Item -LiteralPath $rawInstaller -Destination $publishedInstaller
    if (Test-Path -LiteralPath $rawInstaller) {
        throw "Unmarked raw NSIS artifact must not remain after publication: $rawInstaller"
    }
    Move-Item -LiteralPath $temporaryManifestPath -Destination $manifestPath
} catch {
    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
        Remove-Item -LiteralPath $manifestPath -Force
    }
    if ((Test-Path -LiteralPath $publishedInstaller -PathType Leaf) -and
        -not (Test-Path -LiteralPath $rawInstaller)) {
        Move-Item -LiteralPath $publishedInstaller -Destination $rawInstaller
    }
    throw
}

if ($Production) {
    Write-Host "PASS: built signed production NSIS installer ($bundleFlavor)."
} elseif ($DeliveredCatalog) {
    Write-Host "PASS: built unsigned Delivered Catalog NSIS installer ($bundleFlavor)."
} else {
    Write-Host "PASS: built unsigned non-production NSIS installer ($bundleFlavor)."
}
Write-Host "Installer: $publishedInstaller"
Write-Host "SHA-256: $installerHash"
Write-Host "Release binary: $releaseBinary"
Write-Host "Manifest: $manifestPath"

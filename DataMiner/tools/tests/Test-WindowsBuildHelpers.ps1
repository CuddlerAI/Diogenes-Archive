[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$toolsRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataMinerRoot = (Resolve-Path (Join-Path $toolsRoot '..')).Path
$helpersPath = Join-Path $toolsRoot 'windows-build-helpers.ps1'

if (-not (Test-Path -LiteralPath $helpersPath -PathType Leaf)) {
    throw "Windows build helpers are missing: $helpersPath"
}
. $helpersPath

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) { throw $Message }
}

function Assert-ThrowsLike {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string[]]$ExpectedFragments
    )

    try {
        & $Action
    } catch {
        $message = $_.Exception.Message
        foreach ($fragment in $ExpectedFragments) {
            if ($message.IndexOf($fragment, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
                throw "Expected failure to contain '$fragment', got: $message"
            }
        }
        return
    }
    throw 'Expected action to fail, but it succeeded.'
}

$tempRoot = Join-Path ([IO.Path]::GetTempPath()) "dataminer-windows-build-helpers-$PID-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $tempRoot | Out-Null
$lockJob = $null

try {
    Remove-AuditManifestTemporaryFile -Path (Join-Path $tempRoot 'already-absent.manifest')

    # A real exclusive Windows lock plus a cleanup job makes the transition
    # deterministic: cleanup must remain in flight while locked, then succeed
    # after this process releases the lock. Production retry defaults are used.
    $transientPath = Join-Path $tempRoot 'transient.manifest'
    [IO.File]::WriteAllText($transientPath, 'temporary manifest')
    $transientLock = [IO.File]::Open(
        $transientPath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $lockJob = Start-Job -ScriptBlock {
            param($HelpersPath, $Path)
            $VerbosePreference = 'Continue'
            . $HelpersPath
            Remove-AuditManifestTemporaryFile -Path $Path
        } -ArgumentList $helpersPath, $transientPath

        # Wait for the helper's retry diagnostic, which is emitted only after a
        # real Remove-Item attempt has failed against this exclusive lock. This
        # avoids a timer-only test that could pass without exercising a retry.
        $retryDeadline = [DateTime]::UtcNow.AddSeconds(10)
        while ($lockJob.State -eq 'Running' -and
               $lockJob.ChildJobs[0].Verbose.Count -eq 0 -and
               [DateTime]::UtcNow -lt $retryDeadline) {
            Start-Sleep -Milliseconds 10
        }
        Assert-True -Condition ($lockJob.ChildJobs[0].Verbose.Count -gt 0) `
            -Message "Manifest cleanup did not report a failed removal attempt under the exclusive lock (state: $($lockJob.State))."
        Assert-True -Condition ($lockJob.State -eq 'Running') `
            -Message "Manifest cleanup did not remain in flight under the exclusive lock (state: $($lockJob.State))."
    } finally {
        $transientLock.Dispose()
    }
    Wait-Job -Job $lockJob -Timeout 10 | Out-Null
    Receive-Job -Job $lockJob | Out-Null
    Remove-Job -Job $lockJob -Force
    $lockJob = $null
    Assert-True -Condition (-not (Test-Path -LiteralPath $transientPath)) `
        -Message 'Transiently locked audit manifest was not removed after the lock cleared.'

    # A lock that outlives the retry budget must still fail closed and identify
    # the exact file, bounded attempt count, and final OS error.
    $persistentPath = Join-Path $tempRoot 'persistent.manifest'
    [IO.File]::WriteAllText($persistentPath, 'temporary manifest')
    $persistentLock = [IO.File]::Open(
        $persistentPath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        Assert-ThrowsLike -Action {
            Remove-AuditManifestTemporaryFile -Path $persistentPath -MaxAttempts 3 -DelayMilliseconds 10
        } -ExpectedFragments @($persistentPath, '3 attempts', 'last error')
    } finally {
        $persistentLock.Dispose()
    }
    Assert-True -Condition (Test-Path -LiteralPath $persistentPath -PathType Leaf) `
        -Message 'Persistent failure was swallowed and the audit file unexpectedly disappeared.'

    $defaultTarget = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -Flavor default_dev
    $expectedDefaultTarget = [IO.Path]::GetFullPath((Join-Path $dataMinerRoot 'target'))
    Assert-True -Condition ($defaultTarget -eq $expectedDefaultTarget) `
        -Message "Default target must stay inside this worktree. Expected '$expectedDefaultTarget', got '$defaultTarget'."

    $largeDiskBase = Join-Path $tempRoot 'large-disk-target'
    $customTarget = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $largeDiskBase `
        -Flavor default_dev
    Assert-True -Condition ($customTarget -eq [IO.Path]::GetFullPath($largeDiskBase)) `
        -Message "An explicit per-worktree CARGO_TARGET_DIR must be honored, got '$customTarget'."

    $flavoredTarget = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $largeDiskBase `
        -Flavor all_games_dev
    $expectedFlavoredTarget = [IO.Path]::GetFullPath((Join-Path $largeDiskBase 'all-games-dev'))
    Assert-True -Condition ($flavoredTarget -eq $expectedFlavoredTarget) `
        -Message "All Games DEV must retain a flavor-isolated target, got '$flavoredTarget'."

    $deliveredTarget = Resolve-WindowsCargoTargetDirectory `
        -DataMinerRoot $dataMinerRoot `
        -ConfiguredTargetDirectory $largeDiskBase `
        -Flavor delivered_catalog
    $expectedDeliveredTarget = [IO.Path]::GetFullPath((Join-Path $largeDiskBase 'delivered-catalog'))
    Assert-True -Condition ($deliveredTarget -eq $expectedDeliveredTarget) `
        -Message "Delivered Catalog must retain a flavor-isolated target, got '$deliveredTarget'."

    # Installer feature combinations are a shipping contract, not an ad-hoc CI
    # sample. The installer and clippy runner must consume the same executable
    # matrix so adding a flavor or game cannot leave -D warnings blind to it.
    $shippedBuildMatrix = @(Get-WindowsShippedClientBuildMatrix -DataMinerRoot $dataMinerRoot)
    $expectedShippedFlavors = @(
        'production',
        'delivered_catalog',
        'default_dev',
        'all_games_dev'
    )
    Assert-True -Condition ($shippedBuildMatrix.Count -eq $expectedShippedFlavors.Count) `
        -Message "Expected $($expectedShippedFlavors.Count) shipped client build cases, got $($shippedBuildMatrix.Count)."
    Assert-True -Condition (
        (@($shippedBuildMatrix.Flavor | Sort-Object) -join ',') -eq
        (@($expectedShippedFlavors | Sort-Object) -join ',')
    ) -Message "Shipped client build flavors drifted: $($shippedBuildMatrix.Flavor -join ', ')."
    Assert-True -Condition (@($shippedBuildMatrix | Where-Object CargoProfile -ne 'release').Count -eq 0) `
        -Message 'Every shipped Windows client case must use the Cargo release profile.'
    Assert-True -Condition (@($shippedBuildMatrix | Where-Object BuildFlavor -notin @('unsigned-dev', 'unsigned-delivered', 'production')).Count -eq 0) `
        -Message 'Every shipped Windows client case must carry an explicit compiler build flavor.'
    Assert-True -Condition (@($shippedBuildMatrix.Flavor | Select-Object -Unique).Count -eq $shippedBuildMatrix.Count) `
        -Message 'Shipped Windows client build flavors must be unique.'

    # services/updater.rs 的编译期断言只在期望值在场时武装。一个新用例忘了声明
    # 期望值，不会报错，只会悄悄不检查——所以在这里挡住。
    foreach ($shippedCase in $shippedBuildMatrix) {
        Assert-True -Condition ([string]$shippedCase.ExpectUpdaterEnabled -in @('0', '1')) `
            -Message "Shipped case '$($shippedCase.Flavor)' must declare ExpectUpdaterEnabled as '0' or '1'."
        Assert-True -Condition ([string]$shippedCase.ExpectDevCatalog -in @('0', '1')) `
            -Message "Shipped case '$($shippedCase.Flavor)' must declare ExpectDevCatalog as '0' or '1'."
        $declaresDevCatalog = if (@($shippedCase.Features) -contains 'dev-catalog') { '1' } else { '0' }
        Assert-True -Condition ([string]$shippedCase.ExpectDevCatalog -eq $declaresDevCatalog) `
            -Message "Shipped case '$($shippedCase.Flavor)' declares ExpectDevCatalog '$($shippedCase.ExpectDevCatalog)' but passes features '$(@($shippedCase.Features) -join ',')'."
        $expectedEnabled = if (
            [string]$shippedCase.BuildFlavor -in @('production', 'unsigned-delivered') -and
            [string]$shippedCase.CargoProfile -eq 'release' -and
            $declaresDevCatalog -eq '0'
        ) { '1' } else { '0' }
        Assert-True -Condition ([string]$shippedCase.ExpectUpdaterEnabled -eq $expectedEnabled) `
            -Message "Shipped case '$($shippedCase.Flavor)' declares ExpectUpdaterEnabled '$($shippedCase.ExpectUpdaterEnabled)' but docs/15 2.3 requires '$expectedEnabled'."
    }
    Assert-True -Condition (
        @($shippedBuildMatrix | Where-Object ExpectUpdaterEnabled -eq '1').Count -eq 2
    ) -Message 'Exactly the signed production and unsigned Delivered Catalog cases may compile with the updater enabled.'

    $productionCase = @($shippedBuildMatrix | Where-Object Flavor -eq 'production')
    $deliveredCase = @($shippedBuildMatrix | Where-Object Flavor -eq 'delivered_catalog')
    $defaultDevCase = @($shippedBuildMatrix | Where-Object Flavor -eq 'default_dev')
    $allGamesCase = @($shippedBuildMatrix | Where-Object Flavor -eq 'all_games_dev')
    Assert-True -Condition (
        @($productionCase[0].Features).Count -eq 1 -and
        @($productionCase[0].Features) -contains 'depth-lab'
    ) -Message 'The production shipped case must include formal Depth without embedding a dev catalog.'
    Assert-True -Condition (
        @($deliveredCase[0].Features).Count -eq 1 -and
        @($deliveredCase[0].Features) -contains 'depth-lab' -and
        $deliveredCase[0].BuildFlavor -eq 'unsigned-delivered'
    ) -Message 'The unsigned Delivered Catalog case must include formal Depth and its distinct compiler flavor.'
    Assert-True -Condition (
        @($defaultDevCase[0].Features).Count -eq 1 -and
        @($defaultDevCase[0].Features) -contains 'dev-catalog'
    ) -Message 'The default unsigned DEV case must be release + dev-catalog only.'
    foreach ($requiredAllGamesFeature in @('dev-catalog', 'local-catalog-only', 'depth-lab')) {
        Assert-True -Condition (@($allGamesCase[0].Features) -contains $requiredAllGamesFeature) `
            -Message "All Games DEV is missing required feature '$requiredAllGamesFeature'."
    }
    # 逐游戏 cargo feature 已退役：矩阵特性集必须恒为固定三元组，manifest 里
    # 也不许再出现 cargoFeature 字段（游戏集合由 client build.rs 按 catalogDir 派生）。
    $allGamesFixedFeatures = @('dev-catalog', 'local-catalog-only', 'depth-lab')
    Assert-True -Condition (@(Compare-Object `
        -ReferenceObject @($allGamesFixedFeatures | Sort-Object) `
        -DifferenceObject @(@($allGamesCase[0].Features) | Sort-Object)).Count -eq 0) `
        -Message 'All Games DEV features must be exactly dev-catalog + local-catalog-only + depth-lab.'
    $shippedGamesManifestPath = Join-Path $dataMinerRoot 'config\catalogs\shipped-games.json'
    $shippedGamesManifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $shippedGamesManifestPath | ConvertFrom-Json
    Assert-True -Condition (@($shippedGamesManifest.games).Count -gt 0) `
        -Message 'shipped-games.json must list at least one shipped game.'
    $manifestCargoFeatures = @($shippedGamesManifest.games | Where-Object {
        $null -ne $_.PSObject.Properties['cargoFeature']
    })
    Assert-True -Condition ($manifestCargoFeatures.Count -eq 0) `
        -Message 'cargoFeature is retired from shipped-games.json; games embed via catalogDir alone.'

    # Reproduce the formal rejection diagnostic, then inject real catalog
    # payloads: sharing a runtime game ID must not permit any shipped DEV game.
    $catalogIdentityTokens = @(
        foreach ($game in $shippedGamesManifest.games) {
            [string]$game.gameId
            $profilePath = Join-Path $dataMinerRoot "config/catalogs/dev/$($game.catalogDir)/game-profile.json"
            $profile = Get-Content -Raw -Encoding UTF8 -LiteralPath $profilePath | ConvertFrom-Json
            foreach ($name in $profile.executable_names) { [string]$name }
        }
        'ffmpeg.exe'
    )
    $onlineForbidden = @(Get-WindowsOnlineClientForbiddenCatalogTokens -Tokens $catalogIdentityTokens)
    $formalDiagnostic = 'D3D12 early-launch target cyberpunk-2077-steam is DEV/test-only and pending an exact reviewed DepthProfile/CameraBinding pair; formal recording was not started'
    Assert-True -Condition (@($onlineForbidden | Where-Object { $formalDiagnostic.Contains($_) }).Count -eq 0) `
        -Message 'The formal pending-target diagnostic must not be mistaken for an embedded catalog.'
    Assert-True -Condition ($catalogIdentityTokens -contains 'cyberpunk-2077-steam') `
        -Message 'The online-client exception must not mutate strict DEV/capture-lab token inputs.'
    Assert-True -Condition ($onlineForbidden -cnotcontains 'kingdom-come-deliverance-2-steam') `
        -Message 'The formal KCD2 runtime/provenance identity is not an embedded catalog.'
    Assert-True -Condition ($catalogIdentityTokens -contains 'kingdom-come-deliverance-2-steam') `
        -Message 'The KCD2 online exception must not mutate strict catalog scan inputs.'
    foreach ($runtimeName in @('Cyberpunk2077.exe', 'KingdomCome.exe')) {
        Assert-True -Condition ($onlineForbidden -cnotcontains $runtimeName) `
            -Message "The formal launcher runtime basename '$runtimeName' is not an embedded catalog."
        Assert-True -Condition ($catalogIdentityTokens -ccontains $runtimeName) `
            -Message "The online exception must not mutate strict DEV/capture-lab token '$runtimeName'."
    }
    foreach ($blockedToken in @(
        'dataminer-cyberpunk-2077-steam-dev',
        'cyberpunk-2077-steam-recording-v1',
        'cyberpunk-2077-steam-windows-nvidia-v2',
        'game.cyberpunk-2077.steam.pc.on-foot',
        'dataminer-kingdom-come-deliverance-2-steam',
        'kingdom-come-deliverance-2-steam-recording-v1',
        'kingdom-come-deliverance-2-steam-windows-nvidia-v1',
        'game.kingdom-come-deliverance-2.steam.pc',
        'fallout4-steam-release-test',
        'ffmpeg.exe'
    )) {
        Assert-True -Condition ($onlineForbidden -contains $blockedToken) `
            -Message "Online-client catalog checking must still reject '$blockedToken'."
    }
    $checkedPayloads = 0
    foreach ($game in $shippedGamesManifest.games) {
        $payloadNames = @('catalog.json', 'game-profile.json')
        if ([string]$game.gameId -in @('cyberpunk-2077-steam', 'kingdom-come-deliverance-2-steam')) {
            $payloadNames += @('device-profile.json', 'recording-profile.json', 'mapping.json')
        }
        foreach ($payloadName in $payloadNames) {
            $payloadPath = Join-Path $dataMinerRoot "config/catalogs/dev/$($game.catalogDir)/$payloadName"
            # Match the installer's ASCII byte view of include_bytes! JSON,
            # including non-ASCII game labels rather than a Unicode text surrogate.
            $payload = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($payloadPath))
            Assert-True -Condition (@($onlineForbidden | Where-Object { $payload.Contains($_) }).Count -gt 0) `
                -Message "Actual DEV payload escaped the online-client scan: $payloadPath"
            $checkedPayloads += 1
        }
    }
    Assert-True -Condition ($checkedPayloads -eq 2 * @($shippedGamesManifest.games).Count + 6) `
        -Message 'Online payload coverage must keep all catalogs/game profiles and all five files for both runtime-name exceptions.'


    & (Join-Path $PSScriptRoot 'Test-WindowsBuildEnvironment.ps1')
    $initialClippyEnvironment = @{}
    foreach ($name in @(
        'DATAMINER_BUILD_FLAVOR',
        'DATAMINER_EXPECT_BUILD_FLAVOR',
        'DATAMINER_EXPECT_DEBUG_ASSERTIONS',
        'DATAMINER_EXPECT_DEV_CATALOG',
        'DATAMINER_EXPECT_UPDATER_ENABLED'
    )) {
        $initialClippyEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    $script:shippedClippyInvocations = [Collections.Generic.List[object]]::new()
    function cargo {
        [void]$script:shippedClippyInvocations.Add([pscustomobject]@{
            Arguments = [string[]]@($args)
            BuildFlavor = [Environment]::GetEnvironmentVariable('DATAMINER_BUILD_FLAVOR', 'Process')
            ExpectBuildFlavor = [Environment]::GetEnvironmentVariable('DATAMINER_EXPECT_BUILD_FLAVOR', 'Process')
            ExpectDebugAssertions = [Environment]::GetEnvironmentVariable('DATAMINER_EXPECT_DEBUG_ASSERTIONS', 'Process')
            ExpectDevCatalog = [Environment]::GetEnvironmentVariable('DATAMINER_EXPECT_DEV_CATALOG', 'Process')
            ExpectUpdaterEnabled = [Environment]::GetEnvironmentVariable('DATAMINER_EXPECT_UPDATER_ENABLED', 'Process')
        })
        $global:LASTEXITCODE = 0
    }
    try {
        Invoke-WindowsShippedClientClippy -DataMinerRoot $dataMinerRoot
    } finally {
        Remove-Item -LiteralPath Function:\cargo -ErrorAction SilentlyContinue
    }
    Assert-True -Condition ($script:shippedClippyInvocations.Count -eq $shippedBuildMatrix.Count) `
        -Message "Shipped clippy coverage invoked $($script:shippedClippyInvocations.Count) cases for a $($shippedBuildMatrix.Count)-case build matrix."
    for ($caseIndex = 0; $caseIndex -lt $shippedBuildMatrix.Count; $caseIndex++) {
        $buildCase = $shippedBuildMatrix[$caseIndex]
        $invocationRecord = $script:shippedClippyInvocations[$caseIndex]
        $invocation = [string[]]$invocationRecord.Arguments
        $expectedPrefix = @('clippy', '-p', 'dataminer-client', '--release', '--all-targets')
        Assert-True -Condition (
            ($invocation[0..($expectedPrefix.Count - 1)] -join ',') -eq ($expectedPrefix -join ',')
        ) -Message "Clippy case '$($buildCase.Flavor)' does not target the release dataminer-client build: $($invocation -join ' ')."
        $featuresIndex = [Array]::IndexOf($invocation, '--features')
        if (@($buildCase.Features).Count -eq 0) {
            Assert-True -Condition ($featuresIndex -lt 0) `
                -Message "Default-feature clippy case '$($buildCase.Flavor)' unexpectedly passed --features."
        } else {
            Assert-True -Condition (
                $featuresIndex -ge 0 -and
                $invocation[$featuresIndex + 1] -eq (@($buildCase.Features) -join ',')
            ) -Message "Clippy features drifted for '$($buildCase.Flavor)': $($invocation -join ' ')."
        }
        Assert-True -Condition (
            ($invocation[($invocation.Count - 3)..($invocation.Count - 1)] -join ',') -eq '--,-D,warnings'
        ) -Message "Clippy case '$($buildCase.Flavor)' must fail on warnings."
        Assert-True -Condition ($invocationRecord.BuildFlavor -eq $buildCase.BuildFlavor) `
            -Message "Clippy case '$($buildCase.Flavor)' compiled with build flavor '$($invocationRecord.BuildFlavor)' instead of '$($buildCase.BuildFlavor)'."

        # 期望值必须真的到达编译器，否则 updater.rs 的 const 断言根本不武装。
        Assert-True -Condition ($invocationRecord.ExpectBuildFlavor -eq $buildCase.BuildFlavor) `
            -Message "Clippy case '$($buildCase.Flavor)' armed the policy check with expected flavor '$($invocationRecord.ExpectBuildFlavor)'."
        $expectedDebugAssertions = if ([string]$buildCase.CargoProfile -eq 'release') { '0' } else { '1' }
        Assert-True -Condition ($invocationRecord.ExpectDebugAssertions -eq $expectedDebugAssertions) `
            -Message "Clippy case '$($buildCase.Flavor)' armed debug_assertions expectation '$($invocationRecord.ExpectDebugAssertions)' instead of '$expectedDebugAssertions'."
        Assert-True -Condition ($invocationRecord.ExpectDevCatalog -eq [string]$buildCase.ExpectDevCatalog) `
            -Message "Clippy case '$($buildCase.Flavor)' armed dev-catalog expectation '$($invocationRecord.ExpectDevCatalog)' instead of '$($buildCase.ExpectDevCatalog)'."
        Assert-True -Condition ($invocationRecord.ExpectUpdaterEnabled -eq [string]$buildCase.ExpectUpdaterEnabled) `
            -Message "Clippy case '$($buildCase.Flavor)' armed updater expectation '$($invocationRecord.ExpectUpdaterEnabled)' instead of '$($buildCase.ExpectUpdaterEnabled)'."
    }

    foreach ($leakedVariable in $initialClippyEnvironment.Keys) {
        Assert-True -Condition (
            [object]::Equals($initialClippyEnvironment[$leakedVariable],
                [Environment]::GetEnvironmentVariable($leakedVariable, 'Process'))
        ) -Message "Shipped clippy leaked '$leakedVariable' into the caller's environment."
    }

    $repoRoot = Split-Path -Parent $dataMinerRoot
    $installerFeatureSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $toolsRoot 'build-windows-installer.ps1')
    $clippyFeatureSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $toolsRoot 'invoke-windows-clippy.ps1')
    $workflowFeatureSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $repoRoot '.github\workflows\dataminer-ci.yml')
    Assert-True -Condition ($installerFeatureSource -match 'Get-WindowsShippedClientBuildCase') `
        -Message 'The installer does not consume the executable shipped-client feature matrix.'
    Assert-True -Condition ($installerFeatureSource -notmatch '\$allGamesFeatures\s*=\s*@\(') `
        -Message 'The installer still carries a second hand-written All Games feature list.'
    Assert-True -Condition ($clippyFeatureSource -match 'Invoke-WindowsShippedClientClippy') `
        -Message 'The Windows clippy entry point does not execute the shipped-client matrix.'
    Assert-True -Condition (
        $workflowFeatureSource -match '(?m)^\s*run:\s*powershell\s+-NoProfile\s+-ExecutionPolicy\s+Bypass\s+-File\s+\./tools/invoke-windows-clippy\.ps1\s*$'
    ) -Message 'dataminer-ci must call the matrix-aware PowerShell clippy entry point directly.'
    Assert-True -Condition ($workflowFeatureSource -notmatch 'pnpm rust:clippy:windows') `
        -Message 'dataminer-ci must not depend on an uninstalled pnpm shim to reach Windows clippy.'
    # 出货 clippy 矩阵全是 release profile，debug_assertions 恒为假。策略那根线
    # 只有这一条命令会在生产风味下把它翻成真；删掉它，`!debug_assertions` 再无人验。
    Assert-True -Condition (
        $workflowFeatureSource -match "(?s)DATAMINER_BUILD_FLAVOR:\s*production.*?DATAMINER_EXPECT_DEBUG_ASSERTIONS:\s*'1'.*?cargo check -p dataminer-client --lib"
    ) -Message 'dataminer-ci must keep a production-flavor debug-profile check for the updater policy.'
    foreach ($rustRelevantTool in @(
        'build-windows-installer.ps1',
        'invoke-windows-clippy.ps1',
        'windows-build-helpers.ps1'
    )) {
        Assert-True -Condition (
            $workflowFeatureSource -match [Regex]::Escape("path !== 'DataMiner/tools/$rustRelevantTool'")
        ) -Message "dataminer-ci can skip Rust checks when '$rustRelevantTool' changes."
    }

    $allGamesResourceMap = New-TauriBundleResourceMap -Entries @(
        @{ Source = (Join-Path $flavoredTarget 'release\dm_depth_probe.dll'); Destination = 'depth-lab/dm_depth_probe.dll' },
        @{ Source = (Join-Path $flavoredTarget 'release\dm-depth-inject.exe'); Destination = 'depth-lab/dm-depth-inject.exe' }
    )
    $expectedProbeSource = [IO.Path]::GetFullPath((Join-Path $flavoredTarget 'release\dm_depth_probe.dll'))
    Assert-True -Condition ($allGamesResourceMap[$expectedProbeSource] -eq 'depth-lab/dm_depth_probe.dll') `
        -Message 'Dynamic All Games resource mapping did not use the resolved large-disk target.'
    Assert-True -Condition (-not ($allGamesResourceMap.Keys -match '\\DataMiner\\target\\')) `
        -Message 'Dynamic resource mapping fell back to the repository target instead of the explicit target.'

    $baseConfig = [pscustomobject]@{
        build = [pscustomobject]@{ devUrl = $null; beforeDevCommand = $null }
        bundle = [pscustomobject]@{ resources = [pscustomobject]@{ stale = 'stale' } }
    }
    $dynamicConfigPath = Join-Path $tempRoot 'tauri.dynamic.conf.json'
    Write-TauriConfigWithBundleResources `
        -BaseConfig $baseConfig `
        -ResourceMap $allGamesResourceMap `
        -Path $dynamicConfigPath
    $dynamicConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $dynamicConfigPath | ConvertFrom-Json
    Assert-True -Condition ($dynamicConfig.bundle.resources.$expectedProbeSource -eq 'depth-lab/dm_depth_probe.dll') `
        -Message 'Generated Tauri config did not persist the resolved absolute resource source.'
    Assert-True -Condition ($null -eq $dynamicConfig.bundle.resources.PSObject.Properties['stale']) `
        -Message 'Generated Tauri config retained a stale repository-target resource entry.'

    $configWithoutBundle = [pscustomobject]@{
        build = [pscustomobject]@{ devUrl = $null; beforeDevCommand = $null }
    }
    $dynamicConfigWithoutBundlePath = Join-Path $tempRoot 'tauri.dynamic-without-bundle.conf.json'
    Write-TauriConfigWithBundleResources `
        -BaseConfig $configWithoutBundle `
        -ResourceMap $allGamesResourceMap `
        -Path $dynamicConfigWithoutBundlePath
    $dynamicConfigWithoutBundle = Get-Content -Raw -Encoding UTF8 -LiteralPath $dynamicConfigWithoutBundlePath | ConvertFrom-Json
    Assert-True -Condition ($dynamicConfigWithoutBundle.bundle.resources.$expectedProbeSource -eq 'depth-lab/dm_depth_probe.dll') `
        -Message 'Generated Tauri config could not add resources when the base config omitted bundle.'

    $releaseSurfaceSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $toolsRoot 'verify-release-surface.ps1')
    $mainInstallerSource = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $toolsRoot 'build-windows-installer.ps1')
    Assert-True -Condition ($releaseSurfaceSource -match '\[string\]\$ExpectedTargetRoot') `
        -Message 'Release-surface verifier cannot validate a binary under an explicit large-disk target.'
    Assert-True -Condition ($mainInstallerSource -match '-ExpectedTargetRoot\s+\$cargoTargetRoot') `
        -Message 'Main installer does not pass its resolved target to release-surface verification.'
    Assert-True -Condition ($mainInstallerSource -match '\$configuredTargetRoot/release') `
        -Message 'Bundle manifest paths do not describe an explicit large-disk target.'

    $containmentRoot = Join-Path $tempRoot 'containment-target\release'
    $insidePath = Join-Path $containmentRoot 'dataminer-client.exe'
    $outsidePath = Join-Path $tempRoot 'containment-target-sibling\release\dataminer-client.exe'
    Assert-True -Condition ((Assert-PathUnderDirectory `
        -Path $insidePath `
        -Directory $containmentRoot `
        -Description 'Release binary') -eq [IO.Path]::GetFullPath($insidePath)) `
        -Message 'Release target containment rejected a path inside the expected release directory.'
    Assert-ThrowsLike -Action {
        Assert-PathUnderDirectory `
            -Path $outsidePath `
            -Directory $containmentRoot `
            -Description 'Release binary'
    } -ExpectedFragments @('Release binary', [IO.Path]::GetFullPath($outsidePath))

    $helperSource = Get-Content -Raw -Encoding UTF8 -LiteralPath $helpersPath
    Assert-True -Condition ($helperSource -match 'function Invoke-CargoTestWithWindowsManifest') `
        -Message 'Updater matrix lacks the manifest-aware Windows Cargo test runner.'
    Assert-True -Condition ($helperSource -match "'--no-run'.*'--message-format=json-render-diagnostics'") `
        -Message 'Manifest-aware Cargo runner must discover the exact test executable from Cargo JSON.'
    Assert-True -Condition ($helperSource -match '-outputresource:\$\(\$testExecutables\[0\]\);#1') `
        -Message 'Manifest-aware Cargo runner does not embed RT_MANIFEST #1 before executing tests.'

    # Cargo versions describe an ordinary library target as either "lib" or
    # "rlib" in compiler-artifact JSON. Exercise the CI form (kind=["lib"])
    # end to end, with same-target and same-package decoys proving that the
    # selector uses both the package and target names rather than type alone.
    $fakeTestExecutable = Join-Path $tempRoot 'fake-dataminer-lib-test.exe'
    Add-Type -TypeDefinition @'
using System;

public static class FakeDataMinerLibTest
{
    public static int Main(string[] args)
    {
        if (Array.IndexOf(args, "--list") >= 0)
        {
            Console.WriteLine("services::updater::tests::update_policy_truth_table_is_fail_closed: test");
            Console.WriteLine("services::updater::tests::compiled_policy_matches_explicit_matrix_expectation: test");
            Console.WriteLine("services::updater::tests::disabled_manual_check_returns_before_updater_access: test");
        }
        return 0;
    }
}
'@ -Language CSharp -OutputAssembly $fakeTestExecutable -OutputType ConsoleApplication

    $fakeCargoMetadata = [ordered]@{
        packages = @(
            [ordered]@{
                name = 'not-dataminer-client'
                id = 'opaque-decoy-package-id'
                targets = @(
                    [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
                )
            },
            [ordered]@{
                name = 'dataminer-client'
                id = 'opaque-dataminer-package-id'
                targets = @(
                    [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
                )
            }
        )
    }
    $fakeBuildFinished = [ordered]@{
        reason = 'build-finished'
        success = $true
    }
    $fakeCargoDecoys = @(
        [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-decoy-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
            profile = [ordered]@{ test = $true }
            executable = (Join-Path $tempRoot 'wrong-package.exe')
        },
        [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dm-depth-acceptance'; kind = @('lib'); crate_types = @('lib') }
            profile = [ordered]@{ test = $true }
            executable = (Join-Path $tempRoot 'wrong-target.exe')
        },
        [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
            profile = [ordered]@{ test = $false }
            executable = (Join-Path $tempRoot 'not-a-test-profile.exe')
        },
        [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('bin'); crate_types = @('bin') }
            profile = [ordered]@{ test = $true }
            executable = (Join-Path $tempRoot 'wrong-target-type.exe')
        },
        [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
            profile = [ordered]@{ test = $true }
            executable = $null
        }
    )
    $fakeValidArtifact = [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('lib'); crate_types = @('lib') }
            profile = [ordered]@{ test = $true }
            executable = $fakeTestExecutable
    }
    $fakeCargoMessages = @($fakeBuildFinished) + $fakeCargoDecoys + @($fakeValidArtifact)
    function cargo {
        if ($args -contains 'metadata') {
            $fakeCargoMetadata | ConvertTo-Json -Compress -Depth 10
        } else {
            foreach ($message in $fakeCargoMessages) {
                $message | ConvertTo-Json -Compress -Depth 10
            }
        }
        $global:LASTEXITCODE = 0
    }
    $fakeInvocation = @{
        CargoArguments = @('test', '-p', 'dataminer-client', '--lib')
        ManifestPath = (Join-Path $dataMinerRoot 'apps\client\src-tauri\windows\acceptance-helper.manifest')
        TestFilter = 'services::updater::tests::'
        RequiredTestNames = @(
                'services::updater::tests::update_policy_truth_table_is_fail_closed',
                'services::updater::tests::compiled_policy_matches_explicit_matrix_expectation',
                'services::updater::tests::disabled_manual_check_returns_before_updater_access'
        )
    }
    try {
        # CI/current Cargo reports lib/lib; older Cargo reports rlib/rlib. Both
        # describe the same --lib test target and must select the same executable.
        Invoke-CargoTestWithWindowsManifest @fakeInvocation

        $allTestsInvocation = $fakeInvocation.Clone()
        $allTestsInvocation.TestFilter = ''
        $allTestsInvocation.RequiredTestNames = @()
        Invoke-CargoTestWithWindowsManifest @allTestsInvocation

        $fakeCargoMetadata.packages[1].targets[0].kind = @('rlib')
        $fakeCargoMetadata.packages[1].targets[0].crate_types = @('rlib')
        $fakeValidArtifact.target.kind = @('rlib')
        $fakeValidArtifact.target.crate_types = @('rlib')
        $fakeCargoMessages = @($fakeBuildFinished) + $fakeCargoDecoys + @($fakeValidArtifact)
        Invoke-CargoTestWithWindowsManifest @fakeInvocation

        $fakeCargoMessages = @($fakeBuildFinished) + $fakeCargoDecoys
        Assert-ThrowsLike -Action {
            Invoke-CargoTestWithWindowsManifest @fakeInvocation
        } -ExpectedFragments @('Expected exactly one Cargo lib-test executable', 'found 0')

        $secondTestExecutable = Join-Path $tempRoot 'second-dataminer-lib-test.exe'
        $secondValidArtifact = [ordered]@{
            reason = 'compiler-artifact'
            package_id = 'opaque-dataminer-package-id'
            target = [ordered]@{ name = 'dataminer_client_lib'; kind = @('rlib'); crate_types = @('rlib') }
            profile = [ordered]@{ test = $true }
            executable = $secondTestExecutable
        }
        $fakeCargoMessages = @($fakeBuildFinished) + $fakeCargoDecoys + @($fakeValidArtifact, $secondValidArtifact)
        Assert-ThrowsLike -Action {
            Invoke-CargoTestWithWindowsManifest @fakeInvocation
        } -ExpectedFragments @(
            'Expected exactly one Cargo lib-test executable',
            'found 2',
            $fakeTestExecutable,
            $secondTestExecutable
        )
    } finally {
        Remove-Item -LiteralPath Function:\cargo -ErrorAction SilentlyContinue
    }

    $depthArtifactRoot = Join-Path $tempRoot 'depth-artifact'
    $depthStageRoot = Join-Path $tempRoot 'depth-stage'
    New-Item -ItemType Directory -Path $depthArtifactRoot | Out-Null
    $depthBinaryNames = @(
        'dm_depth_probe.dll',
        'dm-depth-inject.exe',
        'dm-depth-launch.exe',
        'dm-depth-extract.exe'
    )
    foreach ($name in $depthBinaryNames) {
        [IO.File]::WriteAllBytes(
            (Join-Path $depthArtifactRoot $name),
            [Text.Encoding]::UTF8.GetBytes("locked-$name")
        )
    }
    $probeSha = Get-DepthProbeArtifactSha256 `
        -Path (Join-Path $depthArtifactRoot 'dm_depth_probe.dll')
    $runtimeManifest = [ordered]@{
        schema = 'dm-depth-probe-runtime-manifest-v1'
        runtimeVersion = 'depth-2026.08.1'
        dllSha256 = $probeSha
        d3d11Generation = 4
        d3d12Generation = 9
    }
    [IO.File]::WriteAllText(
        (Join-Path $depthArtifactRoot 'dm-depth-probe-runtime.json'),
        ($runtimeManifest | ConvertTo-Json -Depth 10),
        [Text.UTF8Encoding]::new($false)
    )
    $lockedNames = $depthBinaryNames + @('dm-depth-probe-runtime.json')
    $lockedFiles = @($lockedNames | ForEach-Object {
        $path = Join-Path $depthArtifactRoot $_
        [ordered]@{
            name = $_
            size = (Get-Item -LiteralPath $path).Length
            sha256 = Get-DepthProbeArtifactSha256 -Path $path
        }
    })
    $artifactLock = [ordered]@{
        schemaVersion = 'dm-depth-probe-artifact-lock-v1'
        artifactId = 'depth-2026.08.1-test'
        files = $lockedFiles
    }
    [IO.File]::WriteAllText(
        (Join-Path $depthArtifactRoot 'artifact-lock.json'),
        ($artifactLock | ConvertTo-Json -Depth 10),
        [Text.UTF8Encoding]::new($false)
    )

    $stagedRuntime = Stage-LockedDepthProbeArtifact `
        -ArtifactDirectory $depthArtifactRoot `
        -DestinationDirectory $depthStageRoot
    Assert-True -Condition ([string]$stagedRuntime.runtimeVersion -eq 'depth-2026.08.1') `
        -Message 'The locked Depth runtime identity was not returned after staging.'
    foreach ($name in $lockedNames) {
        Assert-True -Condition (Test-Path -LiteralPath (Join-Path $depthStageRoot $name) -PathType Leaf) `
            -Message "Locked Depth artifact file was not staged: $name"
    }

    [IO.File]::AppendAllText((Join-Path $depthArtifactRoot 'dm_depth_probe.dll'), 'tampered')
    Assert-ThrowsLike -Action {
        Stage-LockedDepthProbeArtifact `
            -ArtifactDirectory $depthArtifactRoot `
            -DestinationDirectory (Join-Path $tempRoot 'tampered-stage')
    } -ExpectedFragments @('dm_depth_probe.dll', 'mismatch')

    $eightGiB = [uint64](8GB)
    Assert-ThrowsLike -Action {
        Assert-WindowsBuildDiskHeadroom `
            -TargetPath $customTarget `
            -AvailableBytes ([uint64](3GB)) `
            -MinimumFreeBytes $eightGiB
    } -ExpectedFragments @($customTarget, '3.00 GiB', '8.00 GiB')

    Assert-WindowsBuildDiskHeadroom `
        -TargetPath $customTarget `
        -AvailableBytes $eightGiB `
        -MinimumFreeBytes $eightGiB

    $clippyEntryPoint = Join-Path $toolsRoot 'invoke-windows-clippy.ps1'
    & $clippyEntryPoint -HeadroomOnly
    Assert-True -Condition ($LASTEXITCODE -eq 0) `
        -Message 'The Windows clippy entry point did not complete its executable headroom preflight.'

    Write-Host 'PASS: Windows build helper behavior'
} finally {
    if ($null -ne $lockJob) {
        Stop-Job -Job $lockJob -ErrorAction SilentlyContinue
        Remove-Job -Job $lockJob -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

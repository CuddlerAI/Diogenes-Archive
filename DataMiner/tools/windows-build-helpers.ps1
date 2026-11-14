Set-StrictMode -Version Latest

function Set-WindowsBuildProcessEnvironmentVariable {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][AllowNull()][AllowEmptyString()][object]$Value
    )

    # PowerShell coerces an ordinary $null to an empty string for this .NET
    # parameter. Modern .NET preserves empty environment values, so use a typed
    # null to actually remove an originally absent variable. Keep real empty
    # strings and existing values intact when restoring the caller's state.
    if ($null -eq $Value) {
        [Environment]::SetEnvironmentVariable($Name, [NullString]::Value, 'Process')
    } else {
        [Environment]::SetEnvironmentVariable($Name, [string]$Value, 'Process')
    }
}

function Get-WindowsOnlineClientForbiddenCatalogTokens {
    param([Parameter(Mandatory = $true)][string[]]$Tokens)

    # Formal EarlyLaunch needs game IDs and executable basenames for the pending
    # CP2077 target and the KCD2 runtime path. These four exact strings are not
    # embedded catalogs. Keep catalog/profile/mapping identities forbidden so
    # sharing runtime names never admits either complete DEV bundle.
    # Apply this only to the online client, never to capture-lab or DEV scans.
    return @(
        $Tokens | Where-Object { $_ -cnotin @('cyberpunk-2077-steam', 'Cyberpunk2077.exe', 'kingdom-come-deliverance-2-steam', 'KingdomCome.exe') }
        'dataminer-cyberpunk-2077-steam-dev'
        'cyberpunk-2077-steam-recording-v1'
        'cyberpunk-2077-steam-windows-nvidia-v2'
        'game.cyberpunk-2077.steam.pc.on-foot'
        'dataminer-kingdom-come-deliverance-2-steam'
        'kingdom-come-deliverance-2-steam-recording-v1'
        'kingdom-come-deliverance-2-steam-windows-nvidia-v1'
        'game.kingdom-come-deliverance-2.steam.pc'
    ) | Sort-Object -Unique
}

function Get-WindowsShippedClientBuildMatrix {
    param(
        [Parameter(Mandatory = $true)][string]$DataMinerRoot
    )

    # This is the single source of truth for Cargo combinations emitted by
    # build-windows-installer.ps1. CI consumes the same objects rather than
    # maintaining a second YAML/PowerShell feature list that can drift.
    #
    # 游戏不再有逐款的 cargo feature：dev-catalog 一开，client build.rs 就从
    # config/catalogs/shipped-games.json 派生并内嵌全部游戏目录，因此矩阵
    # 不读 manifest、特性集为固定三元组。COD 专属口味与逐游戏特性一同退役
    # 正式发布流水线只出 delivered_catalog；它与未来的 production 都编入
    # catalog 驱动的正式 Depth 能力，但仍然零游戏内嵌。all_games_dev 另保留
    # local-catalog-only 与 CP2077 本地试验工具，不占用线上更新渠道。
    # Expect* are the updater-policy expectations asserted at compile time by
    # services/updater.rs. They are declared per case, never derived from the
    # flavor, so a shipped combination cannot grade its own answer.
    return @(
        [pscustomobject][ordered]@{
            Flavor = 'production'
            CargoProfile = 'release'
            BuildFlavor = 'production'
            Features = [string[]]@('depth-lab')
            ExpectDevCatalog = '0'
            ExpectUpdaterEnabled = '1'
        },
        [pscustomobject][ordered]@{
            # Authenticode 尚未接入时的线上采集口味。它与 production 一样不内嵌
            # dev catalog，只从服务端验签下发；区别只在 Windows 代码签名状态与
            # 对应的醒目 UNSIGNED 文案。
            Flavor = 'delivered_catalog'
            CargoProfile = 'release'
            BuildFlavor = 'unsigned-delivered'
            Features = [string[]]@('depth-lab')
            ExpectDevCatalog = '0'
            ExpectUpdaterEnabled = '1'
        },
        [pscustomobject][ordered]@{
            Flavor = 'default_dev'
            CargoProfile = 'release'
            BuildFlavor = 'unsigned-dev'
            Features = [string[]]@('dev-catalog')
            ExpectDevCatalog = '1'
            ExpectUpdaterEnabled = '0'
        },
        [pscustomobject][ordered]@{
            Flavor = 'all_games_dev'
            CargoProfile = 'release'
            BuildFlavor = 'unsigned-dev'
            Features = [string[]]@('dev-catalog', 'local-catalog-only', 'depth-lab')
            ExpectDevCatalog = '1'
            ExpectUpdaterEnabled = '0'
        }
    )
}

function Get-WindowsShippedClientBuildCase {
    param(
        [Parameter(Mandatory = $true)][string]$DataMinerRoot,
        [Parameter(Mandatory = $true)]
        [ValidateSet('production', 'delivered_catalog', 'default_dev', 'all_games_dev')]
        [string]$Flavor
    )

    $matches = @(Get-WindowsShippedClientBuildMatrix -DataMinerRoot $DataMinerRoot | Where-Object {
        $_.Flavor -eq $Flavor
    })
    if ($matches.Count -ne 1) {
        throw "Expected exactly one shipped Windows client build case for '$Flavor', found $($matches.Count)."
    }

    # Return a fresh feature array so callers cannot mutate the shared plan they
    # use to reconcile installer and CI behavior.
    return [pscustomobject][ordered]@{
        Flavor = [string]$matches[0].Flavor
        CargoProfile = [string]$matches[0].CargoProfile
        BuildFlavor = [string]$matches[0].BuildFlavor
        Features = [string[]]@($matches[0].Features)
    }
}

function Invoke-WindowsShippedClientClippy {
    param([Parameter(Mandatory = $true)][string]$DataMinerRoot)

    $policyExpectationVariables = @(
        'DATAMINER_EXPECT_BUILD_FLAVOR',
        'DATAMINER_EXPECT_DEBUG_ASSERTIONS',
        'DATAMINER_EXPECT_DEV_CATALOG',
        'DATAMINER_EXPECT_UPDATER_ENABLED'
    )
    $previousBuildFlavor = [Environment]::GetEnvironmentVariable('DATAMINER_BUILD_FLAVOR', 'Process')
    $previousPolicyExpectations = @{}
    foreach ($variableName in $policyExpectationVariables) {
        $previousPolicyExpectations[$variableName] =
            [Environment]::GetEnvironmentVariable($variableName, 'Process')
    }
    try {
        $buildMatrix = @(Get-WindowsShippedClientBuildMatrix -DataMinerRoot $DataMinerRoot)
        foreach ($buildCase in $buildMatrix) {
            $cargoArguments = @(
                'clippy',
                '-p',
                'dataminer-client',
                '--release',
                '--all-targets'
            )
            $features = [string[]]@($buildCase.Features)
            if ($features.Count -gt 0) {
                $cargoArguments += @('--features', ($features -join ','))
            }
            $cargoArguments += @('--', '-D', 'warnings')

            $featureLabel = if ($features.Count -eq 0) {
                '<default>'
            } else {
                $features -join ','
            }
            [Environment]::SetEnvironmentVariable(
                'DATAMINER_BUILD_FLAVOR',
                [string]$buildCase.BuildFlavor,
                'Process'
            )

            # Arm the compile-time updater-policy assertion in services/updater.rs.
            # This is what replaced a 38-minute step that linked one Tauri test
            # executable per flavor to run 0.08s of assertions: the values are all
            # compile-time constants, so `clippy` already decides them here at no
            # extra cost. A missing expectation fails the build rather than being
            # skipped, so a typo cannot quietly stop checking.
            [Environment]::SetEnvironmentVariable(
                'DATAMINER_EXPECT_BUILD_FLAVOR',
                [string]$buildCase.BuildFlavor,
                'Process'
            )
            [Environment]::SetEnvironmentVariable(
                'DATAMINER_EXPECT_DEBUG_ASSERTIONS',
                $(if ([string]$buildCase.CargoProfile -eq 'release') { '0' } else { '1' }),
                'Process'
            )
            [Environment]::SetEnvironmentVariable(
                'DATAMINER_EXPECT_DEV_CATALOG',
                [string]$buildCase.ExpectDevCatalog,
                'Process'
            )
            [Environment]::SetEnvironmentVariable(
                'DATAMINER_EXPECT_UPDATER_ENABLED',
                [string]$buildCase.ExpectUpdaterEnabled,
                'Process'
            )
            Write-Host "Clippy shipped Windows client case: $($buildCase.Flavor) ($featureLabel)"
            & cargo @cargoArguments
            if ($LASTEXITCODE -ne 0) {
                throw "cargo clippy failed for shipped Windows client case '$($buildCase.Flavor)' with exit code $LASTEXITCODE"
            }
        }
    } finally {
        Set-WindowsBuildProcessEnvironmentVariable -Name 'DATAMINER_BUILD_FLAVOR' -Value $previousBuildFlavor
        foreach ($variableName in $policyExpectationVariables) {
            Set-WindowsBuildProcessEnvironmentVariable -Name $variableName -Value $previousPolicyExpectations[$variableName]
        }
    }
}

function Format-BuildBytes {
    param([Parameter(Mandatory = $true)][uint64]$Bytes)

    return [string]::Format(
        [Globalization.CultureInfo]::InvariantCulture,
        '{0:N2} GiB',
        ($Bytes / 1GB)
    )
}

function Remove-AuditManifestTemporaryFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [ValidateRange(1, 100)][int]$MaxAttempts = 6,
        [ValidateRange(0, 60000)][int]$DelayMilliseconds = 100
    )

    $lastError = $null
    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        try {
            Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            return
        } catch {
            # Another actor deleting the same disposable audit file is already
            # the desired end state; do not turn that race into a build failure.
            if (-not (Test-Path -LiteralPath $Path)) {
                return
            }
            $lastError = $_.Exception
            Write-Verbose "Manifest audit temporary-file removal attempt $attempt failed for '$Path': $($lastError.Message)"
            if ($attempt -lt $MaxAttempts -and $DelayMilliseconds -gt 0) {
                Start-Sleep -Milliseconds $DelayMilliseconds
            }
        }
    }

    throw "Could not remove manifest audit temporary file '$Path' after $MaxAttempts attempts; last error: $($lastError.Message)"
}

function Resolve-WindowsCargoTargetDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$DataMinerRoot,
        [AllowEmptyString()][string]$ConfiguredTargetDirectory,
        [Parameter(Mandatory = $true)]
        [ValidateSet('default_dev', 'delivered_catalog', 'all_games_dev', 'production')]
        [string]$Flavor
    )

    $targetRoot = if ([string]::IsNullOrWhiteSpace($ConfiguredTargetDirectory)) {
        Join-Path $DataMinerRoot 'target'
    } elseif ([IO.Path]::IsPathRooted($ConfiguredTargetDirectory)) {
        $ConfiguredTargetDirectory
    } else {
        Join-Path $DataMinerRoot $ConfiguredTargetDirectory
    }

    $flavorDirectory = switch ($Flavor) {
        'delivered_catalog' { 'delivered-catalog' }
        'all_games_dev' { 'all-games-dev' }
        default { $null }
    }
    if ($null -ne $flavorDirectory) {
        $targetRoot = Join-Path $targetRoot $flavorDirectory
    }

    return [IO.Path]::GetFullPath($targetRoot)
}

function Get-WindowsBuildAvailableBytes {
    param([Parameter(Mandatory = $true)][string]$TargetPath)

    $probePath = [IO.Path]::GetFullPath($TargetPath)
    while (-not (Test-Path -LiteralPath $probePath)) {
        $parent = Split-Path -Parent $probePath
        if ([string]::IsNullOrWhiteSpace($parent) -or $parent -eq $probePath) {
            throw "Cannot resolve an existing parent to check build disk headroom for target: $TargetPath"
        }
        $probePath = $parent
    }

    return [uint64]([IO.DriveInfo]::new([IO.Path]::GetPathRoot($probePath)).AvailableFreeSpace)
}

function Assert-WindowsBuildDiskHeadroom {
    param(
        [Parameter(Mandatory = $true)][string]$TargetPath,
        [Nullable[uint64]]$AvailableBytes = $null,
        [uint64]$MinimumFreeBytes = [uint64](8GB)
    )

    $actualAvailableBytes = if ($null -eq $AvailableBytes) {
        Get-WindowsBuildAvailableBytes -TargetPath $TargetPath
    } else {
        [uint64]$AvailableBytes
    }
    if ($actualAvailableBytes -lt $MinimumFreeBytes) {
        throw @"
Insufficient disk headroom for the Windows Rust build target.
  target: $TargetPath
  available: $(Format-BuildBytes -Bytes $actualAvailableBytes)
  required: $(Format-BuildBytes -Bytes $MinimumFreeBytes)
Use a distinct large-disk path for this worktree, for example:
  `$env:CARGO_TARGET_DIR = 'D:\cargo-targets\<worktree-name>'
Do not point concurrent worktrees at the same writable target directory.
"@
    }

    Write-Host "Windows build target: $TargetPath"
    Write-Host "Available headroom: $(Format-BuildBytes -Bytes $actualAvailableBytes) (minimum $(Format-BuildBytes -Bytes $MinimumFreeBytes))"
}

function New-TauriBundleResourceMap {
    param(
        [Parameter(Mandatory = $true)]
        [object[]]$Entries
    )

    $resources = [ordered]@{}
    foreach ($entry in $Entries) {
        $source = [IO.Path]::GetFullPath([string]$entry.Source)
        $destination = [string]$entry.Destination
        if ([string]::IsNullOrWhiteSpace($destination)) {
            throw "Tauri bundle resource destination is empty for source: $source"
        }
        $resources[$source] = $destination
    }
    return $resources
}

function Write-TauriConfigWithBundleResources {
    param(
        [Parameter(Mandatory = $true)][object]$BaseConfig,
        [Parameter(Mandatory = $true)][Collections.IDictionary]$ResourceMap,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $bundleProperty = $BaseConfig.PSObject.Properties['bundle']
    if ($null -eq $bundleProperty) {
        $BaseConfig | Add-Member -MemberType NoteProperty -Name bundle -Value ([pscustomobject]@{})
    }
    $resourcesProperty = $BaseConfig.bundle.PSObject.Properties['resources']
    if ($null -eq $resourcesProperty) {
        $BaseConfig.bundle | Add-Member -MemberType NoteProperty -Name resources -Value $ResourceMap
    } else {
        $BaseConfig.bundle.resources = $ResourceMap
    }
    $json = $BaseConfig | ConvertTo-Json -Depth 20
    [IO.File]::WriteAllText(
        [IO.Path]::GetFullPath($Path),
        $json,
        [Text.UTF8Encoding]::new($false)
    )
}

function Assert-PathUnderDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $resolvedPath = [IO.Path]::GetFullPath($Path)
    $resolvedDirectory = [IO.Path]::GetFullPath($Directory).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedPath.StartsWith($resolvedDirectory, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Description must remain under '$resolvedDirectory': $resolvedPath"
    }
    return $resolvedPath
}

function Invoke-CargoTestWithWindowsManifest {
    param(
        [Parameter(Mandatory = $true)][string[]]$CargoArguments,
        [Parameter(Mandatory = $true)][string]$ManifestPath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$TestFilter,
        [string[]]$RequiredTestNames = @(),
        [string]$PackageName = 'dataminer-client',
        [string]$TargetName = 'dataminer_client_lib'
    )

    # Treat Cargo package IDs as opaque. Resolve the exact ID from metadata by
    # package/target name, then require the compiler artifact to carry that ID.
    $cargoPrefixArguments = @()
    if ($CargoArguments.Count -gt 0 -and $CargoArguments[0].StartsWith('+', [StringComparison]::Ordinal)) {
        $cargoPrefixArguments = @($CargoArguments[0])
    }

    # Windows PowerShell 5.1 promotes normal native stderr to NativeCommandError
    # when the caller uses ErrorActionPreference=Stop. Capture it without letting
    # progress output terminate the helper, then restore the caller's preference.
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $metadataOutput = @(& cargo @cargoPrefixArguments 'metadata' '--no-deps' '--format-version' '1' 2>&1 |
            ForEach-Object { $_.ToString() })
        $metadataExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $metadataOutput | Where-Object { $_ -notmatch '^\{' } | ForEach-Object { Write-Host $_ }
    if ($metadataExitCode -ne 0) {
        throw "cargo metadata failed with exit code $metadataExitCode"
    }
    $metadataDocuments = @($metadataOutput | ForEach-Object {
        if ($_ -notmatch '^\{') {
            return
        }
        $document = $_ | ConvertFrom-Json -ErrorAction SilentlyContinue
        if ($null -ne $document -and $null -ne $document.PSObject.Properties['packages']) {
            $document
        }
    })
    if ($metadataDocuments.Count -ne 1) {
        throw "Expected exactly one Cargo metadata document, found $($metadataDocuments.Count)"
    }
    $packageMatches = @($metadataDocuments[0].packages | Where-Object {
        [string]::Equals([string]$_.name, $PackageName, [StringComparison]::Ordinal)
    })
    if ($packageMatches.Count -ne 1) {
        throw "Expected exactly one Cargo package named '$PackageName', found $($packageMatches.Count)"
    }
    $metadataTargetMatches = @($packageMatches[0].targets | Where-Object {
        $kinds = @($_.kind)
        $crateTypes = @($_.crate_types)
        [string]::Equals([string]$_.name, $TargetName, [StringComparison]::Ordinal) -and
            ($kinds -contains 'lib' -or $kinds -contains 'rlib') -and
            ($crateTypes -contains 'lib' -or $crateTypes -contains 'rlib')
    })
    if ($metadataTargetMatches.Count -ne 1) {
        throw "Expected exactly one Cargo library target named '$TargetName' in package '$PackageName', found $($metadataTargetMatches.Count)"
    }
    $expectedPackageId = [string]$packageMatches[0].id
    if ([string]::IsNullOrWhiteSpace($expectedPackageId)) {
        throw "Cargo metadata returned an empty package ID for '$PackageName'"
    }

    try {
        $ErrorActionPreference = 'Continue'
        $cargoOutput = @(& cargo @CargoArguments '--no-run' '--message-format=json-render-diagnostics' 2>&1 |
            ForEach-Object { $_.ToString() })
        $cargoExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $cargoOutput | Where-Object { $_ -notmatch '^\{' } | ForEach-Object { Write-Host $_ }
    if ($cargoExitCode -ne 0) {
        throw "cargo test --no-run failed with exit code $cargoExitCode"
    }

    $testExecutables = @($cargoOutput | ForEach-Object {
        if ($_ -notmatch '^\{') {
            return
        }
        $message = $_ | ConvertFrom-Json -ErrorAction SilentlyContinue
        if ($null -eq $message -or $message.reason -ne 'compiler-artifact') {
            return
        }
        $targetKinds = @($message.target.kind)
        $targetCrateTypes = @($message.target.crate_types)
        if ($message.profile.test -eq $true -and
            $message.executable -and
            [string]::Equals([string]$message.package_id, $expectedPackageId, [StringComparison]::Ordinal) -and
            [string]::Equals([string]$message.target.name, $TargetName, [StringComparison]::Ordinal) -and
            ($targetKinds -contains 'lib' -or $targetKinds -contains 'rlib') -and
            ($targetCrateTypes -contains 'lib' -or $targetCrateTypes -contains 'rlib')) {
            [string]$message.executable
        }
    } | Select-Object -Unique)
    if ($testExecutables.Count -ne 1) {
        throw "Expected exactly one Cargo lib-test executable for package '$PackageName' target '$TargetName', found $($testExecutables.Count): $($testExecutables -join ', ')"
    }
    if (-not (Test-Path -LiteralPath $testExecutables[0] -PathType Leaf)) {
        throw "Cargo lib-test executable does not exist: $($testExecutables[0])"
    }

    $kitsBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $mt = Get-ChildItem -LiteralPath $kitsBin -Directory |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName 'x64\mt.exe' } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1
    if (-not $mt) {
        throw "Cannot prepare the Windows test executable because x64 mt.exe is missing under: $kitsBin"
    }
    try {
        $ErrorActionPreference = 'Continue'
        & $mt -nologo -manifest $ManifestPath "-outputresource:$($testExecutables[0]);#1"
        $mtExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($mtExitCode -ne 0) {
        throw "mt.exe could not embed the test application manifest in: $($testExecutables[0])"
    }

    try {
        $ErrorActionPreference = 'Continue'
        $listedTests = @(& $testExecutables[0] $TestFilter '--list' 2>&1 |
            ForEach-Object { $_.ToString() })
        $listExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($listExitCode -ne 0) {
        throw "Windows Rust test executable could not list tests (exit $listExitCode)"
    }
    $matchingTests = @(if ([string]::IsNullOrEmpty($TestFilter)) {
        $listedTests | Where-Object { $_ -match ': test$' }
    } else {
        $listedTests | Where-Object {
            $_.StartsWith($TestFilter, [StringComparison]::Ordinal)
        }
    })
    if ($matchingTests.Count -eq 0) {
        throw "Rust test filter matched zero tests: $TestFilter"
    }
    foreach ($requiredTestName in $RequiredTestNames) {
        $listedName = "${requiredTestName}: test"
        if (-not ($listedTests -contains $listedName)) {
            throw "Required Rust test was not discovered: $requiredTestName"
        }
    }

    try {
        $ErrorActionPreference = 'Continue'
        if ([string]::IsNullOrEmpty($TestFilter)) {
            & $testExecutables[0] '--nocapture'
        } else {
            & $testExecutables[0] $TestFilter '--nocapture'
        }
        $testExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($testExitCode -ne 0) {
        throw "Windows Rust test executable failed with exit code $testExitCode"
    }
}

function Get-DepthProbeArtifactSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    return 'sha256:' + (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-DepthProbeArtifactJsonShape {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string[]]$ExpectedProperties,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $actual = @($Object.PSObject.Properties.Name | Sort-Object)
    $expected = @($ExpectedProperties | Sort-Object)
    if (@(Compare-Object -ReferenceObject $expected -DifferenceObject $actual).Count -ne 0) {
        throw "$Label has an unexpected JSON shape. Expected '$($expected -join ', ')', got '$($actual -join ', ')'."
    }
}

function Stage-LockedDepthProbeArtifact {
    param(
        [Parameter(Mandatory = $true)][string]$ArtifactDirectory,
        [Parameter(Mandatory = $true)][string]$DestinationDirectory
    )

    $artifactRoot = (Resolve-Path -LiteralPath $ArtifactDirectory).Path
    $lockPath = Join-Path $artifactRoot 'artifact-lock.json'
    if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
        throw "Depth probe artifact lock is missing: $lockPath"
    }
    $lock = Get-Content -Raw -Encoding UTF8 -LiteralPath $lockPath | ConvertFrom-Json
    Assert-DepthProbeArtifactJsonShape -Object $lock `
        -ExpectedProperties @('schemaVersion', 'artifactId', 'files') `
        -Label 'Depth probe artifact lock'
    if ([string]$lock.schemaVersion -ne 'dm-depth-probe-artifact-lock-v1') {
        throw "Unsupported Depth probe artifact lock schema: $($lock.schemaVersion)"
    }
    if ([string]::IsNullOrWhiteSpace([string]$lock.artifactId) -or
        [string]$lock.artifactId -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') {
        throw 'Depth probe artifactId is empty or unsafe.'
    }

    $requiredNames = @(
        'dm_depth_probe.dll',
        'dm-depth-inject.exe',
        'dm-depth-launch.exe',
        'dm-depth-extract.exe',
        'dm-depth-probe-runtime.json'
    )
    $files = @($lock.files)
    if ($files.Count -ne $requiredNames.Count) {
        throw "Depth probe artifact lock must contain exactly $($requiredNames.Count) files."
    }
    foreach ($entry in $files) {
        Assert-DepthProbeArtifactJsonShape -Object $entry `
            -ExpectedProperties @('name', 'size', 'sha256') `
            -Label 'Depth probe artifact file entry'
        if ([string]$entry.name -notin $requiredNames) {
            throw "Depth probe artifact lock contains an unexpected file: $($entry.name)"
        }
        if ([string]$entry.sha256 -cnotmatch '^sha256:[0-9a-f]{64}$') {
            throw "Depth probe artifact lock has an invalid SHA-256 for $($entry.name)"
        }
        if ([long]$entry.size -le 0) {
            throw "Depth probe artifact lock has a non-positive size for $($entry.name)"
        }
    }
    if (@($files.name | Select-Object -Unique).Count -ne $requiredNames.Count) {
        throw 'Depth probe artifact lock has duplicate or missing file names.'
    }

    foreach ($entry in $files) {
        $source = Join-Path $artifactRoot ([string]$entry.name)
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Locked Depth probe artifact file is missing: $source"
        }
        $item = Get-Item -LiteralPath $source
        if ([long]$item.Length -ne [long]$entry.size) {
            throw "Locked Depth probe artifact size mismatch for ${source}: expected $($entry.size), got $($item.Length)"
        }
        $actualSha256 = Get-DepthProbeArtifactSha256 -Path $source
        if ($actualSha256 -cne [string]$entry.sha256) {
            throw "Locked Depth probe artifact SHA-256 mismatch for ${source}: expected $($entry.sha256), got $actualSha256"
        }
    }

    $runtimePath = Join-Path $artifactRoot 'dm-depth-probe-runtime.json'
    $runtime = Get-Content -Raw -Encoding UTF8 -LiteralPath $runtimePath | ConvertFrom-Json
    Assert-DepthProbeArtifactJsonShape -Object $runtime `
        -ExpectedProperties @('schema', 'runtimeVersion', 'dllSha256', 'd3d11Generation', 'd3d12Generation') `
        -Label 'Depth probe runtime manifest'
    if ([string]$runtime.schema -ne 'dm-depth-probe-runtime-manifest-v1') {
        throw "Unsupported Depth probe runtime manifest schema: $($runtime.schema)"
    }
    if ([string]::IsNullOrWhiteSpace([string]$runtime.runtimeVersion)) {
        throw 'Depth probe runtime version must not be empty.'
    }
    if ([long]$runtime.d3d11Generation -le 0 -or [long]$runtime.d3d12Generation -le 0) {
        throw 'Depth probe runtime D3D generations must be positive.'
    }
    $dllEntry = @($files | Where-Object name -eq 'dm_depth_probe.dll')
    if ($dllEntry.Count -ne 1 -or [string]$runtime.dllSha256 -cne [string]$dllEntry[0].sha256) {
        throw 'Depth probe runtime manifest DLL SHA-256 does not match artifact-lock.json.'
    }

    New-Item -ItemType Directory -Path $DestinationDirectory -Force | Out-Null
    foreach ($entry in $files) {
        Copy-Item -LiteralPath (Join-Path $artifactRoot ([string]$entry.name)) `
            -Destination (Join-Path $DestinationDirectory ([string]$entry.name)) -Force
    }
    return $runtime
}

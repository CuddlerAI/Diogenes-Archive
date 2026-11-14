[CmdletBinding()]
param(
    [string]$CargoPath,
    [string]$Toolchain = 'stable-x86_64-pc-windows-msvc',
    [string]$Target = 'x86_64-pc-windows-msvc'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$toolsRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dataMinerRoot = (Resolve-Path (Join-Path $toolsRoot '..')).Path

if ([string]::IsNullOrWhiteSpace($CargoPath)) {
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    if ($null -ne $cargoCommand) {
        $CargoPath = $cargoCommand.Source
    } else {
        $CargoPath = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo\bin\cargo.exe'
    }
}

if (-not (Test-Path -LiteralPath $CargoPath -PathType Leaf)) {
    throw "cargo executable not found: $CargoPath"
}

function Invoke-CargoProcess {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $allArguments = @()
    if (-not [string]::IsNullOrWhiteSpace($Toolchain)) {
        $allArguments += "+$Toolchain"
    }
    $allArguments += $Arguments

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $CargoPath
    $startInfo.Arguments = $allArguments -join ' '
    $startInfo.WorkingDirectory = $dataMinerRoot
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "failed to start cargo: $CargoPath"
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.Result
        $stderr = $stderrTask.Result

        if ($process.ExitCode -ne 0) {
            throw "cargo $($allArguments -join ' ') failed with exit code $($process.ExitCode):`n$stderr"
        }
        if (-not [string]::IsNullOrWhiteSpace($stderr)) {
            Write-Verbose $stderr.Trim()
        }
        return $stdout
    } finally {
        $process.Dispose()
    }
}

function Get-CargoMetadata {
    param(
        [string[]]$FeatureArguments = @(),
        [string]$ManifestPath
    )

    $arguments = @(
        'metadata',
        '--locked',
        '--format-version', '1',
        '--filter-platform', $Target
    )
    if (-not [string]::IsNullOrWhiteSpace($ManifestPath)) {
        $arguments += @('--manifest-path', $ManifestPath)
    }
    $arguments += $FeatureArguments
    $json = Invoke-CargoProcess -Arguments $arguments
    try {
        return $json | ConvertFrom-Json
    } catch {
        throw "cargo metadata returned invalid JSON: $($_.Exception.Message)"
    }
}

function Get-WorkspacePackage {
    param(
        [Parameter(Mandatory = $true)]$Metadata,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $matches = @(
        $Metadata.packages |
            Where-Object { $_.name -eq $Name -and $Metadata.workspace_members -contains $_.id }
    )
    if ($matches.Count -ne 1) {
        throw "expected one workspace package named '$Name', found $($matches.Count)"
    }
    return $matches[0]
}

function Get-ResolvedClosure {
    param(
        [Parameter(Mandatory = $true)]$Metadata,
        [Parameter(Mandatory = $true)][string]$RootPackageName
    )

    $rootPackage = Get-WorkspacePackage -Metadata $Metadata -Name $RootPackageName
    $nodesById = @{}
    foreach ($node in $Metadata.resolve.nodes) {
        $nodesById[$node.id] = $node
    }
    $packagesById = @{}
    foreach ($package in $Metadata.packages) {
        $packagesById[$package.id] = $package
    }

    $pending = New-Object 'System.Collections.Generic.Stack[string]'
    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    $result = New-Object System.Collections.ArrayList
    $pending.Push($rootPackage.id)
    while ($pending.Count -gt 0) {
        $id = $pending.Pop()
        if (-not $seen.Add($id)) {
            continue
        }
        if (-not $nodesById.ContainsKey($id) -or -not $packagesById.ContainsKey($id)) {
            throw "metadata resolve graph is missing package id: $id"
        }
        [void]$result.Add($packagesById[$id])
        foreach ($dependencyId in $nodesById[$id].dependencies) {
            $pending.Push($dependencyId)
        }
    }
    return $result.ToArray()
}

$failures = New-Object 'System.Collections.Generic.List[string]'

function Assert-Graph {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        $failures.Add($Message)
    }
}

function Assert-ZstdFamilyCount {
    param(
        [Parameter(Mandatory = $true)]$Metadata,
        [Parameter(Mandatory = $true)][string]$RootPackageName,
        [Parameter(Mandatory = $true)][int]$ExpectedCount,
        [Parameter(Mandatory = $true)][string]$CaseName
    )

    $closure = @(Get-ResolvedClosure -Metadata $Metadata -RootPackageName $RootPackageName)
    foreach ($packageName in @('zstd', 'zstd-safe', 'zstd-sys')) {
        $actualCount = @($closure | Where-Object name -eq $packageName).Count
        Assert-Graph -Condition ($actualCount -eq $ExpectedCount) `
            -Message "${CaseName}: expected $ExpectedCount $packageName package(s), found $actualCount"
    }
}

$defaultMetadata = Get-CargoMetadata
$depthPackage = Get-WorkspacePackage -Metadata $defaultMetadata -Name 'depth-lab'
$probePackage = Get-WorkspacePackage -Metadata $defaultMetadata -Name 'dm-depth-probe'
$depthZstdDependencies = @($depthPackage.dependencies | Where-Object name -eq 'zstd')
$probeDepthDependencies = @($probePackage.dependencies | Where-Object name -eq 'depth-lab')

Assert-Graph -Condition ($depthZstdDependencies.Count -eq 1) `
    -Message "depth-lab must declare exactly one direct zstd dependency"
if ($depthZstdDependencies.Count -eq 1) {
    Assert-Graph -Condition ([bool]$depthZstdDependencies[0].optional) `
        -Message "depth-lab's direct zstd dependency must be optional"
}

$clientOrchestration = @($depthPackage.features.'client-orchestration')
Assert-Graph -Condition ($clientOrchestration -contains 'dep:zstd') `
    -Message "depth-lab/client-orchestration must enable dep:zstd"

Assert-Graph -Condition ($probeDepthDependencies.Count -eq 1) `
    -Message "dm-depth-probe must declare exactly one direct depth-lab dependency"
if ($probeDepthDependencies.Count -eq 1) {
    Assert-Graph -Condition (-not [bool]$probeDepthDependencies[0].uses_default_features) `
        -Message "dm-depth-probe must disable depth-lab default features"
}

$workspaceZstdRequirements = @(
    $defaultMetadata.packages |
        Where-Object { $defaultMetadata.workspace_members -contains $_.id } |
        ForEach-Object { $_.dependencies | Where-Object name -eq 'zstd' } |
        ForEach-Object { $_.req }
)
$distinctZstdRequirements = @($workspaceZstdRequirements | Sort-Object -Unique)
Assert-Graph -Condition ($workspaceZstdRequirements.Count -gt 0) `
    -Message "the workspace must contain at least one direct zstd dependency"
Assert-Graph -Condition ($distinctZstdRequirements.Count -eq 1) `
    -Message "workspace zstd requirements drifted: $($distinctZstdRequirements -join ', ')"

Assert-ZstdFamilyCount -Metadata $defaultMetadata -RootPackageName 'depth-lab' `
    -ExpectedCount 0 -CaseName 'depth-lab default features'

$noDefaultMetadata = Get-CargoMetadata -FeatureArguments @('--no-default-features')
Assert-ZstdFamilyCount -Metadata $noDefaultMetadata -RootPackageName 'depth-lab' `
    -ExpectedCount 0 -CaseName 'depth-lab no default features'
Assert-ZstdFamilyCount -Metadata $noDefaultMetadata -RootPackageName 'dm-depth-probe' `
    -ExpectedCount 0 -CaseName 'dm-depth-probe'

$depthFeatureMetadata = Get-CargoMetadata -ManifestPath 'tools/depth-lab/Cargo.toml' `
    -FeatureArguments @(
    '--no-default-features',
    '--features', 'exr-out,ets2'
)
Assert-ZstdFamilyCount -Metadata $depthFeatureMetadata -RootPackageName 'depth-lab' `
    -ExpectedCount 0 -CaseName 'depth-lab exr-out + ets2 features'

$clientMetadata = Get-CargoMetadata -ManifestPath 'tools/depth-lab/Cargo.toml' `
    -FeatureArguments @(
    '--no-default-features',
    '--features', 'client-orchestration'
)
Assert-ZstdFamilyCount -Metadata $clientMetadata -RootPackageName 'depth-lab' `
    -ExpectedCount 1 -CaseName 'depth-lab client-orchestration feature'

$allFeaturesMetadata = Get-CargoMetadata -FeatureArguments @('--all-features')
foreach ($packageName in @('zstd', 'zstd-safe', 'zstd-sys')) {
    $packageCount = @($allFeaturesMetadata.packages | Where-Object name -eq $packageName).Count
    Assert-Graph -Condition ($packageCount -eq 1) `
        -Message "MSVC all-features graph must resolve exactly one $packageName package, found $packageCount"
}

if ($failures.Count -gt 0) {
    throw "Depth feature graph guard failed:`n - $($failures -join "`n - ")"
}

Write-Host 'Depth feature graph guard passed for default, depth, probe, client, and MSVC all-features combinations.'

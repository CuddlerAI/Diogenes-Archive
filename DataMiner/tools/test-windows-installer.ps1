[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PreviousInstallerPath,
    [Parameter(Mandatory = $true)][string]$CurrentInstallerPath,
    [Parameter(Mandatory = $true)][string]$ProductName,
    [Parameter(Mandatory = $true)][string]$Identifier,
    [Parameter(Mandatory = $true)][string]$PreviousVersion,
    [Parameter(Mandatory = $true)][string]$CurrentVersion,
    [string]$SentinelId,
    [switch]$LaunchApplication,
    [switch]$ConfirmInstallCycle
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Same bootstrap as build-windows-installer.ps1: Assert-Unsigned below must never be
# skipped just because the security module failed to autoload.
. (Join-Path $PSScriptRoot 'ensure-authenticode.ps1')

if (-not $ConfirmInstallCycle) {
    throw 'Installer mutation is disabled. Re-run with -ConfirmInstallCycle after reviewing the resolved targets.'
}
$stableProductName = 'DataMiner'
$stableIdentifier = 'com.diogeneslab.dataminer'
$testProductPrefix = 'DataMiner UNSIGNED DEV T13 TEST '
$testIdentifierPrefix = 'com.diogeneslab.dataminer.t13test'
$isStableIdentity = $ProductName -ceq $stableProductName
if ($isStableIdentity) {
    if ($Identifier -cne $stableIdentifier) {
        throw "Stable ProductName '$stableProductName' must use the exact identifier '$stableIdentifier'."
    }
    if ($LaunchApplication) {
        throw 'Launching the final identity could inspect or update real DataMiner app-local data; use an isolated T13 test package.'
    }
} else {
    if ($ProductName -cnotmatch '^DataMiner UNSIGNED DEV T13 TEST [A-Za-z0-9-]+$') {
        throw "Refusing an unexpected installer identity: $ProductName"
    }
    if ($Identifier -cnotmatch '^com\.diogeneslab\.dataminer\.t13test[A-Za-z0-9-]+$') {
        throw "Refusing an unexpected isolated test identifier: $Identifier"
    }
    $productSuffix = $ProductName.Substring($testProductPrefix.Length)
    $identifierSuffix = $Identifier.Substring($testIdentifierPrefix.Length)
    if (-not $productSuffix.Equals($identifierSuffix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Isolated ProductName suffix '$productSuffix' must match identifier suffix '$identifierSuffix'."
    }
}
if (-not $SentinelId) {
    $SentinelId = 't13-' + [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds().ToString()
}
if ($SentinelId -notmatch '^t13-[A-Za-z0-9-]+$') {
    throw 'SentinelId must start with t13- and contain only letters, digits, and hyphens.'
}

$previousInstaller = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
$currentInstaller = (Resolve-Path -LiteralPath $CurrentInstallerPath).Path
$localAppData = [System.IO.Path]::GetFullPath($env:LOCALAPPDATA).TrimEnd('\')
$installRoot = [System.IO.Path]::GetFullPath((Join-Path $localAppData $ProductName))
$appDataRoot = [System.IO.Path]::GetFullPath((Join-Path $localAppData $Identifier))
$sentinelRoot = [System.IO.Path]::GetFullPath((Join-Path $appDataRoot "installer-validation\$SentinelId"))
$uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductName"
$evidenceRoot = Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..')).Path 'tools\installer-tests\out'
$evidencePath = Join-Path $evidenceRoot "$SentinelId.json"

function Assert-StrictChildPath {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Child
    )

    $parentFull = [System.IO.Path]::GetFullPath($Parent).TrimEnd('\') + '\'
    $childFull = [System.IO.Path]::GetFullPath($Child)
    if (-not $childFull.StartsWith($parentFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe target outside parent '$Parent': $Child"
    }
}

function Assert-Unsigned {
    param([Parameter(Mandatory = $true)][string]$Path)

    $status = (Get-AuthenticodeSignature -LiteralPath $Path).Status
    if ($status -ne [System.Management.Automation.SignatureStatus]::NotSigned) {
        throw "Expected unsigned development artifact, got '$status': $Path"
    }
}

function Assert-InstallerMetadata {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Version
    )

    $versionInfo = (Get-Item -LiteralPath $Path).VersionInfo
    if ([string]$versionInfo.ProductName -cne $ProductName -or
        [string]$versionInfo.FileDescription -cne $ProductName) {
        throw "Installer metadata identity does not match '$ProductName': $Path"
    }
    if ([string]$versionInfo.ProductVersion -ne $Version) {
        throw "Installer ProductVersion does not match requested version '$Version': $Path"
    }
}

function Invoke-ProcessChecked {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [int]$TimeoutMilliseconds = 120000
    )

    $process = Start-Process -FilePath $Path -ArgumentList $Arguments -PassThru -WindowStyle Hidden
    if (-not $process.WaitForExit($TimeoutMilliseconds)) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "Process timed out: $Path $($Arguments -join ' ')"
    }
    if ($process.ExitCode -ne 0) {
        throw "Process exited with $($process.ExitCode): $Path $($Arguments -join ' ')"
    }
}

function Get-InstallRegistration {
    if (-not (Test-Path -LiteralPath $uninstallKey)) {
        throw "Expected uninstall registration is missing: $uninstallKey"
    }
    return Get-ItemProperty -LiteralPath $uninstallKey
}

function Assert-InstalledVersion {
    param([Parameter(Mandatory = $true)][string]$Version)

    $registration = Get-InstallRegistration
    if ([string]$registration.DisplayVersion -ne $Version) {
        throw "Expected installed version '$Version', got '$($registration.DisplayVersion)'."
    }
    $registeredRoot = ([string]$registration.InstallLocation).Trim('"')
    if (-not $registeredRoot.Equals($installRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Unexpected install location '$registeredRoot'; expected '$installRoot'."
    }
    $client = Join-Path $installRoot 'dataminer-client.exe'
    if (-not (Test-Path -LiteralPath $client -PathType Leaf)) {
        throw "Installed client binary is missing: $client"
    }
    $clientBytes = [System.IO.File]::ReadAllBytes($client)
    $peOffset = [System.BitConverter]::ToInt32($clientBytes, 0x3c)
    $subsystem = [System.BitConverter]::ToUInt16($clientBytes, $peOffset + 24 + 68)
    if ($subsystem -ne 2) {
        throw "Installed client is not a Windows GUI executable (subsystem=$subsystem)."
    }
    $unexpectedExecutables = @(Get-ChildItem -LiteralPath $installRoot -Recurse -File -Filter '*.exe' |
        Where-Object { $_.Name -notin @('dataminer-client.exe', 'uninstall.exe') })
    if ($unexpectedExecutables.Count -gt 0) {
        throw "Unexpected bundled helper executable(s): $($unexpectedExecutables.FullName -join ', ')"
    }
    return $client
}

function Invoke-AppHealthCheck {
    param([Parameter(Mandatory = $true)][string]$ClientPath)

    if (Get-NetTCPConnection -State Listen -LocalPort 1421 -ErrorAction SilentlyContinue) {
        throw 'Port 1421 already has a listener; stop the dev server before installer health validation.'
    }
    $healthCheckBudgetMs = 15000
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $process = Start-Process -FilePath $ClientPath -ArgumentList '--dev-installer-smoke-exit-ms=3000' -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 1200
    if ($process.HasExited) {
        throw "Installed client exited too early with code $($process.ExitCode)."
    }
    $processTree = @($process.Id)
    $frontier = @($process.Id)
    while ($frontier.Count -gt 0) {
        $parents = @($frontier)
        $children = @(Get-CimInstance Win32_Process |
            Where-Object { $parents -contains [int]$_.ParentProcessId })
        $frontier = @($children | ForEach-Object { [int]$_.ProcessId } |
            Where-Object { $processTree -notcontains $_ })
        $processTree += $frontier
    }
    $treeProcesses = @(Get-CimInstance Win32_Process |
        Where-Object { $processTree -contains [int]$_.ProcessId })
    $unexpectedChildren = @($treeProcesses |
        Where-Object { $_.Name -notin @('dataminer-client.exe', 'msedgewebview2.exe') })
    if ($unexpectedChildren.Count -gt 0) {
        throw "Unexpected business/helper process: $($unexpectedChildren.Name -join ', ')"
    }
    $devConnections = @(Get-NetTCPConnection -ErrorAction SilentlyContinue |
        Where-Object {
            $processTree -contains [int]$_.OwningProcess -and
            ($_.LocalPort -eq 1421 -or $_.RemotePort -eq 1421)
        })
    if ($devConnections.Count -gt 0) {
        throw 'Installed client process tree connected to the Vite dev-server port.'
    }
    $remainingMs = [Math]::Max(0, $healthCheckBudgetMs - [int]$stopwatch.ElapsedMilliseconds)
    if (-not $process.WaitForExit($remainingMs)) {
        $stopwatch.Stop()
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "Installed client did not complete its bounded safe-exit smoke check within ${healthCheckBudgetMs}ms (elapsed=$($stopwatch.ElapsedMilliseconds)ms)."
    }
    $stopwatch.Stop()
    if ($process.ExitCode -ne 0) {
        throw "Installed client health check exited with code $($process.ExitCode)."
    }
    if ($stopwatch.ElapsedMilliseconds -lt 3000 -or
        $stopwatch.ElapsedMilliseconds -ge $healthCheckBudgetMs) {
        throw "Installed client safe-exit elapsed $($stopwatch.ElapsedMilliseconds)ms; expected [3000, ${healthCheckBudgetMs})ms."
    }
    return [ordered]@{
        pid = $process.Id
        exit_code = $process.ExitCode
        elapsed_ms = [long]$stopwatch.ElapsedMilliseconds
        deadline_ms = $healthCheckBudgetMs
        observed_processes = @($treeProcesses.Name | Sort-Object -Unique)
        dev_server_connections = 0
    }
}

Assert-StrictChildPath -Parent $localAppData -Child $installRoot
Assert-StrictChildPath -Parent $localAppData -Child $appDataRoot
Assert-StrictChildPath -Parent $appDataRoot -Child $sentinelRoot
if (Test-Path -LiteralPath $installRoot) {
    throw "Fresh-install target already exists; refusing to overwrite it: $installRoot"
}
if (Test-Path -LiteralPath $uninstallKey) {
    throw "Fresh-install registry key already exists; refusing to overwrite it: $uninstallKey"
}
if (-not $isStableIdentity -and (Test-Path -LiteralPath $appDataRoot)) {
    throw "Isolated test app-data root already exists; refusing to reuse it: $appDataRoot"
}
if (Test-Path -LiteralPath $sentinelRoot) {
    throw "Sentinel target already exists: $sentinelRoot"
}

Assert-Unsigned -Path $previousInstaller
Assert-Unsigned -Path $currentInstaller
Assert-InstallerMetadata -Path $previousInstaller -Version $PreviousVersion
Assert-InstallerMetadata -Path $currentInstaller -Version $CurrentVersion
if ([version]$CurrentVersion -le [version]$PreviousVersion) {
    throw 'A real upgrade requires CurrentVersion to be greater than PreviousVersion.'
}

New-Item -ItemType Directory -Path (Join-Path $sentinelRoot 'journal') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $sentinelRoot 'artifact') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $sentinelRoot 'logs') -Force | Out-Null
Set-Content -Encoding UTF8 -LiteralPath (Join-Path $sentinelRoot 'journal\journal.sqlite3.sentinel') -Value $SentinelId
Set-Content -Encoding UTF8 -LiteralPath (Join-Path $sentinelRoot 'artifact\ready-artifact.sentinel') -Value $SentinelId
Set-Content -Encoding UTF8 -LiteralPath (Join-Path $sentinelRoot 'logs\events.jsonl.sentinel') -Value $SentinelId
$sentinelHashes = @{}
Get-ChildItem -LiteralPath $sentinelRoot -Recurse -File | ForEach-Object {
    $relative = $_.FullName.Substring($sentinelRoot.Length + 1)
    $sentinelHashes[$relative] = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
}

Write-Host "Validated install target: $installRoot"
Write-Host "Validated uninstall key: $uninstallKey"
Write-Host "Isolated retention sentinel: $sentinelRoot"

Invoke-ProcessChecked -Path $previousInstaller -Arguments @('/S')
$previousClient = Assert-InstalledVersion -Version $PreviousVersion
$previousHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $previousClient).Hash.ToLowerInvariant()
$healthChecks = @()
if ($LaunchApplication) {
    $healthChecks += Invoke-AppHealthCheck -ClientPath $previousClient
}

Invoke-ProcessChecked -Path $currentInstaller -Arguments @('/S')
$currentClient = Assert-InstalledVersion -Version $CurrentVersion
$currentHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $currentClient).Hash.ToLowerInvariant()
if ($previousHash -eq $currentHash) {
    throw 'Upgrade did not replace the installed client binary.'
}
if ($LaunchApplication) {
    $healthChecks += Invoke-AppHealthCheck -ClientPath $currentClient
    $healthChecks += Invoke-AppHealthCheck -ClientPath $currentClient
}

$registration = Get-InstallRegistration
$uninstaller = ([string]$registration.UninstallString).Trim('"')
Assert-StrictChildPath -Parent $installRoot -Child $uninstaller
if (-not (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
    throw "Registered uninstaller is missing: $uninstaller"
}
Invoke-ProcessChecked -Path $uninstaller -Arguments @('/S')

$deadline = [DateTime]::UtcNow.AddSeconds(30)
while (((Test-Path -LiteralPath $installRoot) -or (Test-Path -LiteralPath $uninstallKey)) -and
    [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 250
}
if (Test-Path -LiteralPath $installRoot) {
    throw "Uninstall left the application directory behind: $installRoot"
}
if (Test-Path -LiteralPath $uninstallKey) {
    throw "Uninstall left the registration behind: $uninstallKey"
}
foreach ($entry in $sentinelHashes.GetEnumerator()) {
    $path = Join-Path $sentinelRoot $entry.Key
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Default uninstall removed retained data: $path"
    }
    $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actualHash -ne $entry.Value) {
        throw "Default uninstall modified retained data: $path"
    }
}

$evidence = [ordered]@{
    schema = 'dm-windows-installer-cycle-1'
    product_name = $ProductName
    identifier = $Identifier
    previous_version = $PreviousVersion
    current_version = $CurrentVersion
    fresh_install = 'passed'
    version_upgrade = 'passed'
    restart_equivalent_launches = $healthChecks.Count
    health_checks = $healthChecks
    installed_executable_surface = @('dataminer-client.exe', 'uninstall.exe')
    uninstall = 'passed'
    install_binary_removed = $true
    uninstall_registry_removed = $true
    app_data_retained = $true
    sentinel_root = $sentinelRoot
    sentinel_hashes = $sentinelHashes
}
New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
$evidence | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 -LiteralPath $evidencePath

if ($healthChecks.Count -gt 0) {
    Write-Host 'PASS: fresh install, real version upgrade, restart-equivalent launch, uninstall, and data retention.'
} else {
    Write-Host 'PASS: fresh install, real version upgrade, uninstall, and data retention (application launch intentionally skipped).'
}
Write-Host "Evidence: $evidencePath"
Write-Host "Retained sentinel: $sentinelRoot"

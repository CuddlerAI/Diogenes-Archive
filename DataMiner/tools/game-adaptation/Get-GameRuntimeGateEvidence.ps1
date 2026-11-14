<#
.SYNOPSIS
  Collect fail-closed runtime graphics and anti-cheat evidence for one game process.

.DESCRIPTION
  This tool is read-only. It does not inject, attach a debugger, read process memory, or
  record the process command line. It records executable identity, loaded module names,
  graphics-runtime hits, and matching Windows services/drivers in a reproducible JSON report.

  Exit code 0 means every requested gate passed. Exit code 2 means evidence collection was
  incomplete or a requested identity/graphics/anti-cheat gate failed. A clean report is not
  legal permission to inject; it only closes the runtime fact named by the report.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ProcessId,

    [string]$ExpectedExecutablePath = '',
    [string]$ExpectedExecutableSha256 = '',

    [ValidateSet('any', 'd3d11', 'd3d12', 'vulkan')]
    [string]$ExpectedGraphicsApi = 'any',

    [switch]$RequireNoLoadedAntiCheat,
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Normalize-Sha256 {
    param([string]$Value)
    $normalized = $Value.Trim().ToLowerInvariant()
    if ($normalized.StartsWith('sha256:')) { $normalized = $normalized.Substring(7) }
    if ($normalized -and $normalized -notmatch '^[0-9a-f]{64}$') {
        throw "ExpectedExecutableSha256 must be 64 lowercase or uppercase hex characters."
    }
    return $normalized
}

function Get-PortableExecutableMachine {
    param([string]$Path)
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try {
        if ($stream.Length -lt 64) { throw "Executable is too small to contain a PE header: $Path" }
        $reader = [IO.BinaryReader]::new($stream)
        try {
            if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Executable has no DOS MZ header: $Path" }
            $stream.Position = 0x3c
            $peOffset = $reader.ReadInt32()
            if ($peOffset -lt 0 -or ($peOffset + 6) -gt $stream.Length) {
                throw "Executable has an invalid PE header offset: $Path"
            }
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) { throw "Executable has no PE signature: $Path" }
            return $reader.ReadUInt16()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Test-AntiCheatName {
    param([string]$Value)
    if (-not $Value) { return $false }
    return $Value -match '(?i)(easy.?anti.?cheat|(?:^|[^a-z0-9])(?:eac(?:_launcher)?|battleye|beclient|belauncher|beservice|bedaisy|equ8|vgk|vgc)(?:[^a-z0-9]|$))'
}

function Write-AtomicUtf8Json {
    param([string]$Path, [string]$Json)
    $fullPath = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $fullPath) {
        throw "Refusing to overwrite existing runtime evidence: $fullPath"
    }
    $directory = [IO.Path]::GetDirectoryName($fullPath)
    if (-not $directory) { $directory = (Get-Location).Path }
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $temporary = "$fullPath.tmp-$([Guid]::NewGuid().ToString('N'))"
    try {
        [IO.File]::WriteAllText($temporary, $Json, [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $temporary -Destination $fullPath
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

$expectedHash = Normalize-Sha256 -Value $ExpectedExecutableSha256
$process = Get-Process -Id $ProcessId -ErrorAction Stop
$cimProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction Stop
$actualPath = [string]$cimProcess.ExecutablePath
if (-not $actualPath -or -not (Test-Path -LiteralPath $actualPath -PathType Leaf)) {
    throw "Process $ProcessId has no readable executable path."
}
$actualPath = [IO.Path]::GetFullPath($actualPath)
$actualHash = (Get-FileHash -LiteralPath $actualPath -Algorithm SHA256).Hash.ToLowerInvariant()
$targetPeMachine = Get-PortableExecutableMachine -Path $actualPath

# A 64-bit PowerShell host can return only the WOW64 bridge modules for a 32-bit target,
# which would turn a real D3D11 process into a false blocked report. Re-run the complete
# collector under the system 32-bit Windows PowerShell host so Process.Modules sees the
# target's native 32-bit module list. The child inherits the current working directory and
# preserves the same output/no-overwrite and exit-code contract.
if ([Environment]::Is64BitOperatingSystem -and [Environment]::Is64BitProcess -and $targetPeMachine -eq 0x014c) {
    $x86PowerShell = Join-Path $env:WINDIR 'SysWOW64\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $x86PowerShell -PathType Leaf)) {
        throw "Cannot enumerate 32-bit target modules: x86 Windows PowerShell is missing at $x86PowerShell"
    }
    $forwardedArguments = @(
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', $PSCommandPath,
        '-ProcessId', [string]$ProcessId,
        '-ExpectedGraphicsApi', $ExpectedGraphicsApi
    )
    if ($ExpectedExecutablePath) {
        $forwardedArguments += @('-ExpectedExecutablePath', [IO.Path]::GetFullPath($ExpectedExecutablePath))
    }
    if ($expectedHash) {
        $forwardedArguments += @('-ExpectedExecutableSha256', $expectedHash)
    }
    if ($RequireNoLoadedAntiCheat) {
        $forwardedArguments += '-RequireNoLoadedAntiCheat'
    }
    if ($OutputPath) {
        $forwardedArguments += @('-OutputPath', [IO.Path]::GetFullPath($OutputPath))
    }
    & $x86PowerShell @forwardedArguments
    exit $LASTEXITCODE
}

$targetPeMachineName = switch ($targetPeMachine) {
    0x014c { 'i386' }
    0x8664 { 'amd64' }
    0xaa64 { 'arm64' }
    default { ('0x{0:x4}' -f $targetPeMachine) }
}
$collectorProcessArchitecture = $(if ([Environment]::Is64BitProcess) { 'x64' } else { 'x86' })

$moduleEnumerationError = $null
$moduleNames = @()
try {
    $moduleNames = @(
        $process.Modules |
            ForEach-Object { [string]$_.ModuleName } |
            Where-Object { $_ } |
            Sort-Object -Unique
    )
} catch {
    $moduleEnumerationError = $_.Exception.Message
}

$graphicsNames = @('d3d11.dll', 'd3d12.dll', 'dxgi.dll', 'vulkan-1.dll')
$graphicsHits = @($moduleNames | Where-Object { $graphicsNames -contains $_.ToLowerInvariant() })
$antiCheatModuleHits = @($moduleNames | Where-Object { Test-AntiCheatName -Value $_ })

$serviceEnumerationError = $null
$serviceHits = @()
try {
    $serviceHits = @(
        Get-CimInstance Win32_Service |
            Where-Object {
                (Test-AntiCheatName -Value ([string]$_.Name)) -or
                (Test-AntiCheatName -Value ([string]$_.DisplayName)) -or
                (Test-AntiCheatName -Value ([string]$_.PathName))
            } |
            Sort-Object Name |
            ForEach-Object {
                [ordered]@{
                    name = [string]$_.Name
                    display_name = [string]$_.DisplayName
                    state = [string]$_.State
                    start_mode = [string]$_.StartMode
                }
            }
    )
} catch {
    $serviceEnumerationError = $_.Exception.Message
}

$driverEnumerationError = $null
$driverHits = @()
try {
    $driverHits = @(
        Get-CimInstance Win32_SystemDriver |
            Where-Object {
                (Test-AntiCheatName -Value ([string]$_.Name)) -or
                (Test-AntiCheatName -Value ([string]$_.DisplayName)) -or
                (Test-AntiCheatName -Value ([string]$_.PathName))
            } |
            Sort-Object Name |
            ForEach-Object {
                [ordered]@{
                    name = [string]$_.Name
                    display_name = [string]$_.DisplayName
                    state = [string]$_.State
                    start_mode = [string]$_.StartMode
                }
            }
    )
} catch {
    $driverEnumerationError = $_.Exception.Message
}

$blocking = [Collections.Generic.List[string]]::new()
if ($moduleEnumerationError) {
    $blocking.Add("module_enumeration_failed: $moduleEnumerationError")
}
if ($serviceEnumerationError) {
    $blocking.Add("service_enumeration_failed: $serviceEnumerationError")
}
if ($driverEnumerationError) {
    $blocking.Add("driver_enumeration_failed: $driverEnumerationError")
}
if ($ExpectedExecutablePath) {
    $expectedPath = [IO.Path]::GetFullPath($ExpectedExecutablePath)
    if (-not $actualPath.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        $blocking.Add("executable_path_mismatch: expected=$expectedPath actual=$actualPath")
    }
}
if ($expectedHash -and $actualHash -ne $expectedHash) {
    $blocking.Add("executable_sha256_mismatch: expected=$expectedHash actual=$actualHash")
}

$requiredGraphicsModule = switch ($ExpectedGraphicsApi) {
    'd3d11' { 'd3d11.dll' }
    'd3d12' { 'd3d12.dll' }
    'vulkan' { 'vulkan-1.dll' }
    default { '' }
}
if ($requiredGraphicsModule -and -not ($moduleNames -contains $requiredGraphicsModule)) {
    $blocking.Add("expected_graphics_module_missing: $requiredGraphicsModule")
}

$runningServiceHits = @($serviceHits | Where-Object { $_.state -eq 'Running' })
$runningDriverHits = @($driverHits | Where-Object { $_.state -eq 'Running' })
if ($RequireNoLoadedAntiCheat) {
    foreach ($name in $antiCheatModuleHits) {
        $blocking.Add("anti_cheat_module_loaded: $name")
    }
    foreach ($entry in $runningServiceHits) {
        $blocking.Add("anti_cheat_service_running: $($entry.name)")
    }
    foreach ($entry in $runningDriverHits) {
        $blocking.Add("anti_cheat_driver_running: $($entry.name)")
    }
}

$collectorHash = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
$report = [ordered]@{
    schema = 'game-runtime-gate-evidence/v1'
    captured_utc = [DateTime]::UtcNow.ToString('o')
    host = [string]$env:COMPUTERNAME
    collector_sha256 = "sha256:$collectorHash"
    process = [ordered]@{
        process_id = $ProcessId
        process_name = [string]$process.ProcessName
        executable_path = $actualPath
        executable_sha256 = "sha256:$actualHash"
        start_time_utc = $process.StartTime.ToUniversalTime().ToString('o')
    }
    module_enumeration = [ordered]@{
        complete = -not [bool]$moduleEnumerationError
        error = $moduleEnumerationError
        collector_process_architecture = $collectorProcessArchitecture
        target_pe_machine = $targetPeMachineName
        count = $moduleNames.Count
        names = $moduleNames
    }
    graphics = [ordered]@{
        expected_api = $ExpectedGraphicsApi
        hits = $graphicsHits
    }
    anti_cheat = [ordered]@{
        require_none_loaded = [bool]$RequireNoLoadedAntiCheat
        module_hits = $antiCheatModuleHits
        service_enumeration_complete = -not [bool]$serviceEnumerationError
        service_enumeration_error = $serviceEnumerationError
        service_hits = $serviceHits
        driver_enumeration_complete = -not [bool]$driverEnumerationError
        driver_enumeration_error = $driverEnumerationError
        driver_hits = $driverHits
        running_service_hits = $runningServiceHits.Count
        running_driver_hits = $runningDriverHits.Count
    }
    status = $(if ($blocking.Count -eq 0) { 'pass' } else { 'blocked' })
    blocking = @($blocking)
    limitations = @(
        'This report is a point-in-time runtime observation, not legal permission to inject.',
        'A stopped service or absent install-tree file does not prove a future launch will remain anti-cheat-free.',
        'The process command line and process memory are intentionally not collected.'
    )
}

$json = $report | ConvertTo-Json -Depth 10
if ($OutputPath) { Write-AtomicUtf8Json -Path $OutputPath -Json $json }
$json
if ($blocking.Count -ne 0) { exit 2 }
exit 0

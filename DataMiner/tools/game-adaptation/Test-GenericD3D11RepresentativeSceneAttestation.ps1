<#
.SYNOPSIS
  Validates one local operator representative-scene attestation for generic D3D11 discovery.

.DESCRIPTION
  The attestation is an anti-misuse control, not machine vision or a proof of scene semantics.
  Validation binds the exact attestation bytes, game/build/executable, arm runtime evidence,
  live process instance, and current top-level window class/client size. Unknown fields, stale
  timestamps, byte drift, and identity drift fail closed.
#>
[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$AttestationPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedAttestationSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^\d+$')]
    [string]$AppId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^\d+$')]
    [string]$BuildId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$ExpectedDiscoveryId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$ExpectedOutputLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^[^\\/:*?"<>|]{1,260}\.exe$')]
    [string]$ExecutableBasename,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExecutableSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$RuntimeEvidencePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ExpectedProcessId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$ExpectedProcessStartTimeUtc,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidateLength(1, 256)]
    [string]$ExpectedWindowClass,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidateRange(1, 16384)]
    [uint32]$ExpectedClientWidth,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidateRange(1, 16384)]
    [uint32]$ExpectedClientHeight,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidateRange(1, 600)]
    [uint32]$MaximumAgeSeconds,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:AttestationSchema = 'generic-d3d11-representative-scene-attestation/v1'
$script:AttestationKind = 'local_operator'
$script:SceneKind = 'representative_offline_single_player'
$script:MaximumAttestationBytes = 64KB

function Assert-NoReparseAncestor {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$Label = 'path')
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label contains an NTFS reparse point: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
}

function Read-VerifiedUtf8Json {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [string]$ExpectedSha256 = '',
        [string]$Label = 'JSON'
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    $full = (Resolve-Path -LiteralPath $Path).Path
    if (-not ([IO.Path]::GetExtension($full)).Equals('.json', [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must be a .json file."
    }
    Assert-NoReparseAncestor -Path $full -Label $Label
    $stream = [IO.File]::Open($full, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -lt 2 -or $stream.Length -gt $script:MaximumAttestationBytes) {
            throw "$Label byte length is outside the admitted bound."
        }
        $bytes = New-Object byte[] ([int]$stream.Length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -le 0) { throw "$Label ended before its declared byte length." }
            $offset += $read
        }
    } finally {
        $stream.Dispose()
    }
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xef -and $bytes[1] -eq 0xbb -and $bytes[2] -eq 0xbf) {
        throw "$Label must be UTF-8 without a BOM."
    }
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $actualHash = 'sha256:' + ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
    if ($ExpectedSha256 -and $actualHash -cne $ExpectedSha256.ToLowerInvariant()) {
        throw "$Label full-file SHA-256 mismatch."
    }
    try {
        $text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
        $value = $text | ConvertFrom-Json
    } catch {
        throw "$Label is not strict UTF-8 JSON: $($_.Exception.Message)"
    }
    return [pscustomobject][ordered]@{ path = $full; sha256 = $actualHash; value = $value }
}

function Assert-ExactProperties {
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string[]]$Expected,
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($null -eq $Value -or $Value -is [string] -or $Value -is [Array]) { throw "$Label must be an object." }
    $actual = @($Value.PSObject.Properties.Name)
    foreach ($name in $actual) {
        if (-not ($Expected -ccontains $name)) { throw "$Label contains unsupported field '$name'." }
    }
    foreach ($name in $Expected) {
        if (-not ($actual -ccontains $name)) { throw "$Label is missing required field '$name'." }
    }
}

function Assert-IntegerValue {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory = $true)][uint64]$Minimum,
        [Parameter(Mandatory = $true)][uint64]$Maximum,
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($Value -isnot [sbyte] -and $Value -isnot [byte] -and
        $Value -isnot [int16] -and $Value -isnot [uint16] -and
        $Value -isnot [int32] -and $Value -isnot [uint32] -and
        $Value -isnot [int64] -and $Value -isnot [uint64]) {
        throw "$Label must be an integer."
    }
    if ([int64]$Value -lt [int64]$Minimum -or [uint64]$Value -gt $Maximum) {
        throw "$Label is outside the admitted bound."
    }
}

function Get-CanonicalUtcTimestamp {
    param([Parameter(Mandatory = $true)][string]$Value, [string]$Label = 'UTC timestamp')
    try {
        $parsed = [DateTimeOffset]::ParseExact(
            $Value, 'o', [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    } catch {
        throw "$Label is not a round-trip ISO-8601 timestamp."
    }
    $canonical = $parsed.UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    if ($Value -cne $canonical) { throw "$Label must be canonical UTC with the Z suffix." }
    return $canonical
}

function Initialize-WindowNativeMethods {
    if ('DmGenericSceneWindowNative' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class DmGenericSceneWindowNative {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hWnd, StringBuilder value, int capacity);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
}
'@
}

function Get-LiveMainWindowIdentity {
    param([Parameter(Mandatory = $true)][uint32]$Id)
    Initialize-WindowNativeMethods
    $before = Get-Process -Id $Id -ErrorAction Stop
    $startBefore = $before.StartTime.ToUniversalTime()
    $cim = Get-CimInstance Win32_Process -Filter "ProcessId=$Id" -ErrorAction Stop
    $executablePath = [string]$cim.ExecutablePath
    if (-not $executablePath -or -not (Test-Path -LiteralPath $executablePath -PathType Leaf)) {
        throw "Process $Id has no readable executable path."
    }
    $executablePath = [IO.Path]::GetFullPath($executablePath)
    Assert-NoReparseAncestor -Path $executablePath -Label 'live process executable'
    $executableHash = 'sha256:' + (Get-FileHash -LiteralPath $executablePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $process = Get-Process -Id $Id -ErrorAction Stop
    $process.Refresh()
    $startAfter = $process.StartTime.ToUniversalTime()
    if ($startBefore.Ticks -ne $startAfter.Ticks) {
        throw "Process $Id changed instances while representative-scene identity was read."
    }
    $handle = $process.MainWindowHandle
    if ($handle -eq [IntPtr]::Zero -or
        -not [DmGenericSceneWindowNative]::IsWindow($handle) -or
        -not [DmGenericSceneWindowNative]::IsWindowVisible($handle)) {
        throw "Process $Id has no visible main window."
    }
    $classBuilder = [Text.StringBuilder]::new(257)
    if ([DmGenericSceneWindowNative]::GetClassName($handle, $classBuilder, $classBuilder.Capacity) -le 0) {
        throw "Process $Id main window class is unreadable."
    }
    $rect = New-Object DmGenericSceneWindowNative+RECT
    if (-not [DmGenericSceneWindowNative]::GetClientRect($handle, [ref]$rect)) {
        throw "Process $Id main window client rectangle is unreadable."
    }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0 -or $width -gt 16384 -or $height -gt 16384) {
        throw "Process $Id main window client size is outside the admitted bound."
    }
    return [pscustomobject][ordered]@{
        executable_path = $executablePath
        executable_sha256 = $executableHash
        process_start_time_utc = $startAfter.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        class_name = $classBuilder.ToString()
        client_width = [uint32]$width
        client_height = [uint32]$height
    }
}

function Assert-StringArray {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [Array]) { throw "$Label must be an array." }
    foreach ($item in $Value) {
        if ($item -isnot [string]) { throw "$Label entries must be strings." }
    }
}

function Assert-ProtectionHitArray {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [Array]) { throw "$Label must be an array." }
    $running = 0
    foreach ($entry in $Value) {
        Assert-ExactProperties $entry @('name', 'display_name', 'state', 'start_mode') "$Label entry"
        foreach ($field in @('name', 'display_name', 'state', 'start_mode')) {
            if ($entry.PSObject.Properties[$field].Value -isnot [string]) {
                throw "$Label entry.$field must be a string."
            }
        }
        if ([string]::IsNullOrWhiteSpace([string]$entry.name)) {
            throw "$Label entry.name must not be blank."
        }
        if ([string]$entry.state -ceq 'Running') { $running++ }
    }
    return [uint32]$running
}

function Assert-BoundRuntimeEvidence {
    param(
        [Parameter(Mandatory = $true)]$Runtime,
        [Parameter(Mandatory = $true)][uint32]$ExpectedPid,
        [Parameter(Mandatory = $true)][string]$ExpectedStartTimeUtc,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableBasename,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableSha256,
        [Parameter(Mandatory = $true)]$LiveIdentity,
        [Parameter(Mandatory = $true)][uint32]$MaxAgeSeconds,
        [Parameter(Mandatory = $true)][DateTime]$NowUtc
    )
    Assert-ExactProperties $Runtime @(
        'schema', 'captured_utc', 'host', 'collector_sha256', 'process',
        'module_enumeration', 'graphics', 'anti_cheat', 'status', 'blocking', 'limitations'
    ) 'bound runtime evidence'
    Assert-ExactProperties $Runtime.process @(
        'process_id', 'process_name', 'executable_path', 'executable_sha256', 'start_time_utc'
    ) 'runtime.process'
    Assert-ExactProperties $Runtime.module_enumeration @(
        'complete', 'error', 'collector_process_architecture', 'target_pe_machine', 'count', 'names'
    ) 'runtime.module_enumeration'
    Assert-ExactProperties $Runtime.graphics @('expected_api', 'hits') 'runtime.graphics'
    Assert-ExactProperties $Runtime.anti_cheat @(
        'require_none_loaded', 'module_hits', 'service_enumeration_complete',
        'service_enumeration_error', 'service_hits', 'driver_enumeration_complete',
        'driver_enumeration_error', 'driver_hits', 'running_service_hits', 'running_driver_hits'
    ) 'runtime.anti_cheat'

    foreach ($field in @('schema', 'captured_utc', 'host', 'collector_sha256', 'status')) {
        if ($Runtime.PSObject.Properties[$field].Value -isnot [string]) {
            throw "runtime.$field must be a string."
        }
    }
    if ($Runtime.schema -cne 'game-runtime-gate-evidence/v1' -or $Runtime.status -cne 'pass' -or
        [string]$Runtime.collector_sha256 -cnotmatch '^sha256:[0-9a-f]{64}$') {
        throw 'Bound runtime evidence is not a canonical passing game-runtime-gate-evidence/v1 report.'
    }
    if ([string]::IsNullOrWhiteSpace([string]$Runtime.host)) {
        throw 'runtime.host must not be blank.'
    }
    Assert-IntegerValue $Runtime.process.process_id 1 ([uint32]::MaxValue) 'runtime.process.process_id'
    if ([uint32]$Runtime.process.process_id -ne $ExpectedPid) { throw 'Bound runtime evidence PID drifted.' }
    foreach ($field in @('process_name', 'executable_path', 'executable_sha256', 'start_time_utc')) {
        if ($Runtime.process.PSObject.Properties[$field].Value -isnot [string]) {
            throw "runtime.process.$field must be a string."
        }
    }
    if ([string]::IsNullOrWhiteSpace([string]$Runtime.process.process_name) -or
        [string]::IsNullOrWhiteSpace([string]$Runtime.process.executable_path) -or
        [string]$Runtime.process.executable_sha256 -cnotmatch '^sha256:[0-9a-f]{64}$') {
        throw 'Bound runtime process identity is not canonical.'
    }
    $runtimePath = [IO.Path]::GetFullPath([string]$Runtime.process.executable_path)
    if (-not ([IO.Path]::GetFileName($runtimePath)).Equals(
        $ExpectedExecutableBasename, [StringComparison]::OrdinalIgnoreCase) -or
        -not $runtimePath.Equals([string]$LiveIdentity.executable_path, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Bound runtime evidence executable path drifted.'
    }
    if ([string]$Runtime.process.executable_sha256 -cne $ExpectedExecutableSha256 -or
        [string]$LiveIdentity.executable_sha256 -cne $ExpectedExecutableSha256) {
        throw 'Bound runtime evidence or live executable hash drifted.'
    }
    $runtimeStart = Get-CanonicalUtcTimestamp ([string]$Runtime.process.start_time_utc) 'runtime.process.start_time_utc'
    $expectedStart = Get-CanonicalUtcTimestamp $ExpectedStartTimeUtc 'expected process start time'
    if ($runtimeStart -cne $expectedStart -or
        [string]$LiveIdentity.process_start_time_utc -cne $runtimeStart) {
        throw 'Bound runtime evidence or live process instance drifted.'
    }

    if ($Runtime.module_enumeration.complete -isnot [bool] -or
        $Runtime.anti_cheat.require_none_loaded -isnot [bool] -or
        $Runtime.anti_cheat.service_enumeration_complete -isnot [bool] -or
        $Runtime.anti_cheat.driver_enumeration_complete -isnot [bool]) {
        throw 'Bound runtime protection completeness flags must be booleans.'
    }
    Assert-IntegerValue $Runtime.module_enumeration.count 0 ([uint32]::MaxValue) 'runtime.module_enumeration.count'
    Assert-IntegerValue $Runtime.anti_cheat.running_service_hits 0 ([uint32]::MaxValue) 'runtime.anti_cheat.running_service_hits'
    Assert-IntegerValue $Runtime.anti_cheat.running_driver_hits 0 ([uint32]::MaxValue) 'runtime.anti_cheat.running_driver_hits'
    Assert-StringArray $Runtime.module_enumeration.names 'runtime.module_enumeration.names'
    Assert-StringArray $Runtime.graphics.hits 'runtime.graphics.hits'
    Assert-StringArray $Runtime.anti_cheat.module_hits 'runtime.anti_cheat.module_hits'
    Assert-StringArray $Runtime.blocking 'runtime.blocking'
    Assert-StringArray $Runtime.limitations 'runtime.limitations'
    $runningServices = Assert-ProtectionHitArray `
        $Runtime.anti_cheat.service_hits 'runtime.anti_cheat.service_hits'
    $runningDrivers = Assert-ProtectionHitArray `
        $Runtime.anti_cheat.driver_hits 'runtime.anti_cheat.driver_hits'
    foreach ($nullableError in @(
        $Runtime.module_enumeration.error,
        $Runtime.anti_cheat.service_enumeration_error,
        $Runtime.anti_cheat.driver_enumeration_error
    )) {
        if ($null -ne $nullableError -and $nullableError -isnot [string]) {
            throw 'Bound runtime enumeration errors must be null or strings.'
        }
    }
    foreach ($field in @('collector_process_architecture', 'target_pe_machine')) {
        if ($Runtime.module_enumeration.PSObject.Properties[$field].Value -isnot [string]) {
            throw "runtime.module_enumeration.$field must be a string."
        }
    }
    if ([uint32]$Runtime.module_enumeration.count -ne @($Runtime.module_enumeration.names).Count) {
        throw 'runtime.module_enumeration.count does not match names.'
    }
    if ($Runtime.graphics.expected_api -isnot [string] -or
        $Runtime.graphics.expected_api -cne 'd3d11' -or
        -not (@($Runtime.graphics.hits) -ccontains 'd3d11.dll')) {
        throw 'Bound runtime evidence does not prove D3D11.'
    }
    if (-not $Runtime.module_enumeration.complete -or
        -not $Runtime.anti_cheat.require_none_loaded -or
        -not $Runtime.anti_cheat.service_enumeration_complete -or
        -not $Runtime.anti_cheat.driver_enumeration_complete -or
        @($Runtime.anti_cheat.module_hits).Count -ne 0 -or
        [uint32]$Runtime.anti_cheat.running_service_hits -ne 0 -or
        [uint32]$Runtime.anti_cheat.running_driver_hits -ne 0 -or
        $runningServices -ne [uint32]$Runtime.anti_cheat.running_service_hits -or
        $runningDrivers -ne [uint32]$Runtime.anti_cheat.running_driver_hits -or
        $null -ne $Runtime.module_enumeration.error -or
        $null -ne $Runtime.anti_cheat.service_enumeration_error -or
        $null -ne $Runtime.anti_cheat.driver_enumeration_error -or
        @($Runtime.blocking).Count -ne 0) {
        throw 'Bound runtime anti-cheat/module/service/driver gate is incomplete or blocked.'
    }
    $captured = Get-CanonicalUtcTimestamp ([string]$Runtime.captured_utc) 'runtime.captured_utc'
    $capturedUtc = [DateTimeOffset]::ParseExact(
        $captured, 'o', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind).UtcDateTime
    $age = ($NowUtc.ToUniversalTime() - $capturedUtc).TotalSeconds
    if ($age -lt -5 -or $age -gt $MaxAgeSeconds) {
        throw "Bound runtime evidence is not fresh enough (age_seconds=$([Math]::Round($age, 1)))."
    }
    return $captured
}

function Assert-AttestationObject {
    param(
        [Parameter(Mandatory = $true)]$Attestation,
        [Parameter(Mandatory = $true)][string]$ExpectedGameId,
        [Parameter(Mandatory = $true)][string]$ExpectedAppId,
        [Parameter(Mandatory = $true)][string]$ExpectedBuildId,
        [Parameter(Mandatory = $true)][string]$ExpectedDiscoveryId,
        [Parameter(Mandatory = $true)][string]$ExpectedOutputLeaf,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableBasename,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedRuntimeEvidenceSha256,
        [Parameter(Mandatory = $true)]$BoundRuntime,
        [Parameter(Mandatory = $true)][uint32]$ExpectedPid,
        [Parameter(Mandatory = $true)][string]$ExpectedStartTimeUtc,
        [Parameter(Mandatory = $true)]$CurrentWindow,
        [Parameter(Mandatory = $true)][string]$RequiredWindowClass,
        [Parameter(Mandatory = $true)][uint32]$RequiredClientWidth,
        [Parameter(Mandatory = $true)][uint32]$RequiredClientHeight,
        [Parameter(Mandatory = $true)][uint32]$MaxAgeSeconds,
        [DateTime]$NowUtc = [DateTime]::UtcNow
    )
    Assert-ExactProperties $Attestation @(
        'schema', 'attestationKind', 'gameId', 'steamAppId', 'steamBuildId', 'discoveryId', 'outputLeaf',
        'executable', 'runtimeEvidence', 'window', 'sceneKind', 'observedAtUtc', 'nonce'
    ) 'representative-scene attestation'
    Assert-ExactProperties $Attestation.executable @('basename', 'sha256') 'attestation.executable'
    Assert-ExactProperties $Attestation.runtimeEvidence @(
        'sha256', 'processId', 'processStartTimeUtc'
    ) 'attestation.runtimeEvidence'
    Assert-ExactProperties $Attestation.window @(
        'className', 'clientWidth', 'clientHeight'
    ) 'attestation.window'

    foreach ($field in @('schema', 'attestationKind', 'gameId', 'steamAppId', 'steamBuildId', 'discoveryId', 'outputLeaf', 'sceneKind', 'observedAtUtc', 'nonce')) {
        if ($Attestation.PSObject.Properties[$field].Value -isnot [string]) {
            throw "attestation.$field must be a string."
        }
    }
    foreach ($field in @('basename', 'sha256')) {
        if ($Attestation.executable.PSObject.Properties[$field].Value -isnot [string]) {
            throw "attestation.executable.$field must be a string."
        }
    }
    foreach ($field in @('sha256', 'processStartTimeUtc')) {
        if ($Attestation.runtimeEvidence.PSObject.Properties[$field].Value -isnot [string]) {
            throw "attestation.runtimeEvidence.$field must be a string."
        }
    }
    if ($Attestation.window.className -isnot [string]) { throw 'attestation.window.className must be a string.' }
    Assert-IntegerValue $Attestation.runtimeEvidence.processId 1 ([uint32]::MaxValue) 'attestation.runtimeEvidence.processId'
    Assert-IntegerValue $Attestation.window.clientWidth 1 16384 'attestation.window.clientWidth'
    Assert-IntegerValue $Attestation.window.clientHeight 1 16384 'attestation.window.clientHeight'

    if ([string]$Attestation.schema -cne $script:AttestationSchema -or
        [string]$Attestation.attestationKind -cne $script:AttestationKind -or
        [string]$Attestation.sceneKind -cne $script:SceneKind) {
        throw 'Representative-scene attestation contract is unsupported.'
    }
    if ([string]$Attestation.gameId -cne $ExpectedGameId -or
        [string]$Attestation.steamAppId -cne $ExpectedAppId -or
        [string]$Attestation.steamBuildId -cne $ExpectedBuildId -or
        [string]$Attestation.discoveryId -cne $ExpectedDiscoveryId -or
        [string]$Attestation.outputLeaf -cne $ExpectedOutputLeaf) {
        throw 'Representative-scene attestation game/AppID/build/discovery/output identity drifted.'
    }
    if (-not ([string]$Attestation.executable.basename).Equals(
        $ExpectedExecutableBasename, [StringComparison]::OrdinalIgnoreCase) -or
        [string]$Attestation.executable.sha256 -cne $ExpectedExecutableSha256.ToLowerInvariant()) {
        throw 'Representative-scene attestation executable identity drifted.'
    }
    if ([string]$Attestation.runtimeEvidence.sha256 -cne $ExpectedRuntimeEvidenceSha256 -or
        [uint32]$Attestation.runtimeEvidence.processId -ne $ExpectedPid) {
        throw 'Representative-scene attestation runtime evidence identity drifted.'
    }
    $attestedStart = Get-CanonicalUtcTimestamp `
        -Value ([string]$Attestation.runtimeEvidence.processStartTimeUtc) `
        -Label 'attestation.runtimeEvidence.processStartTimeUtc'
    $expectedStart = Get-CanonicalUtcTimestamp $ExpectedStartTimeUtc 'expected process start time'
    if ($attestedStart -cne $expectedStart) { throw 'Representative-scene attestation process instance drifted.' }

    $runtimeCaptured = Assert-BoundRuntimeEvidence `
        -Runtime $BoundRuntime -ExpectedPid $ExpectedPid -ExpectedStartTimeUtc $ExpectedStartTimeUtc `
        -ExpectedExecutableBasename $ExpectedExecutableBasename `
        -ExpectedExecutableSha256 $ExpectedExecutableSha256 `
        -LiveIdentity $CurrentWindow -MaxAgeSeconds $MaxAgeSeconds -NowUtc $NowUtc

    if ([string]$Attestation.window.className -cne $RequiredWindowClass -or
        [uint32]$Attestation.window.clientWidth -ne $RequiredClientWidth -or
        [uint32]$Attestation.window.clientHeight -ne $RequiredClientHeight) {
        throw 'Representative-scene attestation window does not match the target registry.'
    }
    if ([string]$CurrentWindow.class_name -cne [string]$Attestation.window.className -or
        [uint32]$CurrentWindow.client_width -ne [uint32]$Attestation.window.clientWidth -or
        [uint32]$CurrentWindow.client_height -ne [uint32]$Attestation.window.clientHeight) {
        throw 'Live game window class or client size drifted after operator attestation.'
    }
    if ([string]$Attestation.nonce -cnotmatch '^[0-9a-f]{64}$') {
        throw 'Representative-scene attestation nonce must be 32 random bytes encoded as lowercase hex.'
    }

    $observed = Get-CanonicalUtcTimestamp ([string]$Attestation.observedAtUtc) 'attestation.observedAtUtc'
    $observedUtc = [DateTimeOffset]::ParseExact(
        $observed, 'o', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind).UtcDateTime
    $runtimeCapturedDate = [DateTimeOffset]::ParseExact(
        $runtimeCaptured, 'o', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind).UtcDateTime
    if ($observedUtc -lt $runtimeCapturedDate) {
        throw 'Representative-scene attestation predates the bound runtime evidence.'
    }
    $age = ($NowUtc.ToUniversalTime() - $observedUtc).TotalSeconds
    if ($age -lt -5 -or $age -gt $MaxAgeSeconds) {
        throw "Representative-scene attestation is not fresh enough (age_seconds=$([Math]::Round($age, 1)))."
    }
}

function Copy-JsonObject {
    param([Parameter(Mandatory = $true)]$Value)
    return ($Value | ConvertTo-Json -Depth 12 | ConvertFrom-Json)
}

function Invoke-SelfTest {
    $now = [DateTime]::UtcNow
    $start = $now.AddMinutes(-2).ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    $runtimeCaptured = $now.AddSeconds(-10).ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    $window = [pscustomobject]@{
        executable_path = 'C:\fixture\Game.exe'; executable_sha256 = 'sha256:' + ('a' * 64)
        process_start_time_utc = $start
        class_name = 'FixtureWindow'; client_width = 1920; client_height = 1080
    }
    $runtime = [pscustomobject]@{
        schema = 'game-runtime-gate-evidence/v1'; captured_utc = $runtimeCaptured
        host = 'fixture-host'; collector_sha256 = 'sha256:' + ('d' * 64)
        process = [pscustomobject]@{
            process_id = 1234; process_name = 'Game'; executable_path = 'C:\fixture\Game.exe'
            executable_sha256 = 'sha256:' + ('a' * 64); start_time_utc = $start
        }
        module_enumeration = [pscustomobject]@{
            complete = $true; error = $null; collector_process_architecture = 'x64'
            target_pe_machine = 'AMD64'; count = 2; names = @('d3d11.dll', 'dxgi.dll')
        }
        graphics = [pscustomobject]@{ expected_api = 'd3d11'; hits = @('d3d11.dll', 'dxgi.dll') }
        anti_cheat = [pscustomobject]@{
            require_none_loaded = $true; module_hits = @()
            service_enumeration_complete = $true; service_enumeration_error = $null; service_hits = @()
            driver_enumeration_complete = $true; driver_enumeration_error = $null; driver_hits = @()
            running_service_hits = 0; running_driver_hits = 0
        }
        status = 'pass'; blocking = @(); limitations = @('fixture limitation')
    }
    $attestation = [pscustomobject][ordered]@{
        schema = $script:AttestationSchema
        attestationKind = $script:AttestationKind
        gameId = 'fixture-game-steam'
        steamAppId = '100'
        steamBuildId = '200'
        discoveryId = 'fixture-discovery'
        outputLeaf = 'fixture-output'
        executable = [pscustomobject][ordered]@{
            basename = 'Game.exe'; sha256 = 'sha256:' + ('a' * 64)
        }
        runtimeEvidence = [pscustomobject][ordered]@{
            sha256 = 'sha256:' + ('b' * 64); processId = 1234; processStartTimeUtc = $start
        }
        window = [pscustomobject][ordered]@{
            className = 'FixtureWindow'; clientWidth = 1920; clientHeight = 1080
        }
        sceneKind = $script:SceneKind
        observedAtUtc = $now.AddSeconds(-5).ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        nonce = 'c' * 64
    }
    $common = @{
        ExpectedGameId = 'fixture-game-steam'; ExpectedAppId = '100'; ExpectedBuildId = '200'
        ExpectedDiscoveryId = 'fixture-discovery'; ExpectedOutputLeaf = 'fixture-output'
        ExpectedExecutableBasename = 'Game.exe'; ExpectedExecutableSha256 = 'sha256:' + ('a' * 64)
        ExpectedRuntimeEvidenceSha256 = 'sha256:' + ('b' * 64); BoundRuntime = $runtime
        ExpectedPid = [uint32]1234; ExpectedStartTimeUtc = $start; CurrentWindow = $window
        RequiredWindowClass = 'FixtureWindow'; RequiredClientWidth = [uint32]1920
        RequiredClientHeight = [uint32]1080; MaxAgeSeconds = [uint32]120; NowUtc = $now
    }
    Assert-AttestationObject -Attestation $attestation @common
    $tests = 1

    $negativeCases = @(
        @{ Name = 'unknown field'; Mutate = { param($v) $v | Add-Member unexpected $true } },
        @{ Name = 'wrong attestation kind'; Mutate = { param($v) $v.attestationKind = 'machine_vision' } },
        @{ Name = 'wrong game'; Mutate = { param($v) $v.gameId = 'other-game' } },
        @{ Name = 'wrong build'; Mutate = { param($v) $v.steamBuildId = '201' } },
        @{ Name = 'wrong discovery id'; Mutate = { param($v) $v.discoveryId = 'other-discovery' } },
        @{ Name = 'wrong output leaf'; Mutate = { param($v) $v.outputLeaf = 'other-output' } },
        @{ Name = 'wrong executable hash'; Mutate = { param($v) $v.executable.sha256 = 'sha256:' + ('d' * 64) } },
        @{ Name = 'wrong runtime evidence hash'; Mutate = { param($v) $v.runtimeEvidence.sha256 = 'sha256:' + ('d' * 64) } },
        @{ Name = 'PID encoded as a string'; Mutate = { param($v) $v.runtimeEvidence.processId = '1234' } },
        @{ Name = 'process instance drift'; Mutate = { param($v) $v.runtimeEvidence.processStartTimeUtc = [DateTime]::UtcNow.AddMinutes(-3).ToString('o') } },
        @{ Name = 'wrong scene kind'; Mutate = { param($v) $v.sceneKind = 'main_menu' } },
        @{ Name = 'wrong window class'; Mutate = { param($v) $v.window.className = 'MenuWindow' } },
        @{ Name = 'wrong window size'; Mutate = { param($v) $v.window.clientWidth = 1280 } },
        @{ Name = 'window size encoded as a string'; Mutate = { param($v) $v.window.clientWidth = '1920' } },
        @{ Name = 'invalid nonce'; Mutate = { param($v) $v.nonce = 'not-random' } },
        @{ Name = 'predates runtime evidence'; Mutate = { param($v) $v.observedAtUtc = [DateTime]::UtcNow.AddMinutes(-1).ToString('o') } },
        @{ Name = 'stale attestation'; Mutate = { param($v) $v.observedAtUtc = [DateTime]::UtcNow.AddMinutes(-5).ToString('o') } },
        @{ Name = 'future attestation'; Mutate = { param($v) $v.observedAtUtc = [DateTime]::UtcNow.AddMinutes(1).ToString('o') } }
    )
    foreach ($case in $negativeCases) {
        $candidate = Copy-JsonObject $attestation
        & $case.Mutate $candidate
        $blocked = $false
        try { Assert-AttestationObject -Attestation $candidate @common } catch { $blocked = $true }
        if (-not $blocked) { throw "self-test negative case passed unexpectedly: $($case.Name)" }
        $tests++
    }

    $runtimeNegativeCases = @(
        @{ Name = 'runtime unknown field'; Mutate = { param($v) $v | Add-Member unexpected $true } },
        @{ Name = 'runtime status blocked'; Mutate = { param($v) $v.status = 'blocked' } },
        @{ Name = 'runtime PID encoded as a string'; Mutate = { param($v) $v.process.process_id = '1234' } },
        @{ Name = 'runtime path drift'; Mutate = { param($v) $v.process.executable_path = 'C:\fixture\Other.exe' } },
        @{ Name = 'runtime hash drift'; Mutate = { param($v) $v.process.executable_sha256 = 'sha256:' + ('e' * 64) } },
        @{ Name = 'runtime start drift'; Mutate = { param($v) $v.process.start_time_utc = [DateTime]::UtcNow.AddMinutes(-3).ToString('o') } },
        @{ Name = 'runtime graphics hits scalar'; Mutate = { param($v) $v.graphics.hits = 'd3d11.dll' } },
        @{ Name = 'runtime complete string'; Mutate = { param($v) $v.module_enumeration.complete = 'false' } },
        @{ Name = 'runtime protection enumeration incomplete'; Mutate = { param($v) $v.anti_cheat.driver_enumeration_complete = $false } },
        @{ Name = 'runtime protection hit'; Mutate = { param($v) $v.anti_cheat.module_hits = @('bedaisy.sys') } },
        @{ Name = 'runtime module count drift'; Mutate = { param($v) $v.module_enumeration.count = 1 } },
        @{ Name = 'runtime completed enumeration error'; Mutate = { param($v) $v.module_enumeration.error = 'partial read' } },
        @{ Name = 'runtime service hits scalar'; Mutate = { param($v) $v.anti_cheat.service_hits = 'service' } },
        @{ Name = 'runtime running service count drift'; Mutate = {
            param($v)
            $v.anti_cheat.service_hits = @([pscustomobject]@{
                name = 'fixture'; display_name = 'Fixture'; state = 'Running'; start_mode = 'Manual'
            })
        } },
        @{ Name = 'runtime protection hit unknown field'; Mutate = {
            param($v)
            $v.anti_cheat.driver_hits = @([pscustomobject]@{
                name = 'fixture'; display_name = 'Fixture'; state = 'Stopped'; start_mode = 'Manual'; extra = $true
            })
        } },
        @{ Name = 'runtime blockers null'; Mutate = { param($v) $v.blocking = $null } },
        @{ Name = 'runtime captured after attestation'; Mutate = {
            param($v) $v.captured_utc = $now.AddSeconds(-1).ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        } },
        @{ Name = 'runtime captured time stale'; Mutate = { param($v) $v.captured_utc = [DateTime]::UtcNow.AddMinutes(-5).ToString('o') } }
    )
    foreach ($case in $runtimeNegativeCases) {
        $candidateRuntime = Copy-JsonObject $runtime
        & $case.Mutate $candidateRuntime
        $candidateCommon = $common.Clone()
        $candidateCommon.BoundRuntime = $candidateRuntime
        $blocked = $false
        try { Assert-AttestationObject -Attestation $attestation @candidateCommon } catch { $blocked = $true }
        if (-not $blocked) { throw "self-test runtime negative case passed unexpectedly: $($case.Name)" }
        $tests++
    }

    $temp = Join-Path ([IO.Path]::GetTempPath()) ('dm-scene-attestation-' + [Guid]::NewGuid().ToString('N') + '.json')
    try {
        [IO.File]::WriteAllText($temp, ($attestation | ConvertTo-Json -Depth 8) + "`n", [Text.UTF8Encoding]::new($false))
        $document = Read-VerifiedUtf8Json -Path $temp
        $wrongHashBlocked = $false
        try { Read-VerifiedUtf8Json -Path $temp -ExpectedSha256 ('sha256:' + ('f' * 64)) | Out-Null }
        catch { $wrongHashBlocked = $true }
        if (-not $wrongHashBlocked) { throw 'self-test accepted the wrong full-file SHA-256.' }
        $tests++
        if ($document.sha256 -cnotmatch '^sha256:[0-9a-f]{64}$') { throw 'self-test did not hash the complete file.' }
    } finally {
        if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Force }
    }
    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

try {
    if ($SelfTest) { Invoke-SelfTest; exit 0 }

    $document = Read-VerifiedUtf8Json `
        -Path $AttestationPath -ExpectedSha256 $ExpectedAttestationSha256 `
        -Label 'representative-scene attestation'
    $runtimeDocument = Read-VerifiedUtf8Json -Path $RuntimeEvidencePath -Label 'arm runtime gate evidence'
    $currentWindow = Get-LiveMainWindowIdentity -Id $ExpectedProcessId
    Assert-AttestationObject `
        -Attestation $document.value `
        -ExpectedGameId $GameId -ExpectedAppId $AppId -ExpectedBuildId $BuildId `
        -ExpectedDiscoveryId $ExpectedDiscoveryId -ExpectedOutputLeaf $ExpectedOutputLeaf `
        -ExpectedExecutableBasename $ExecutableBasename `
        -ExpectedExecutableSha256 $ExecutableSha256.ToLowerInvariant() `
        -ExpectedRuntimeEvidenceSha256 $runtimeDocument.sha256 `
        -BoundRuntime $runtimeDocument.value `
        -ExpectedPid $ExpectedProcessId -ExpectedStartTimeUtc $ExpectedProcessStartTimeUtc `
        -CurrentWindow $currentWindow `
        -RequiredWindowClass $ExpectedWindowClass `
        -RequiredClientWidth $ExpectedClientWidth -RequiredClientHeight $ExpectedClientHeight `
        -MaxAgeSeconds $MaximumAgeSeconds

    [pscustomobject][ordered]@{
        status = 'pass'
        schema = $script:AttestationSchema
        attestation_sha256 = $document.sha256
        attestation_kind = $script:AttestationKind
        scene_kind = $script:SceneKind
        game_id = $GameId
        steam_app_id = $AppId
        steam_build_id = $BuildId
        discovery_id = $ExpectedDiscoveryId
        output_leaf = $ExpectedOutputLeaf
        process_id = $ExpectedProcessId
        window_class = $currentWindow.class_name
        client_width = $currentWindow.client_width
        client_height = $currentWindow.client_height
    } | ConvertTo-Json -Depth 3
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}

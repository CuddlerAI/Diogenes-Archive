<#
.SYNOPSIS
  Creates one local operator attestation for a representative offline single-player scene.

.DESCRIPTION
  The operator must visually determine that the live game is in the required scene and pass the
  explicit confirmation switch. This tool cannot recognize gameplay. It binds that assertion to
  the exact game/build/executable, runtime evidence bytes, process instance, and live window, then
  writes a new UTF-8 JSON file without overwriting any prior evidence. It does not inject or arm.
#>
[CmdletBinding(DefaultParameterSetName = 'Create')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [switch]$ConfirmRepresentativeOfflineSinglePlayer,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$AppId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidatePattern('^\d+$')]
    [string]$ExpectedBuildId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$DiscoveryId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$OutputLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ProcessId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [string]$ExpectedExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedExecutableSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [string]$RuntimeEvidencePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Create')]
    [string]$OutputPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

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

function Write-CreateNewUtf8Json {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)]$Value)
    $full = [IO.Path]::GetFullPath($Path)
    if (-not ([IO.Path]::GetExtension($full)).Equals('.json', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Attestation output must be a .json file.'
    }
    if (Test-Path -LiteralPath $full) { throw "Refusing to overwrite an existing attestation: $full" }
    $parent = [IO.Path]::GetDirectoryName($full)
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
        throw "Attestation parent directory does not exist: $parent"
    }
    Assert-NoReparseAncestor -Path $parent -Label 'attestation directory'
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Depth 8) + "`n")
    $stream = [IO.File]::Open($full, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    return $full
}

function Read-StableUtf8Json {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$Label = 'JSON')
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    $full = (Resolve-Path -LiteralPath $Path).Path
    Assert-NoReparseAncestor -Path $full -Label $Label
    $beforeHash = 'sha256:' + (Get-FileHash -LiteralPath $full -Algorithm SHA256).Hash.ToLowerInvariant()
    $value = Get-Content -LiteralPath $full -Raw -Encoding UTF8 | ConvertFrom-Json
    $afterHash = 'sha256:' + (Get-FileHash -LiteralPath $full -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($beforeHash -cne $afterHash) { throw "$Label changed while it was being read." }
    return [pscustomobject][ordered]@{ path = $full; sha256 = $afterHash; value = $value }
}

function Get-CanonicalUtcTimestamp {
    param([Parameter(Mandatory = $true)][string]$Value, [string]$Label = 'UTC timestamp')
    try {
        $parsed = [DateTimeOffset]::ParseExact(
            $Value, 'o', [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    } catch { throw "$Label is not a round-trip ISO-8601 timestamp." }
    $canonical = $parsed.UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    if ($Value -cne $canonical) { throw "$Label must be canonical UTC with the Z suffix." }
    return $canonical
}

function New-RandomNonce {
    $bytes = New-Object byte[] 32
    $generator = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $generator.GetBytes($bytes) } finally { $generator.Dispose() }
    return ([BitConverter]::ToString($bytes)).Replace('-', '').ToLowerInvariant()
}

function Initialize-WindowNativeMethods {
    if ('DmGenericSceneAttestationNative' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class DmGenericSceneAttestationNative {
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

function Get-LiveIdentityAndWindow {
    param([Parameter(Mandatory = $true)][uint32]$Id)
    Initialize-WindowNativeMethods
    $before = Get-Process -Id $Id -ErrorAction Stop
    $startBefore = $before.StartTime.ToUniversalTime()
    $cim = Get-CimInstance Win32_Process -Filter "ProcessId=$Id" -ErrorAction Stop
    $path = [string]$cim.ExecutablePath
    if (-not $path -or -not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Process $Id has no readable executable path."
    }
    $path = [IO.Path]::GetFullPath($path)
    Assert-NoReparseAncestor -Path $path -Label 'live process executable'
    $hash = 'sha256:' + (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    $after = Get-Process -Id $Id -ErrorAction Stop
    $after.Refresh()
    $startAfter = $after.StartTime.ToUniversalTime()
    if ($startBefore.Ticks -ne $startAfter.Ticks) { throw "Process $Id changed instances during attestation." }
    $handle = $after.MainWindowHandle
    if ($handle -eq [IntPtr]::Zero -or
        -not [DmGenericSceneAttestationNative]::IsWindow($handle) -or
        -not [DmGenericSceneAttestationNative]::IsWindowVisible($handle)) {
        throw "Process $Id has no visible main window."
    }
    $classBuilder = [Text.StringBuilder]::new(257)
    if ([DmGenericSceneAttestationNative]::GetClassName($handle, $classBuilder, $classBuilder.Capacity) -le 0) {
        throw "Process $Id main window class is unreadable."
    }
    $rect = New-Object DmGenericSceneAttestationNative+RECT
    if (-not [DmGenericSceneAttestationNative]::GetClientRect($handle, [ref]$rect)) {
        throw "Process $Id main window client rectangle is unreadable."
    }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0 -or $width -gt 16384 -or $height -gt 16384) {
        throw "Process $Id main window client size is outside the admitted bound."
    }
    return [pscustomobject][ordered]@{
        process_id = $Id; executable_path = $path; executable_sha256 = $hash
        start_time_utc = $startAfter.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        window_class = $classBuilder.ToString(); client_width = [uint32]$width; client_height = [uint32]$height
    }
}

function Invoke-SelfTest {
    $root = Join-Path ([IO.Path]::GetTempPath()) ('dm-scene-attestation-create-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($root) | Out-Null
    try {
        $path = Join-Path $root 'attestation.json'
        Write-CreateNewUtf8Json -Path $path -Value ([ordered]@{ nonce = New-RandomNonce }) | Out-Null
        $blocked = $false
        try { Write-CreateNewUtf8Json -Path $path -Value ([ordered]@{ nonce = New-RandomNonce }) | Out-Null }
        catch { $blocked = $true }
        if (-not $blocked) { throw 'self-test overwrote an existing attestation.' }
        $nonceA = New-RandomNonce
        $nonceB = New-RandomNonce
        if ($nonceA -cnotmatch '^[0-9a-f]{64}$' -or $nonceB -cnotmatch '^[0-9a-f]{64}$' -or $nonceA -ceq $nonceB) {
            throw 'self-test random nonce generation failed.'
        }
        [pscustomobject][ordered]@{ status = 'pass'; tests = 2 } | ConvertTo-Json
    } finally {
        if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    }
}

try {
    if ($SelfTest) { Invoke-SelfTest; exit 0 }
    if (-not $ConfirmRepresentativeOfflineSinglePlayer) {
        throw 'The local operator must explicitly attest a representative offline single-player scene.'
    }
    $expectedExeHash = $ExpectedExecutableSha256.ToLowerInvariant()
    $exe = (Resolve-Path -LiteralPath $ExpectedExecutablePath).Path
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'Expected executable is not a file.' }
    Assert-NoReparseAncestor -Path $exe -Label 'expected executable'
    if (('sha256:' + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedExeHash) {
        throw 'Expected executable hash mismatch.'
    }

    $targetScript = Join-Path $PSScriptRoot 'Test-GenericD3D11DsvDiscoveryTargets.ps1'
    $targetText = (& $targetScript `
        -GameId $GameId -AppId ([string]$AppId) -BuildId $ExpectedBuildId `
        -ExecutableBasename ([IO.Path]::GetFileName($exe)) -ExecutableSha256 $expectedExeHash | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Generic D3D11 target binding blocked $GameId.`n$targetText" }
    $targetReport = $targetText | ConvertFrom-Json
    $target = $targetReport.selected_target
    if ($targetReport.status -cne 'pass' -or $null -eq $target -or -not $target.representativeSceneGate.required) {
        throw 'Target registry does not require the supported representative-scene attestation.'
    }

    $runtimeDocument = Read-StableUtf8Json -Path $RuntimeEvidencePath -Label 'runtime gate evidence'
    $runtime = $runtimeDocument.value
    if ($runtime.schema -cne 'game-runtime-gate-evidence/v1' -or $runtime.status -cne 'pass' -or
        [uint32]$runtime.process.process_id -ne $ProcessId -or
        [string]$runtime.process.executable_sha256 -cne $expectedExeHash -or
        $runtime.graphics.expected_api -cne 'd3d11' -or -not (@($runtime.graphics.hits) -contains 'd3d11.dll') -or
        -not $runtime.module_enumeration.complete -or -not $runtime.anti_cheat.require_none_loaded -or
        -not $runtime.anti_cheat.service_enumeration_complete -or -not $runtime.anti_cheat.driver_enumeration_complete -or
        @($runtime.anti_cheat.module_hits).Count -ne 0 -or [int]$runtime.anti_cheat.running_service_hits -ne 0 -or
        [int]$runtime.anti_cheat.running_driver_hits -ne 0 -or @($runtime.blocking).Count -ne 0) {
        throw 'Runtime gate evidence is not a passing exact-instance D3D11/no-protection report.'
    }
    $runtimeStart = Get-CanonicalUtcTimestamp ([string]$runtime.process.start_time_utc) 'runtime process start time'
    Get-CanonicalUtcTimestamp ([string]$runtime.captured_utc) 'runtime captured time' | Out-Null
    if (-not ([IO.Path]::GetFullPath([string]$runtime.process.executable_path)).Equals(
        $exe, [StringComparison]::OrdinalIgnoreCase)) { throw 'Runtime executable path drifted.' }

    $live = Get-LiveIdentityAndWindow -Id $ProcessId
    if (-not $live.executable_path.Equals($exe, [StringComparison]::OrdinalIgnoreCase) -or
        $live.executable_sha256 -cne $expectedExeHash -or $live.start_time_utc -cne $runtimeStart) {
        throw 'Live process instance does not match the runtime evidence.'
    }
    $requiredWindow = $target.representativeSceneGate.expectedWindow
    if ($live.window_class -cne [string]$requiredWindow.className -or
        [uint32]$live.client_width -ne [uint32]$requiredWindow.clientWidth -or
        [uint32]$live.client_height -ne [uint32]$requiredWindow.clientHeight) {
        throw 'Live window class or client size does not match the target registry.'
    }

    $attestation = [ordered]@{
        schema = [string]$target.representativeSceneGate.attestationSchema
        attestationKind = [string]$target.representativeSceneGate.attestationKind
        gameId = $GameId
        steamAppId = [string]$AppId
        steamBuildId = $ExpectedBuildId
        discoveryId = $DiscoveryId
        outputLeaf = $OutputLeaf
        executable = [ordered]@{ basename = [IO.Path]::GetFileName($exe); sha256 = $expectedExeHash }
        runtimeEvidence = [ordered]@{
            sha256 = $runtimeDocument.sha256; processId = $ProcessId; processStartTimeUtc = $runtimeStart
        }
        window = [ordered]@{
            className = $live.window_class; clientWidth = $live.client_width; clientHeight = $live.client_height
        }
        sceneKind = [string]$target.representativeSceneGate.sceneKind
        observedAtUtc = [DateTime]::UtcNow.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        nonce = New-RandomNonce
    }
    $created = Write-CreateNewUtf8Json -Path $OutputPath -Value $attestation
    $attestationHash = 'sha256:' + (Get-FileHash -LiteralPath $created -Algorithm SHA256).Hash.ToLowerInvariant()

    $validator = Join-Path $PSScriptRoot 'Test-GenericD3D11RepresentativeSceneAttestation.ps1'
    $validationText = (& $validator `
        -AttestationPath $created -ExpectedAttestationSha256 $attestationHash `
        -GameId $GameId -AppId ([string]$AppId) -BuildId $ExpectedBuildId `
        -ExpectedDiscoveryId $DiscoveryId -ExpectedOutputLeaf $OutputLeaf `
        -ExecutableBasename ([IO.Path]::GetFileName($exe)) -ExecutableSha256 $expectedExeHash `
        -RuntimeEvidencePath $runtimeDocument.path -ExpectedProcessId $ProcessId `
        -ExpectedProcessStartTimeUtc $runtimeStart `
        -ExpectedWindowClass ([string]$requiredWindow.className) `
        -ExpectedClientWidth ([uint32]$requiredWindow.clientWidth) `
        -ExpectedClientHeight ([uint32]$requiredWindow.clientHeight) `
        -MaximumAgeSeconds ([uint32]$target.representativeSceneGate.maximumAgeSeconds) | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Created attestation failed validation; preserve it for review.`n$validationText" }

    [pscustomobject][ordered]@{
        status = 'created_local_operator_attestation'
        scene_semantics_machine_verified = $false
        attestation_path = $created
        attestation_sha256 = $attestationHash
        runtime_evidence_sha256 = $runtimeDocument.sha256
        game_id = $GameId
        steam_app_id = [string]$AppId
        steam_build_id = $ExpectedBuildId
        discovery_id = $DiscoveryId
        output_leaf = $OutputLeaf
        process_id = $ProcessId
        process_start_time_utc = $runtimeStart
        window_class = $live.window_class
        client_width = $live.client_width
        client_height = $live.client_height
    } | ConvertTo-Json -Depth 3
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}

<#
.SYNOPSIS
  Rechecks one armed generic D3D11 discovery session and performs the guarded injection.

.DESCRIPTION
  This is the only supported injection entrypoint for the generic D3D11 DSV discovery lane.
  It binds the armed runtime report to the live process creation time, recollects a create-new
  no-anti-cheat runtime report immediately before injection, revalidates the exact process,
  config, executable and target registry, then calls dm-depth-inject with the kernel creation
  FILETIME pinned. It never launches a game, weakens a runtime gate or reuses a resident probe.
#>
[CmdletBinding(DefaultParameterSetName = 'Inject')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$AppId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^\d+$')]
    [string]$ExpectedBuildId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidateRange(1, [uint32]::MaxValue)]
    [uint32]$ProcessId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$ExpectedExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedExecutableSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$ArmRuntimeEvidencePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedArmConfigSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$RepresentativeSceneAttestationPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedRepresentativeSceneAttestationSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$OutputLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^[A-Za-z0-9_-]{1,128}$')]
    [string]$DiscoveryId,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\.json$')]
    [string]$PreInjectionEvidenceLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$InjectorPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedInjectorSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [string]$ProbeDllPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Inject')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExpectedProbeDllSha256,

    [Parameter(ParameterSetName = 'Inject')]
    [ValidateRange(1, 120)]
    [uint32]$MaximumArmEvidenceAgeSeconds = 120,

    [Parameter(ParameterSetName = 'Inject')]
    [ValidateRange(1, 30)]
    [uint32]$MaximumPreInjectionEvidenceAgeSeconds = 15,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:InjectionReceiptSchema = 'dm-generic-d3d11-dsv-injection-receipt/v1'
$script:MaximumBoundEvidenceBytes = 4MB
$script:MaximumInjectionReceiptBytes = 64KB
$script:InjectionReceiptLeaf = 'receipt.json'
$script:SceneSnapshotLeaf = 'representative-scene-attestation.json'
$script:ArmRuntimeSnapshotLeaf = 'arm-runtime-gate.json'
$script:PreInjectionRuntimeSnapshotLeaf = 'pre-injection-runtime-gate.json'
$script:ArmedConfigSnapshotLeaf = 'armed-config.json'

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

function ConvertFrom-JsonPreservingStrings {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)
    if ((Get-Command ConvertFrom-Json).Parameters.ContainsKey('DateKind')) {
        return ($Text | ConvertFrom-Json -DateKind String)
    }
    return ($Text | ConvertFrom-Json)
}

function Read-JsonFile {
    param([Parameter(Mandatory = $true)][string]$Path, [string]$Label = 'JSON')
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    Assert-NoReparseAncestor -Path $Path -Label $Label
    return ConvertFrom-JsonPreservingStrings `
        -Text (Get-Content -LiteralPath $Path -Raw -Encoding UTF8)
}

function Get-Sha256ForBytes {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = $hasher.ComputeHash($Bytes)
    } finally {
        $hasher.Dispose()
    }
    return 'sha256:' + [BitConverter]::ToString($digest).Replace('-', '').ToLowerInvariant()
}

function Open-StableBoundedJsonDocument {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][uint64]$MaximumBytes,
        [string]$Label = 'JSON document',
        [IO.FileShare]$FileShare = [IO.FileShare]::Read
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    $full = (Resolve-Path -LiteralPath $Path).Path
    Assert-NoReparseAncestor -Path $full -Label $Label
    $stream = [IO.File]::Open($full, [IO.FileMode]::Open, [IO.FileAccess]::Read, $FileShare)
    try {
        if ([uint64]$stream.Length -gt $MaximumBytes) {
            throw "$Label exceeds its $MaximumBytes-byte bound."
        }
        $length = [int]$stream.Length
        $bytes = [byte[]]::new($length)
        $offset = 0
        while ($offset -lt $length) {
            $read = $stream.Read($bytes, $offset, $length - $offset)
            if ($read -eq 0) { throw "$Label ended before its admitted byte length." }
            $offset += $read
        }
        if ($stream.ReadByte() -ne -1 -or $stream.Length -ne $length) {
            throw "$Label changed length while it was being read."
        }
        $resolvedAfter = (Resolve-Path -LiteralPath $Path).Path
        if (-not $resolvedAfter.Equals($full, [StringComparison]::OrdinalIgnoreCase)) {
            throw "$Label path changed while it was being read."
        }
        Assert-NoReparseAncestor -Path $resolvedAfter -Label $Label
        if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and
            $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
            throw "$Label must be UTF-8 without a BOM."
        }
        $text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
        try {
            $value = ConvertFrom-JsonPreservingStrings -Text $text
        } catch {
            throw "$Label is not strict UTF-8 JSON: $($_.Exception.Message)"
        }
        return [pscustomobject][ordered]@{
            path = $full
            sha256 = Get-Sha256ForBytes -Bytes $bytes
            lengthBytes = [uint64]$bytes.Length
            bytes = $bytes
            value = $value
            stream = $stream
        }
    } catch {
        $stream.Dispose()
        throw
    }
}

function Read-StableBoundedJsonDocument {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][uint64]$MaximumBytes,
        [string]$Label = 'JSON document'
    )
    $document = Open-StableBoundedJsonDocument `
        -Path $Path -MaximumBytes $MaximumBytes -Label $Label
    try {
        return [pscustomobject][ordered]@{
            path = $document.path
            sha256 = $document.sha256
            lengthBytes = $document.lengthBytes
            bytes = $document.bytes
            value = $document.value
        }
    } finally {
        $document.stream.Dispose()
    }
}

function Assert-OpenJsonDocumentUnchanged {
    param(
        [Parameter(Mandatory = $true)]$Document,
        [string]$Label = 'open JSON document'
    )
    $stream = $Document.stream
    if (-not $stream.CanRead -or -not $stream.CanSeek) { throw "$Label stream is not readable and seekable." }
    if ([uint64]$stream.Length -ne [uint64]$Document.lengthBytes) {
        throw "$Label length changed while its lock was held."
    }
    $stream.Seek(0, [IO.SeekOrigin]::Begin) | Out-Null
    $current = [byte[]]::new([int]$Document.lengthBytes)
    $offset = 0
    while ($offset -lt $current.Length) {
        $read = $stream.Read($current, $offset, $current.Length - $offset)
        if ($read -eq 0) { throw "$Label ended during its post-injection reread." }
        $offset += $read
    }
    if ($stream.ReadByte() -ne -1 -or
        [uint64]$stream.Length -ne [uint64]$Document.lengthBytes) {
        throw "$Label changed length during its post-injection reread."
    }
    for ($index = 0; $index -lt $current.Length; $index++) {
        if ($current[$index] -ne $Document.bytes[$index]) {
            throw "$Label bytes changed while its lock was held."
        }
    }
    if ((Get-Sha256ForBytes $current) -cne $Document.sha256) {
        throw "$Label hash changed while its lock was held."
    }
}

function Get-SafeRootRelativeLocator {
    param(
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][string]$Path,
        [string]$Label = 'evidence path'
    )
    $root = (Resolve-Path -LiteralPath $EvidenceRoot).Path.TrimEnd(
        [IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $full = (Resolve-Path -LiteralPath $Path).Path
    Assert-NoReparseAncestor -Path $root -Label 'evidence root'
    Assert-NoReparseAncestor -Path $full -Label $Label
    $prefix = $root + [IO.Path]::DirectorySeparatorChar
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must be contained by EvidenceRoot."
    }
    $locator = $full.Substring($prefix.Length).Replace('\', '/')
    if ($locator.Length -eq 0 -or $locator.Length -gt 512 -or
        $locator.StartsWith('/') -or $locator.Contains('\')) {
        throw "$Label does not have a bounded canonical root-relative locator."
    }
    foreach ($segment in $locator.Split('/')) {
        if ($segment -notmatch '^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$' -or
            $segment -eq '.' -or $segment -eq '..') {
            throw "$Label locator contains an unsafe path segment."
        }
    }
    return $locator
}

function Assert-ReservedOutputAbsent {
    param(
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][string]$OutputPath
    )
    $root = (Resolve-Path -LiteralPath $EvidenceRoot).Path
    Assert-NoReparseAncestor -Path $root -Label 'evidence root'
    $full = [IO.Path]::GetFullPath($OutputPath)
    if (-not ([IO.Path]::GetDirectoryName($full)).Equals(
        $root, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Reserved discovery output must be a direct child of EvidenceRoot.'
    }
    if (Test-Path -LiteralPath $full) {
        throw "Reserved discovery output already exists: $full"
    }
}

function Write-SealedDirectChildBytesCreateNew {
    param(
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)]
        [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]{0,255}$')]
        [string]$Leaf,
        [Parameter(Mandatory = $true)][byte[]]$Bytes,
        [Parameter(Mandatory = $true)][uint64]$MaximumBytes,
        [string]$Label = 'sealed evidence'
    )
    if ([uint64]$Bytes.Length -gt $MaximumBytes) {
        throw "$Label exceeds its $MaximumBytes-byte bound."
    }
    $rootBefore = (Resolve-Path -LiteralPath $EvidenceRoot).Path
    Assert-NoReparseAncestor -Path $rootBefore -Label 'evidence root'
    $path = [IO.Path]::GetFullPath((Join-Path $rootBefore $Leaf))
    if (-not ([IO.Path]::GetDirectoryName($path)).Equals(
        $rootBefore, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must be a direct child of EvidenceRoot."
    }
    if (Test-Path -LiteralPath $path) { throw "Refusing to overwrite existing $Label`: $path" }
    $stream = [IO.File]::Open(
        $path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $stream.Write($Bytes, 0, $Bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    $sealed = (Resolve-Path -LiteralPath $path).Path
    $rootAfter = (Resolve-Path -LiteralPath $EvidenceRoot).Path
    Assert-NoReparseAncestor -Path $sealed -Label $Label
    if (-not $rootAfter.Equals($rootBefore, [StringComparison]::OrdinalIgnoreCase) -or
        -not ([IO.Path]::GetDirectoryName($sealed)).Equals(
            $rootAfter, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label path changed while it was being sealed."
    }
    $sealedDocument = Read-StableBoundedJsonDocument `
        -Path $sealed -MaximumBytes $MaximumBytes -Label $Label
    $expectedSha256 = Get-Sha256ForBytes -Bytes $Bytes
    if ($sealedDocument.sha256 -cne $expectedSha256 -or
        $sealedDocument.lengthBytes -ne [uint64]$Bytes.Length) {
        throw "$Label bytes changed while they were being sealed."
    }
    return [pscustomobject][ordered]@{
        path = $sealedDocument.path
        locator = Get-SafeRootRelativeLocator `
            -EvidenceRoot $rootAfter -Path $sealedDocument.path -Label $Label
        sha256 = $sealedDocument.sha256
        lengthBytes = $sealedDocument.lengthBytes
        value = $sealedDocument.value
    }
}

function Assert-InjectorSuccessOutput {
    param(
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Output,
        [Parameter(Mandatory = $true)][string]$ExpectedProbeSha256
    )
    if ($ExitCode -ne 0) { throw "Exact-instance depth injection failed (exit=$ExitCode)." }
    $expectedLine = "loaded_probe_sha256=$ExpectedProbeSha256"
    if ($expectedLine -notmatch '^loaded_probe_sha256=sha256:[0-9a-f]{64}$') {
        throw 'Expected probe hash is not canonical.'
    }
    $normalized = $Output.Replace("`r`n", "`n")
    if ($normalized.EndsWith("`n", [StringComparison]::Ordinal)) {
        $normalized = $normalized.Substring(0, $normalized.Length - 1)
    }
    if ($normalized.Contains("`n") -or $normalized.Contains("`r") -or
        $normalized -cne $expectedLine) {
        throw 'Depth injector stdout was not exactly one canonical loaded_probe_sha256 line.'
    }
    return $ExpectedProbeSha256
}

function New-InjectionBundleStagingDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][string]$FinalBundleLeaf
    )
    if ($FinalBundleLeaf -notmatch '^[A-Za-z0-9][A-Za-z0-9_.-]{0,255}$') {
        throw 'Injection receipt bundle leaf is unsafe.'
    }
    $root = (Resolve-Path -LiteralPath $EvidenceRoot).Path
    Assert-NoReparseAncestor -Path $root -Label 'evidence root'
    $finalPath = [IO.Path]::GetFullPath((Join-Path $root $FinalBundleLeaf))
    if (-not ([IO.Path]::GetDirectoryName($finalPath)).Equals(
        $root, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Injection receipt bundle must be a direct child of EvidenceRoot.'
    }
    if (Test-Path -LiteralPath $finalPath) {
        throw "Injection receipt bundle already exists: $finalPath"
    }
    $pendingLeaf = '.pending-' + [Guid]::NewGuid().ToString('N')
    $pendingPath = [IO.Path]::GetFullPath((Join-Path $root $pendingLeaf))
    New-Item -ItemType Directory -Path $pendingPath -ErrorAction Stop | Out-Null
    $resolved = (Resolve-Path -LiteralPath $pendingPath).Path
    Assert-NoReparseAncestor -Path $resolved -Label 'pending injection receipt bundle'
    if (-not ([IO.Path]::GetDirectoryName($resolved)).Equals(
        $root, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Pending injection receipt bundle escaped EvidenceRoot.'
    }
    return [pscustomobject][ordered]@{
        root = $root
        pendingLeaf = $pendingLeaf
        pendingPath = $resolved
        finalLeaf = $FinalBundleLeaf
        finalPath = $finalPath
    }
}

function Remove-OwnedPendingInjectionBundle {
    param(
        [Parameter(Mandatory = $true)]$Bundle,
        [Parameter(Mandatory = $true)][string[]]$AllowedLeaves
    )
    if (-not (Test-Path -LiteralPath $Bundle.pendingPath -PathType Container)) { return }
    $root = (Resolve-Path -LiteralPath $Bundle.root).Path
    $pending = (Resolve-Path -LiteralPath $Bundle.pendingPath).Path
    Assert-NoReparseAncestor -Path $pending -Label 'pending injection receipt bundle'
    if (-not ([IO.Path]::GetDirectoryName($pending)).Equals(
        $root, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($pending) -cne $Bundle.pendingLeaf -or
        $Bundle.pendingLeaf -notmatch '^\.pending-[0-9a-f]{32}$') {
        throw 'Refusing to clean up an unowned pending injection receipt bundle.'
    }
    foreach ($item in @(Get-ChildItem -LiteralPath $pending -Force)) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            $item.PSIsContainer -or -not ($AllowedLeaves -ccontains $item.Name)) {
            throw 'Refusing to clean up a pending bundle containing an unexpected entry.'
        }
    }
    Remove-Item -LiteralPath $pending -Recurse -Force
    if (Test-Path -LiteralPath $pending) {
        throw 'Failed to clean up the pending injection receipt bundle.'
    }
}

function Publish-InjectionReceiptBundle {
    param(
        [Parameter(Mandatory = $true)]$Bundle,
        [Parameter(Mandatory = $true)][string[]]$ExpectedLeaves,
        [Parameter(Mandatory = $true)][string]$ExpectedReceiptSha256,
        [Parameter(Mandatory = $true)][uint64]$ExpectedReceiptLengthBytes
    )
    $rootBefore = (Resolve-Path -LiteralPath $Bundle.root).Path
    $pending = (Resolve-Path -LiteralPath $Bundle.pendingPath).Path
    Assert-NoReparseAncestor -Path $pending -Label 'pending injection receipt bundle'
    if (-not ([IO.Path]::GetDirectoryName($pending)).Equals(
        $rootBefore, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Pending injection receipt bundle is not a direct child of EvidenceRoot.'
    }
    $actualLeaves = @(Get-ChildItem -LiteralPath $pending -Force)
    if ($actualLeaves.Count -ne $ExpectedLeaves.Count) {
        throw 'Pending injection receipt bundle is incomplete.'
    }
    foreach ($item in $actualLeaves) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            $item.PSIsContainer -or -not ($ExpectedLeaves -ccontains $item.Name)) {
            throw 'Pending injection receipt bundle contains an unexpected entry.'
        }
    }
    if (Test-Path -LiteralPath $Bundle.finalPath) {
        throw "Refusing to overwrite an existing injection receipt bundle: $($Bundle.finalPath)"
    }
    [IO.Directory]::Move($pending, $Bundle.finalPath)
    $rootAfter = (Resolve-Path -LiteralPath $Bundle.root).Path
    $final = (Resolve-Path -LiteralPath $Bundle.finalPath).Path
    Assert-NoReparseAncestor -Path $final -Label 'published injection receipt bundle'
    if (-not $rootAfter.Equals($rootBefore, [StringComparison]::OrdinalIgnoreCase) -or
        -not ([IO.Path]::GetDirectoryName($final)).Equals(
            $rootAfter, [StringComparison]::OrdinalIgnoreCase) -or
        (Test-Path -LiteralPath $pending)) {
        throw 'Injection receipt bundle path changed during atomic publication.'
    }
    $receiptPath = Join-Path $final $script:InjectionReceiptLeaf
    $receipt = Read-StableBoundedJsonDocument `
        -Path $receiptPath -MaximumBytes $script:MaximumInjectionReceiptBytes `
        -Label 'published injection receipt'
    if ($receipt.sha256 -cne $ExpectedReceiptSha256 -or
        $receipt.lengthBytes -ne $ExpectedReceiptLengthBytes) {
        throw 'Published injection receipt bytes differ from the committed bytes.'
    }
    return [pscustomobject][ordered]@{
        path = $receipt.path
        locator = Get-SafeRootRelativeLocator `
            -EvidenceRoot $rootAfter -Path $receipt.path -Label 'published injection receipt'
        sha256 = $receipt.sha256
        lengthBytes = $receipt.lengthBytes
        value = $receipt.value
    }
}

function Get-CanonicalUtcTimestamp {
    param([Parameter(Mandatory = $true)][string]$Value, [string]$Label = 'UTC timestamp')
    try {
        $parsed = [DateTimeOffset]::ParseExact(
            $Value,
            'o',
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    } catch {
        throw "$Label is not a round-trip ISO-8601 timestamp."
    }
    $canonical = $parsed.UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    if ($Value -cne $canonical) { throw "$Label must be canonical UTC with the Z suffix." }
    return $canonical
}

function Get-LiveProcessIdentity {
    param([Parameter(Mandatory = $true)][uint32]$Id)
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
    $startAfter = $after.StartTime.ToUniversalTime()
    if ($startBefore.Ticks -ne $startAfter.Ticks) {
        throw "Process $Id changed instances while its identity was being read."
    }
    return [pscustomobject][ordered]@{
        process_id = $Id
        executable_path = $path
        executable_sha256 = $hash
        start_time_utc = $startAfter.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        start_time_filetime = [uint64]$startAfter.ToFileTimeUtc()
    }
}

function Assert-LiveProcessIdentity {
    param(
        [Parameter(Mandatory = $true)][uint32]$Id,
        [Parameter(Mandatory = $true)][string]$ExpectedPath,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedStartTimeUtc
    )
    $identity = Get-LiveProcessIdentity -Id $Id
    if (-not $identity.executable_path.Equals(
        [IO.Path]::GetFullPath($ExpectedPath), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Live process executable path drifted.'
    }
    if ([string]$identity.executable_sha256 -cne $ExpectedSha256) {
        throw 'Live process executable hash drifted.'
    }
    if ([string]$identity.start_time_utc -cne $ExpectedStartTimeUtc) {
        throw "Live PID now refers to a different process instance (expected_start=$ExpectedStartTimeUtc live_start=$($identity.start_time_utc))."
    }
    return $identity
}

function Assert-RuntimeGateEvidence {
    param(
        [Parameter(Mandatory = $true)]$Runtime,
        [Parameter(Mandatory = $true)][uint32]$ExpectedProcessId,
        [Parameter(Mandatory = $true)][string]$ExpectedPath,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedStartTimeUtc,
        [Parameter(Mandatory = $true)][uint32]$MaximumAgeSeconds,
        [string]$ExpectedCollectorSha256 = '',
        [string]$Label = 'runtime gate evidence'
    )
    if ($Runtime.schema -cne 'game-runtime-gate-evidence/v1' -or $Runtime.status -cne 'pass') {
        throw "$Label is not a passing game-runtime-gate-evidence/v1 report."
    }
    if ([uint32]$Runtime.process.process_id -ne $ExpectedProcessId) { throw "$Label PID drifted." }
    if ([string]$Runtime.process.executable_sha256 -cne $ExpectedSha256) { throw "$Label EXE hash drifted." }
    if (-not ([IO.Path]::GetFullPath([string]$Runtime.process.executable_path)).Equals(
        [IO.Path]::GetFullPath($ExpectedPath), [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label EXE path drifted."
    }
    $reportedStart = Get-CanonicalUtcTimestamp `
        -Value ([string]$Runtime.process.start_time_utc) `
        -Label "$Label process.start_time_utc"
    if ($reportedStart -cne $ExpectedStartTimeUtc) { throw "$Label process instance drifted." }
    if ($Runtime.graphics.expected_api -cne 'd3d11' -or
        -not (@($Runtime.graphics.hits) -contains 'd3d11.dll')) {
        throw "$Label does not prove D3D11."
    }
    if (-not $Runtime.module_enumeration.complete -or
        -not $Runtime.anti_cheat.require_none_loaded -or
        -not $Runtime.anti_cheat.service_enumeration_complete -or
        -not $Runtime.anti_cheat.driver_enumeration_complete -or
        @($Runtime.anti_cheat.module_hits).Count -ne 0 -or
        [int]$Runtime.anti_cheat.running_service_hits -ne 0 -or
        [int]$Runtime.anti_cheat.running_driver_hits -ne 0 -or
        @($Runtime.blocking).Count -ne 0) {
        throw "$Label protection enumeration is incomplete or blocked."
    }
    if ($ExpectedCollectorSha256 -and
        [string]$Runtime.collector_sha256 -cne $ExpectedCollectorSha256) {
        throw "$Label was not produced by the current runtime collector."
    }
    $captured = Get-CanonicalUtcTimestamp -Value ([string]$Runtime.captured_utc) -Label "$Label captured_utc"
    $capturedUtc = [DateTimeOffset]::ParseExact(
        $captured, 'o', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind).UtcDateTime
    $age = ([DateTime]::UtcNow - $capturedUtc).TotalSeconds
    if ($age -lt -5 -or $age -gt $MaximumAgeSeconds) {
        throw "$Label is not fresh enough (age_seconds=$([Math]::Round($age, 1)))."
    }
    Assert-LiveProcessIdentity `
        -Id $ExpectedProcessId `
        -ExpectedPath $ExpectedPath `
        -ExpectedSha256 $ExpectedSha256 `
        -ExpectedStartTimeUtc $ExpectedStartTimeUtc | Out-Null
}

function Get-FixedConfigPath {
    $programData = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if (-not $programData) { throw 'CommonApplicationData is unavailable.' }
    return [IO.Path]::Combine($programData, 'DataMiner', 'depth-lab', 'probe-config.json')
}

function Assert-ArmedConfigDocumentIdentity {
    param(
        [Parameter(Mandatory = $true)]$Config,
        [Parameter(Mandatory = $true)][string]$ExpectedDiscoveryId,
        [Parameter(Mandatory = $true)][string]$ExpectedExeSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedEvidenceRoot,
        [Parameter(Mandatory = $true)][string]$ExpectedOutput,
        [Parameter(Mandatory = $true)][string]$ExpectedRepresentativeSceneAttestationSha256
    )
    if (-not $Config.enabled -or
        $Config.session_id -cne "dsv-discovery-$ExpectedDiscoveryId" -or
        $Config.dsv_discovery.schema -cne 'd3d11-dsv-discovery-config/v2' -or
        $Config.dsv_discovery.discovery_id -cne $ExpectedDiscoveryId -or
        $Config.dsv_discovery.executable_sha256 -cne $ExpectedExeSha256 -or
        $Config.representative_scene_attestation_sha256 -cne $ExpectedRepresentativeSceneAttestationSha256 -or
        -not ([IO.Path]::GetFullPath([string]$Config.dsv_discovery.evidence_root)).Equals(
            $ExpectedEvidenceRoot, [StringComparison]::OrdinalIgnoreCase) -or
        -not ([IO.Path]::GetFullPath([string]$Config.output_dir)).Equals(
            $ExpectedOutput, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The active fixed probe config is not the exact armed discovery identity.'
    }
}

function Assert-ArmedConfigIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedDiscoveryId,
        [Parameter(Mandatory = $true)][string]$ExpectedExeSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedEvidenceRoot,
        [Parameter(Mandatory = $true)][string]$ExpectedOutput,
        [Parameter(Mandatory = $true)][string]$ExpectedConfigSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedRepresentativeSceneAttestationSha256
    )
    $document = Read-StableBoundedJsonDocument `
        -Path $Path -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'active fixed probe config'
    if ($document.sha256 -cne $ExpectedConfigSha256) {
        throw 'The active fixed probe config bytes differ from the exact armed config.'
    }
    Assert-ArmedConfigDocumentIdentity `
        -Config $document.value -ExpectedDiscoveryId $ExpectedDiscoveryId `
        -ExpectedExeSha256 $ExpectedExeSha256 -ExpectedEvidenceRoot $ExpectedEvidenceRoot `
        -ExpectedOutput $ExpectedOutput `
        -ExpectedRepresentativeSceneAttestationSha256 $ExpectedRepresentativeSceneAttestationSha256
}

function Assert-RepresentativeSceneAttestation {
    param(
        [Parameter(Mandatory = $true)]$Target,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedGameId,
        [Parameter(Mandatory = $true)][string]$ExpectedAppId,
        [Parameter(Mandatory = $true)][string]$ExpectedBuildId,
        [Parameter(Mandatory = $true)][string]$ExpectedDiscoveryId,
        [Parameter(Mandatory = $true)][string]$ExpectedOutputLeaf,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableBasename,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutableSha256,
        [Parameter(Mandatory = $true)][string]$RuntimeEvidencePath,
        [Parameter(Mandatory = $true)][uint32]$ExpectedProcessId,
        [Parameter(Mandatory = $true)][string]$ExpectedProcessStartTimeUtc
    )
    $sceneGate = $Target.representativeSceneGate
    if (-not $sceneGate.required) { throw 'Target registry does not require representative-scene attestation.' }
    $validator = Join-Path $PSScriptRoot 'Test-GenericD3D11RepresentativeSceneAttestation.ps1'
    $text = (& $validator `
        -AttestationPath $Path -ExpectedAttestationSha256 $ExpectedSha256 `
        -GameId $ExpectedGameId -AppId $ExpectedAppId -BuildId $ExpectedBuildId `
        -ExpectedDiscoveryId $ExpectedDiscoveryId -ExpectedOutputLeaf $ExpectedOutputLeaf `
        -ExecutableBasename $ExpectedExecutableBasename -ExecutableSha256 $ExpectedExecutableSha256 `
        -RuntimeEvidencePath $RuntimeEvidencePath -ExpectedProcessId $ExpectedProcessId `
        -ExpectedProcessStartTimeUtc $ExpectedProcessStartTimeUtc `
        -ExpectedWindowClass ([string]$sceneGate.expectedWindow.className) `
        -ExpectedClientWidth ([uint32]$sceneGate.expectedWindow.clientWidth) `
        -ExpectedClientHeight ([uint32]$sceneGate.expectedWindow.clientHeight) `
        -MaximumAgeSeconds ([uint32]$sceneGate.maximumAgeSeconds) | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "Representative-scene attestation validation failed.`n$text" }
    $report = $text | ConvertFrom-Json
    if ($report.status -cne 'pass' -or $report.attestation_sha256 -cne $ExpectedSha256 -or
        $report.discovery_id -cne $ExpectedDiscoveryId -or
        $report.output_leaf -cne $ExpectedOutputLeaf -or
        $report.scene_kind -cne 'representative_offline_single_player') {
        throw 'Representative-scene attestation did not return the exact passing identity.'
    }
}

function Copy-JsonObject {
    param([Parameter(Mandatory = $true)]$Value)
    return ConvertFrom-JsonPreservingStrings -Text ($Value | ConvertTo-Json -Depth 20)
}

function Assert-SelfTestBlocked {
    param([Parameter(Mandatory = $true)][scriptblock]$Action, [string]$Name = 'negative case')
    $blocked = $false
    try { & $Action | Out-Null } catch { $blocked = $true }
    if (-not $blocked) { throw "self-test negative case passed unexpectedly: $Name" }
}

function Invoke-SelfTest {
    $identity = Get-LiveProcessIdentity -Id $PID
    $now = [DateTime]::UtcNow.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    $runtime = [pscustomobject]@{
        schema = 'game-runtime-gate-evidence/v1'; captured_utc = $now
        collector_sha256 = 'sha256:' + ('c' * 64)
        process = [pscustomobject]@{
            process_id = $PID; executable_path = $identity.executable_path
            executable_sha256 = $identity.executable_sha256; start_time_utc = $identity.start_time_utc
        }
        module_enumeration = [pscustomobject]@{ complete = $true }
        graphics = [pscustomobject]@{ expected_api = 'd3d11'; hits = @('d3d11.dll', 'dxgi.dll') }
        anti_cheat = [pscustomobject]@{
            require_none_loaded = $true; service_enumeration_complete = $true
            driver_enumeration_complete = $true; module_hits = @()
            running_service_hits = 0; running_driver_hits = 0
        }
        status = 'pass'; blocking = @()
    }
    Assert-RuntimeGateEvidence `
        -Runtime $runtime -ExpectedProcessId $PID -ExpectedPath $identity.executable_path `
        -ExpectedSha256 $identity.executable_sha256 -ExpectedStartTimeUtc $identity.start_time_utc `
        -MaximumAgeSeconds 15 -ExpectedCollectorSha256 $runtime.collector_sha256
    $tests = 1

    $negativeCases = @(
        @{ Name = 'PID reuse or process restart'; Mutate = {
            param($value) $value.process.start_time_utc = [DateTime]::UtcNow.AddSeconds(-1).ToString('o')
        } },
        @{ Name = 'PID drift'; Mutate = { param($value) $value.process.process_id = [uint32]($PID + 1) } },
        @{ Name = 'EXE path drift'; Mutate = { param($value) $value.process.executable_path = 'C:\wrong\Game.exe' } },
        @{ Name = 'EXE hash drift'; Mutate = { param($value) $value.process.executable_sha256 = 'sha256:' + ('b' * 64) } },
        @{ Name = 'stale evidence'; Mutate = { param($value) $value.captured_utc = [DateTime]::UtcNow.AddMinutes(-5).ToString('o') } },
        @{ Name = 'anti-cheat module appeared'; Mutate = { param($value) $value.anti_cheat.module_hits = @('bedaisy.sys') } },
        @{ Name = 'blocking reason appeared'; Mutate = { param($value) $value.blocking = @('anti_cheat_driver_running: bedaisy') } },
        @{ Name = 'module enumeration incomplete'; Mutate = { param($value) $value.module_enumeration.complete = $false } },
        @{ Name = 'collector hash drift'; Mutate = { param($value) $value.collector_sha256 = 'sha256:' + ('d' * 64) } }
    )
    foreach ($case in $negativeCases) {
        $candidate = Copy-JsonObject $runtime
        & $case.Mutate $candidate
        $blocked = $false
        try {
            Assert-RuntimeGateEvidence `
                -Runtime $candidate -ExpectedProcessId $PID -ExpectedPath $identity.executable_path `
                -ExpectedSha256 $identity.executable_sha256 -ExpectedStartTimeUtc $identity.start_time_utc `
                -MaximumAgeSeconds 15 -ExpectedCollectorSha256 $runtime.collector_sha256
        } catch { $blocked = $true }
        if (-not $blocked) { throw "self-test negative case passed unexpectedly: $($case.Name)" }
        $tests++
    }

    $configTestRoot = Join-Path ([IO.Path]::GetTempPath()) ("dm-generic-config-" + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($configTestRoot) | Out-Null
    try {
        $configTestPath = Join-Path $configTestRoot 'probe-config.json'
        $configTestOutput = Join-Path $configTestRoot 'output'
        $configTest = [ordered]@{
            enabled = $true; output_dir = $configTestOutput; unbind_from_end = 4
            max_frames = $null; present_only = $false; watermark_only = $false
            writer_bypass = $false; target_depth_hz = $null; lane_fps = $null
            camera_probe = $false; camera_discovery = $null
            representative_scene_attestation_sha256 = 'sha256:' + ('b' * 64)
            dsv_discovery = [ordered]@{
                schema = 'd3d11-dsv-discovery-config/v2'; discovery_id = 'selftest'
                executable_sha256 = 'sha256:' + ('a' * 64); evidence_root = $configTestRoot
                limits = [ordered]@{ max_frames = 600; max_candidates = 128; max_candidates_per_frame = 32 }
            }
            camera_production = $null; depth_profile_path = $null; depth_profile_sha256 = $null
            session_id = 'dsv-discovery-selftest'
        }
        [IO.File]::WriteAllText(
            $configTestPath,
            ($configTest | ConvertTo-Json -Depth 8),
            [Text.UTF8Encoding]::new($false))
        $configTestHash = 'sha256:' + (Get-FileHash -LiteralPath $configTestPath -Algorithm SHA256).Hash.ToLowerInvariant()
        Assert-ArmedConfigIdentity `
            -Path $configTestPath -ExpectedDiscoveryId 'selftest' `
            -ExpectedExeSha256 ('sha256:' + ('a' * 64)) `
            -ExpectedEvidenceRoot $configTestRoot -ExpectedOutput $configTestOutput `
            -ExpectedConfigSha256 $configTestHash `
            -ExpectedRepresentativeSceneAttestationSha256 ('sha256:' + ('b' * 64))
        $tests++
        $legacyConfig = Copy-JsonObject $configTest
        $legacyConfig.dsv_discovery.schema = 'd3d11-dsv-discovery-config/v1'
        [IO.File]::WriteAllText(
            $configTestPath,
            ($legacyConfig | ConvertTo-Json -Depth 8),
            [Text.UTF8Encoding]::new($false))
        $legacyConfigHash = 'sha256:' + (Get-FileHash -LiteralPath $configTestPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $blocked = $false
        try {
            Assert-ArmedConfigIdentity `
                -Path $configTestPath -ExpectedDiscoveryId 'selftest' `
                -ExpectedExeSha256 ('sha256:' + ('a' * 64)) `
                -ExpectedEvidenceRoot $configTestRoot -ExpectedOutput $configTestOutput `
                -ExpectedConfigSha256 $legacyConfigHash `
                -ExpectedRepresentativeSceneAttestationSha256 ('sha256:' + ('b' * 64))
        } catch { $blocked = $true }
        if (-not $blocked) { throw 'self-test accepted retired discovery config v1.' }
        $tests++
        [IO.File]::WriteAllText(
            $configTestPath,
            ($configTest | ConvertTo-Json -Depth 8),
            [Text.UTF8Encoding]::new($false))
        $configTest.dsv_discovery.limits.max_frames = 1200
        [IO.File]::WriteAllText(
            $configTestPath,
            ($configTest | ConvertTo-Json -Depth 8),
            [Text.UTF8Encoding]::new($false))
        $blocked = $false
        try {
            Assert-ArmedConfigIdentity `
                -Path $configTestPath -ExpectedDiscoveryId 'selftest' `
                -ExpectedExeSha256 ('sha256:' + ('a' * 64)) `
                -ExpectedEvidenceRoot $configTestRoot -ExpectedOutput $configTestOutput `
                -ExpectedConfigSha256 $configTestHash `
                -ExpectedRepresentativeSceneAttestationSha256 ('sha256:' + ('b' * 64))
        } catch { $blocked = $true }
        if (-not $blocked) { throw 'self-test accepted a post-arm discovery limit drift.' }
        $tests++
    } finally {
        if (Test-Path -LiteralPath $configTestRoot) {
            Remove-Item -LiteralPath $configTestRoot -Recurse -Force
        }
    }

    $probeHash = 'sha256:' + ('e' * 64)
    Assert-InjectorSuccessOutput -ExitCode 0 `
        -Output ("loaded_probe_sha256=$probeHash`r`n") `
        -ExpectedProbeSha256 $probeHash | Out-Null
    $tests++
    foreach ($stdoutCase in @(
        @{ Name = 'nonzero injector exit'; Exit = 1; Text = "loaded_probe_sha256=$probeHash`n" },
        @{ Name = 'extra injector stdout line'; Exit = 0; Text = "loaded_probe_sha256=$probeHash`nextra`n" },
        @{ Name = 'uppercase injector hash'; Exit = 0; Text = "loaded_probe_sha256=$($probeHash.ToUpperInvariant())`n" },
        @{ Name = 'missing injector hash prefix'; Exit = 0; Text = "$probeHash`n" }
    )) {
        Assert-SelfTestBlocked {
            Assert-InjectorSuccessOutput `
                -ExitCode $stdoutCase.Exit -Output $stdoutCase.Text `
                -ExpectedProbeSha256 $probeHash
        } $stdoutCase.Name
        $tests++
    }

    $receiptTestRoot = Join-Path ([IO.Path]::GetTempPath()) (
        'dm-generic-injection-receipt-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($receiptTestRoot) | Out-Null
    try {
        $driftPath = Join-Path $receiptTestRoot 'preexisting-writer-config.json'
        $originalDriftBytes = [Text.UTF8Encoding]::new($false).GetBytes("{`"value`":1}`n")
        [IO.File]::WriteAllBytes($driftPath, $originalDriftBytes)
        $preexistingWriter = [IO.File]::Open(
            $driftPath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite,
            [IO.FileShare]::ReadWrite)
        try {
            Assert-SelfTestBlocked {
                Open-StableBoundedJsonDocument `
                    $driftPath $script:MaximumBoundEvidenceBytes `
                    'config with pre-existing writer' ([IO.FileShare]::Read)
            } 'pre-existing config writer admission'
            $tests++
        } finally {
            $preexistingWriter.Dispose()
        }
        $changedDriftBytes = [Text.UTF8Encoding]::new($false).GetBytes("{`"value`":2}`n")
        $syntheticDriftDocument = [pscustomobject]@{
            stream = [IO.MemoryStream]::new($changedDriftBytes, $false)
            bytes = $originalDriftBytes
            lengthBytes = [uint64]$originalDriftBytes.Length
            sha256 = Get-Sha256ForBytes $originalDriftBytes
        }
        try {
            Assert-SelfTestBlocked {
                Assert-OpenJsonDocumentUnchanged `
                    $syntheticDriftDocument 'post-injection config reread'
            } 'post-injection config byte drift'
            $tests++
        } finally {
            $syntheticDriftDocument.stream.Dispose()
        }

        $fakeOutput = Join-Path $receiptTestRoot 'selftest-output'
        Assert-ReservedOutputAbsent $receiptTestRoot $fakeOutput
        $tests++
        [IO.Directory]::CreateDirectory($fakeOutput) | Out-Null
        Assert-SelfTestBlocked {
            Assert-ReservedOutputAbsent $receiptTestRoot $fakeOutput
        } 'pre-injection output already exists'
        $tests++
        Remove-Item -LiteralPath $fakeOutput -Recurse -Force

        $leaves = @(
            $script:SceneSnapshotLeaf, $script:ArmRuntimeSnapshotLeaf,
            $script:PreInjectionRuntimeSnapshotLeaf, $script:ArmedConfigSnapshotLeaf,
            $script:InjectionReceiptLeaf)
        $jsonBytes = [Text.UTF8Encoding]::new($false).GetBytes("{}`n")

        # A failed injector never gets a receipt or a recognizable final bundle. A partial pending
        # bundle is safely removed and cannot be consumed.
        $failedBundle = New-InjectionBundleStagingDirectory `
            $receiptTestRoot 'failed-output-injection-receipt'
        Write-SealedDirectChildBytesCreateNew `
            $failedBundle.pendingPath $script:SceneSnapshotLeaf $jsonBytes `
            $script:MaximumBoundEvidenceBytes 'failed-run source snapshot' | Out-Null
        Remove-OwnedPendingInjectionBundle $failedBundle $leaves
        if (Test-Path -LiteralPath $failedBundle.finalPath) {
            throw 'self-test published an injection receipt for an unsuccessful injection.'
        }
        $tests++

        # Model the asynchronous probe boundary: output is absent at the irreversible call, may
        # appear immediately after it, and is never used as evidence for bundle publication.
        $bundle = New-InjectionBundleStagingDirectory `
            $receiptTestRoot 'selftest-output-injection-receipt'
        foreach ($leaf in $leaves[0..3]) {
            Write-SealedDirectChildBytesCreateNew `
                $bundle.pendingPath $leaf $jsonBytes $script:MaximumBoundEvidenceBytes `
                'self-test source snapshot' | Out-Null
        }
        Assert-ReservedOutputAbsent $receiptTestRoot $fakeOutput
        [IO.Directory]::CreateDirectory($fakeOutput) | Out-Null
        $receiptBytes = [Text.UTF8Encoding]::new($false).GetBytes(
            "{`"schema`":`"$script:InjectionReceiptSchema`"}`n")
        $receiptHash = Get-Sha256ForBytes $receiptBytes
        Write-SealedDirectChildBytesCreateNew `
            $bundle.pendingPath $script:InjectionReceiptLeaf $receiptBytes `
            $script:MaximumInjectionReceiptBytes 'self-test injection receipt' | Out-Null
        $published = Publish-InjectionReceiptBundle `
            $bundle $leaves $receiptHash ([uint64]$receiptBytes.Length)
        if ($published.sha256 -cne $receiptHash -or
            -not (Test-Path -LiteralPath $fakeOutput -PathType Container)) {
            throw 'self-test coupled injection receipt publication to asynchronous probe output.'
        }
        $tests++

        Assert-SelfTestBlocked {
            New-InjectionBundleStagingDirectory `
                $receiptTestRoot 'selftest-output-injection-receipt'
        } 'published bundle overwrite'
        $tests++

        $partial = New-InjectionBundleStagingDirectory `
            $receiptTestRoot 'partial-output-injection-receipt'
        Write-SealedDirectChildBytesCreateNew `
            $partial.pendingPath $script:SceneSnapshotLeaf $jsonBytes `
            $script:MaximumBoundEvidenceBytes 'partial source snapshot' | Out-Null
        Assert-SelfTestBlocked {
            Publish-InjectionReceiptBundle `
                $partial $leaves $receiptHash ([uint64]$receiptBytes.Length)
        } 'partial bundle publication'
        Remove-OwnedPendingInjectionBundle $partial $leaves
        $tests++

        $collision = New-InjectionBundleStagingDirectory `
            $receiptTestRoot 'collision-output-injection-receipt'
        foreach ($leaf in $leaves[0..3]) {
            Write-SealedDirectChildBytesCreateNew `
                $collision.pendingPath $leaf $jsonBytes $script:MaximumBoundEvidenceBytes `
                'collision source snapshot' | Out-Null
        }
        Write-SealedDirectChildBytesCreateNew `
            $collision.pendingPath $script:InjectionReceiptLeaf $receiptBytes `
            $script:MaximumInjectionReceiptBytes 'collision receipt' | Out-Null
        [IO.Directory]::CreateDirectory($collision.finalPath) | Out-Null
        Assert-SelfTestBlocked {
            Publish-InjectionReceiptBundle `
                $collision $leaves $receiptHash ([uint64]$receiptBytes.Length)
        } 'atomic rename collision'
        Remove-OwnedPendingInjectionBundle $collision $leaves
        Remove-Item -LiteralPath $collision.finalPath -Recurse -Force
        $tests++
    } finally {
        if (Test-Path -LiteralPath $receiptTestRoot) {
            Remove-Item -LiteralPath $receiptTestRoot -Recurse -Force
        }
    }
    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

if ($SelfTest) {
    Invoke-SelfTest
    exit 0
}

$expectedExeHash = $ExpectedExecutableSha256.ToLowerInvariant()
$expectedDllHash = $ExpectedProbeDllSha256.ToLowerInvariant()
$expectedInjectorHash = $ExpectedInjectorSha256.ToLowerInvariant()
$expectedConfigHash = $ExpectedArmConfigSha256.ToLowerInvariant()
$expectedSceneAttestationHash = $ExpectedRepresentativeSceneAttestationSha256.ToLowerInvariant()
$exe = (Resolve-Path -LiteralPath $ExpectedExecutablePath).Path
$injector = (Resolve-Path -LiteralPath $InjectorPath).Path
$dll = (Resolve-Path -LiteralPath $ProbeDllPath).Path
foreach ($artifact in @(
    @{ Path = $exe; Label = 'expected executable' },
    @{ Path = $injector; Label = 'depth injector' },
    @{ Path = $dll; Label = 'depth probe DLL' }
)) {
    if (-not (Test-Path -LiteralPath $artifact.Path -PathType Leaf)) {
        throw "$($artifact.Label) is not a file: $($artifact.Path)"
    }
    Assert-NoReparseAncestor -Path $artifact.Path -Label $artifact.Label
}
$requiredInjectorContract = 'dm-depth-inject-guard/v2:start-filetime+loaded-anticheat-module-recheck+single-canonical-stdout'
$actualInjectorHash = 'sha256:' + (Get-FileHash -LiteralPath $injector -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualInjectorHash -cne $expectedInjectorHash) {
    throw 'Depth injector hash does not match the requested guarded binary.'
}
$injectorContract = (& $injector --print-guard-contract 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $injectorContract -cne $requiredInjectorContract) {
    throw 'The selected depth injector does not implement the required exact-instance/protection guard contract.'
}
if (('sha256:' + (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedExeHash) {
    throw 'Expected executable hash does not match the file on disk.'
}
if (('sha256:' + (Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedDllHash) {
    throw 'Expected probe DLL hash does not match the file on disk.'
}

$root = (Resolve-Path -LiteralPath $EvidenceRoot).Path
if (-not (Test-Path -LiteralPath $root -PathType Container)) { throw "EvidenceRoot is not a directory: $EvidenceRoot" }
Assert-NoReparseAncestor -Path $root -Label 'evidence root'
$output = [IO.Path]::GetFullPath((Join-Path $root $OutputLeaf))
$preInjectionEvidence = [IO.Path]::GetFullPath((Join-Path $root $PreInjectionEvidenceLeaf))
if ([IO.Path]::GetDirectoryName($output) -cne $root -or
    [IO.Path]::GetDirectoryName($preInjectionEvidence) -cne $root) {
    throw 'OutputLeaf and PreInjectionEvidenceLeaf must resolve to direct children of EvidenceRoot.'
}
if (Test-Path -LiteralPath $preInjectionEvidence) {
    throw "Pre-injection runtime evidence path must be new: $preInjectionEvidence"
}

$targetScript = Join-Path $PSScriptRoot 'Test-GenericD3D11DsvDiscoveryTargets.ps1'
$targetText = (& $targetScript `
    -GameId $GameId -AppId ([string]$AppId) -BuildId $ExpectedBuildId `
    -ExecutableBasename ([IO.Path]::GetFileName($exe)) `
    -ExecutableSha256 $expectedExeHash | Out-String)
if ($LASTEXITCODE -ne 0) { throw "Generic D3D11 target binding blocked $GameId.`n$targetText" }
$targetReport = $targetText | ConvertFrom-Json
if ($targetReport.status -cne 'pass' -or $null -eq $targetReport.selected_target) {
    throw "Generic D3D11 target binding did not select $GameId."
}
$target = $targetReport.selected_target

$configPath = Get-FixedConfigPath
Assert-ArmedConfigIdentity `
    -Path $configPath -ExpectedDiscoveryId $DiscoveryId -ExpectedExeSha256 $expectedExeHash `
    -ExpectedEvidenceRoot $root -ExpectedOutput $output -ExpectedConfigSha256 $expectedConfigHash `
    -ExpectedRepresentativeSceneAttestationSha256 $expectedSceneAttestationHash

$collector = Join-Path $PSScriptRoot 'Get-GameRuntimeGateEvidence.ps1'
$collectorHash = 'sha256:' + (Get-FileHash -LiteralPath $collector -Algorithm SHA256).Hash.ToLowerInvariant()
$armRuntime = Read-JsonFile -Path $ArmRuntimeEvidencePath -Label 'arm runtime gate evidence'
$armStartTimeUtc = Get-CanonicalUtcTimestamp `
    -Value ([string]$armRuntime.process.start_time_utc) -Label 'arm runtime gate process.start_time_utc'
Assert-RuntimeGateEvidence `
    -Runtime $armRuntime -ExpectedProcessId $ProcessId -ExpectedPath $exe `
    -ExpectedSha256 $expectedExeHash -ExpectedStartTimeUtc $armStartTimeUtc `
    -MaximumAgeSeconds $MaximumArmEvidenceAgeSeconds -ExpectedCollectorSha256 $collectorHash `
    -Label 'arm runtime gate evidence'
Assert-RepresentativeSceneAttestation `
    -Target $target -Path $RepresentativeSceneAttestationPath `
    -ExpectedSha256 $expectedSceneAttestationHash -ExpectedGameId $GameId `
    -ExpectedAppId ([string]$AppId) -ExpectedBuildId $ExpectedBuildId `
    -ExpectedDiscoveryId $DiscoveryId -ExpectedOutputLeaf $OutputLeaf `
    -ExpectedExecutableBasename ([IO.Path]::GetFileName($exe)) `
    -ExpectedExecutableSha256 $expectedExeHash -RuntimeEvidencePath $ArmRuntimeEvidencePath `
    -ExpectedProcessId $ProcessId -ExpectedProcessStartTimeUtc $armStartTimeUtc

# Recollect, do not merely reread, the complete protection gate. The collector owns create-new
# output semantics and will fail closed if any module/service/driver enumeration is incomplete.
$collectorOutput = (& $collector `
    -ProcessId $ProcessId `
    -ExpectedExecutablePath $exe `
    -ExpectedExecutableSha256 $expectedExeHash `
    -ExpectedGraphicsApi d3d11 `
    -RequireNoLoadedAntiCheat `
    -OutputPath $preInjectionEvidence | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "Fresh pre-injection runtime gate blocked; injection was not attempted.`n$collectorOutput"
}

$freshRuntime = Read-JsonFile -Path $preInjectionEvidence -Label 'fresh pre-injection runtime gate evidence'
Assert-RuntimeGateEvidence `
    -Runtime $freshRuntime -ExpectedProcessId $ProcessId -ExpectedPath $exe `
    -ExpectedSha256 $expectedExeHash -ExpectedStartTimeUtc $armStartTimeUtc `
    -MaximumAgeSeconds $MaximumPreInjectionEvidenceAgeSeconds -ExpectedCollectorSha256 $collectorHash `
    -Label 'fresh pre-injection runtime gate evidence'
Assert-RepresentativeSceneAttestation `
    -Target $target -Path $RepresentativeSceneAttestationPath `
    -ExpectedSha256 $expectedSceneAttestationHash -ExpectedGameId $GameId `
    -ExpectedAppId ([string]$AppId) -ExpectedBuildId $ExpectedBuildId `
    -ExpectedDiscoveryId $DiscoveryId -ExpectedOutputLeaf $OutputLeaf `
    -ExpectedExecutableBasename ([IO.Path]::GetFileName($exe)) `
    -ExpectedExecutableSha256 $expectedExeHash -RuntimeEvidencePath $ArmRuntimeEvidencePath `
    -ExpectedProcessId $ProcessId -ExpectedProcessStartTimeUtc $armStartTimeUtc
Assert-ArmedConfigIdentity `
    -Path $configPath -ExpectedDiscoveryId $DiscoveryId -ExpectedExeSha256 $expectedExeHash `
    -ExpectedEvidenceRoot $root -ExpectedOutput $output -ExpectedConfigSha256 $expectedConfigHash `
    -ExpectedRepresentativeSceneAttestationSha256 $expectedSceneAttestationHash
$live = Assert-LiveProcessIdentity `
    -Id $ProcessId -ExpectedPath $exe -ExpectedSha256 $expectedExeHash `
    -ExpectedStartTimeUtc $armStartTimeUtc

# The injector opens the target itself, so pin the kernel creation FILETIME as well as path/hash.
# It repeats this check before and after LoadLibraryW; there is no resident-probe override here.
if (('sha256:' + (Get-FileHash -LiteralPath $injector -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedInjectorHash) {
    throw 'Depth injector bytes changed after guard-contract validation.'
}
Assert-RepresentativeSceneAttestation `
    -Target $target -Path $RepresentativeSceneAttestationPath `
    -ExpectedSha256 $expectedSceneAttestationHash -ExpectedGameId $GameId `
    -ExpectedAppId ([string]$AppId) -ExpectedBuildId $ExpectedBuildId `
    -ExpectedDiscoveryId $DiscoveryId -ExpectedOutputLeaf $OutputLeaf `
    -ExpectedExecutableBasename ([IO.Path]::GetFileName($exe)) `
    -ExpectedExecutableSha256 $expectedExeHash -RuntimeEvidencePath $ArmRuntimeEvidencePath `
    -ExpectedProcessId $ProcessId -ExpectedProcessStartTimeUtc $armStartTimeUtc

# Freeze each bound JSON source from one bounded open. The fixed config handle deliberately
# remains open without write/delete sharing through LoadLibraryW, stdout validation and the
# post-injection process-instance check. The probe output directory remains exclusively probe-owned.
$sceneDocument = Read-StableBoundedJsonDocument `
    -Path $RepresentativeSceneAttestationPath `
    -MaximumBytes $script:MaximumBoundEvidenceBytes `
    -Label 'representative-scene attestation snapshot source'
if ($sceneDocument.sha256 -cne $expectedSceneAttestationHash) {
    throw 'Representative-scene attestation bytes drifted before injection.'
}
$armRuntimeDocument = Read-StableBoundedJsonDocument `
    -Path $ArmRuntimeEvidencePath -MaximumBytes $script:MaximumBoundEvidenceBytes `
    -Label 'arm runtime gate snapshot source'
if ([string]$sceneDocument.value.runtimeEvidence.sha256 -cne $armRuntimeDocument.sha256) {
    throw 'Representative-scene attestation no longer binds the exact arm runtime bytes.'
}
$freshRuntimeDocument = Read-StableBoundedJsonDocument `
    -Path $preInjectionEvidence -MaximumBytes $script:MaximumBoundEvidenceBytes `
    -Label 'pre-injection runtime gate snapshot source'
Assert-RuntimeGateEvidence `
    -Runtime $armRuntimeDocument.value -ExpectedProcessId $ProcessId -ExpectedPath $exe `
    -ExpectedSha256 $expectedExeHash -ExpectedStartTimeUtc $armStartTimeUtc `
    -MaximumAgeSeconds $MaximumArmEvidenceAgeSeconds -ExpectedCollectorSha256 $collectorHash `
    -Label 'frozen arm runtime gate evidence'
Assert-RuntimeGateEvidence `
    -Runtime $freshRuntimeDocument.value -ExpectedProcessId $ProcessId -ExpectedPath $exe `
    -ExpectedSha256 $expectedExeHash -ExpectedStartTimeUtc $armStartTimeUtc `
    -MaximumAgeSeconds $MaximumPreInjectionEvidenceAgeSeconds -ExpectedCollectorSha256 $collectorHash `
    -Label 'frozen pre-injection runtime gate evidence'

$bundleLeaf = $OutputLeaf + '-injection-receipt'
$bundle = $null
$bundlePublished = $false
$configDocument = $null
$snapshotLeaves = @(
    $script:SceneSnapshotLeaf,
    $script:ArmRuntimeSnapshotLeaf,
    $script:PreInjectionRuntimeSnapshotLeaf,
    $script:ArmedConfigSnapshotLeaf,
    $script:InjectionReceiptLeaf
)
try {
    $configDocument = Open-StableBoundedJsonDocument `
        -Path $configPath -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'locked active fixed probe config' -FileShare ([IO.FileShare]::Read)
    if ($configDocument.sha256 -cne $expectedConfigHash) {
        throw 'The locked active fixed probe config bytes differ from the exact armed config.'
    }
    Assert-ArmedConfigDocumentIdentity `
        -Config $configDocument.value -ExpectedDiscoveryId $DiscoveryId `
        -ExpectedExeSha256 $expectedExeHash -ExpectedEvidenceRoot $root `
        -ExpectedOutput $output `
        -ExpectedRepresentativeSceneAttestationSha256 $expectedSceneAttestationHash

    $bundle = New-InjectionBundleStagingDirectory `
        -EvidenceRoot $root -FinalBundleLeaf $bundleLeaf
    $sceneSnapshot = Write-SealedDirectChildBytesCreateNew `
        -EvidenceRoot $bundle.pendingPath -Leaf $script:SceneSnapshotLeaf `
        -Bytes $sceneDocument.bytes -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'representative-scene attestation snapshot'
    $armSnapshot = Write-SealedDirectChildBytesCreateNew `
        -EvidenceRoot $bundle.pendingPath -Leaf $script:ArmRuntimeSnapshotLeaf `
        -Bytes $armRuntimeDocument.bytes -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'arm runtime gate snapshot'
    $freshSnapshot = Write-SealedDirectChildBytesCreateNew `
        -EvidenceRoot $bundle.pendingPath -Leaf $script:PreInjectionRuntimeSnapshotLeaf `
        -Bytes $freshRuntimeDocument.bytes -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'pre-injection runtime gate snapshot'
    $configSnapshot = Write-SealedDirectChildBytesCreateNew `
        -EvidenceRoot $bundle.pendingPath -Leaf $script:ArmedConfigSnapshotLeaf `
        -Bytes $configDocument.bytes -MaximumBytes $script:MaximumBoundEvidenceBytes `
        -Label 'armed config snapshot'

    if (('sha256:' + (Get-FileHash -LiteralPath $injector -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedInjectorHash) {
        throw 'Depth injector bytes changed at the irreversible injection boundary.'
    }
    if (('sha256:' + (Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant()) -cne $expectedDllHash) {
        throw 'Depth probe DLL bytes changed at the irreversible injection boundary.'
    }
    Assert-ReservedOutputAbsent -EvidenceRoot $root -OutputPath $output
    $injectorOutput = (& $injector `
        --pid ([string]$ProcessId) `
        --dll $dll `
        --expected-sha256 $expectedDllHash `
        --expected-exe $exe `
        --expected-exe-sha256 $expectedExeHash `
        --expected-start-time-filetime ([string]$live.start_time_filetime) 2>&1 | Out-String)
    $injectorExitCode = $LASTEXITCODE
    Assert-InjectorSuccessOutput `
        -ExitCode $injectorExitCode -Output $injectorOutput `
        -ExpectedProbeSha256 $expectedDllHash | Out-Null
    $postInjectionLive = Assert-LiveProcessIdentity `
        -Id $ProcessId -ExpectedPath $exe -ExpectedSha256 $expectedExeHash `
        -ExpectedStartTimeUtc $armStartTimeUtc
    if ([uint64]$postInjectionLive.start_time_filetime -ne [uint64]$live.start_time_filetime) {
        throw 'Live process kernel creation FILETIME drifted after guarded module load.'
    }
    Assert-OpenJsonDocumentUnchanged `
        -Document $configDocument -Label 'locked active fixed probe config'

    $receiptLocator = $bundleLeaf + '/' + $script:InjectionReceiptLeaf
    $receipt = [pscustomobject][ordered]@{
        schema = $script:InjectionReceiptSchema
        mode = 'discovery_only'
        status = 'guarded_module_load_succeeded'
        publicationAllowed = $false
        probeInitializationConfirmed = $false
        discoveryStarted = $false
        recordingReady = $false
        deliverableReady = $false
        profileSelected = $false
        profileGenerated = $false
        selectionBindingReady = $false
        bindingGenerated = $false
        receipt = [pscustomobject][ordered]@{
            locator = $receiptLocator
            createdAtUtc = [DateTime]::UtcNow.ToString(
                'o', [Globalization.CultureInfo]::InvariantCulture)
        }
        target = [pscustomobject][ordered]@{
            gameId = $GameId
            steamAppId = [string]$AppId
            steamBuildId = $ExpectedBuildId
            executable = [pscustomobject][ordered]@{
                basename = [IO.Path]::GetFileName($exe)
                sha256 = $expectedExeHash
            }
        }
        run = [pscustomobject][ordered]@{
            discoveryId = $DiscoveryId
            outputLeaf = $OutputLeaf
            requestedOutputLocator = $OutputLeaf
            processId = [uint32]$ProcessId
            processStartTimeUtc = $armStartTimeUtc
            processStartTimeFiletime = [string][uint64]$live.start_time_filetime
        }
        artifacts = [pscustomobject][ordered]@{
            representativeSceneAttestationSnapshot = [pscustomobject][ordered]@{
                locator = $sceneSnapshot.locator
                locatorScope = 'receipt_bundle'
                sha256 = $sceneSnapshot.sha256
                lengthBytes = [uint64]$sceneSnapshot.lengthBytes
                publicationAllowed = $false
            }
            armRuntimeGateSnapshot = [pscustomobject][ordered]@{
                locator = $armSnapshot.locator
                locatorScope = 'receipt_bundle'
                sha256 = $armSnapshot.sha256
                lengthBytes = [uint64]$armSnapshot.lengthBytes
                publicationAllowed = $false
            }
            preInjectionRuntimeGateSnapshot = [pscustomobject][ordered]@{
                locator = $freshSnapshot.locator
                locatorScope = 'receipt_bundle'
                sha256 = $freshSnapshot.sha256
                lengthBytes = [uint64]$freshSnapshot.lengthBytes
                publicationAllowed = $false
            }
            armedConfigSnapshot = [pscustomobject][ordered]@{
                locator = $configSnapshot.locator
                locatorScope = 'receipt_bundle'
                sha256 = $configSnapshot.sha256
                lengthBytes = [uint64]$configSnapshot.lengthBytes
                publicationAllowed = $false
            }
            injector = [pscustomobject][ordered]@{ sha256 = $expectedInjectorHash }
            probe = [pscustomobject][ordered]@{ sha256 = $expectedDllHash }
        }
        injectorOutcome = [pscustomobject][ordered]@{
            exitCode = 0
            loadedProbeSha256 = $expectedDllHash
        }
    }
    $receiptBytes = [Text.UTF8Encoding]::new($false).GetBytes(
        ($receipt | ConvertTo-Json -Depth 12) + "`n")
    $receiptSha256 = Get-Sha256ForBytes -Bytes $receiptBytes
    $stagedReceipt = Write-SealedDirectChildBytesCreateNew `
        -EvidenceRoot $bundle.pendingPath -Leaf $script:InjectionReceiptLeaf `
        -Bytes $receiptBytes -MaximumBytes $script:MaximumInjectionReceiptBytes `
        -Label 'staged injection receipt'
    if ($stagedReceipt.sha256 -cne $receiptSha256) {
        throw 'Staged injection receipt hash differs from the one-shot serialized bytes.'
    }
    $publishedReceipt = Publish-InjectionReceiptBundle `
        -Bundle $bundle -ExpectedLeaves $snapshotLeaves `
        -ExpectedReceiptSha256 $receiptSha256 `
        -ExpectedReceiptLengthBytes ([uint64]$receiptBytes.Length)
    $bundlePublished = $true
} finally {
    if ($null -ne $configDocument) { $configDocument.stream.Dispose() }
    if ($null -ne $bundle -and -not $bundlePublished -and
        (Test-Path -LiteralPath $bundle.pendingPath -PathType Container)) {
        Remove-OwnedPendingInjectionBundle -Bundle $bundle -AllowedLeaves $snapshotLeaves
    }
}

[pscustomobject][ordered]@{
    status = 'guarded_module_load_succeeded'
    mode = 'discovery_only'
    publication_allowed = $false
    probe_initialization_confirmed = $false
    discovery_started = $false
    recording_ready = $false
    deliverable_ready = $false
    profile_selected = $false
    profile_generated = $false
    selection_binding_ready = $false
    binding_generated = $false
    injection_receipt_locator = $publishedReceipt.locator
    injection_receipt_sha256 = $publishedReceipt.sha256
} | ConvertTo-Json -Depth 4

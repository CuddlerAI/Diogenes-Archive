<#
.SYNOPSIS
  Validates one atomically published generic D3D11 guarded-module-load receipt bundle.

.DESCRIPTION
  This validator is deliberately offline. A passing receipt proves only that the guarded injector
  returned the exact loaded-probe hash for the pinned process instance and that the four inputs
  were sealed into the sibling bundle. It does not prove probe initialization, hook installation,
  discovery start, profile selection, recording readiness or deliverability.
#>
[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]{0,255}-injection-receipt$')]
    [string]$BundleLeaf,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedReceiptSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:Schema = 'dm-generic-d3d11-dsv-injection-receipt/v1'
$script:MaximumDocumentBytes = 4MB
$script:MaximumReceiptBytes = 64KB
$script:ReceiptLeaf = 'receipt.json'
$script:SceneLeaf = 'representative-scene-attestation.json'
$script:ArmLeaf = 'arm-runtime-gate.json'
$script:FreshLeaf = 'pre-injection-runtime-gate.json'
$script:ConfigLeaf = 'armed-config.json'
$script:RepresentativeSceneSchema = 'generic-d3d11-representative-scene-attestation/v1'
$script:RepresentativeSceneAttestationKind = 'local_operator'
$script:RepresentativeSceneKind = 'representative_offline_single_player'
$script:ExpectedLeaves = @(
    $script:ReceiptLeaf, $script:SceneLeaf, $script:ArmLeaf, $script:FreshLeaf, $script:ConfigLeaf)

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

function Get-BytesSha256 {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $digest = $hasher.ComputeHash($Bytes) } finally { $hasher.Dispose() }
    return 'sha256:' + [BitConverter]::ToString($digest).Replace('-', '').ToLowerInvariant()
}

function ConvertFrom-JsonPreservingStrings {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)
    if ((Get-Command ConvertFrom-Json).Parameters.ContainsKey('DateKind')) {
        return ($Text | ConvertFrom-Json -DateKind String)
    }
    return ($Text | ConvertFrom-Json)
}

function Read-BoundedJsonOnce {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][uint64]$MaximumBytes,
        [string]$Label = 'JSON document'
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label is missing." }
    $full = (Resolve-Path -LiteralPath $Path).Path
    Assert-NoReparseAncestor -Path $full -Label $Label
    $stream = [IO.File]::Open(
        $full, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ([uint64]$stream.Length -gt $MaximumBytes) { throw "$Label exceeds its byte bound." }
        $length = [int]$stream.Length
        $bytes = [byte[]]::new($length)
        $offset = 0
        while ($offset -lt $length) {
            $read = $stream.Read($bytes, $offset, $length - $offset)
            if ($read -eq 0) { throw "$Label ended before its admitted byte length." }
            $offset += $read
        }
        if ($stream.ReadByte() -ne -1 -or $stream.Length -ne $length) {
            throw "$Label changed while it was read."
        }
    } finally { $stream.Dispose() }
    $after = (Resolve-Path -LiteralPath $Path).Path
    if (-not $after.Equals($full, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label path changed while it was read."
    }
    Assert-NoReparseAncestor -Path $after -Label $Label
    try {
        if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and
            $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
            throw "$Label must be UTF-8 without a BOM."
        }
        $text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
        $value = ConvertFrom-JsonPreservingStrings -Text $text
    } catch { throw "$Label is not strict UTF-8 JSON: $($_.Exception.Message)" }
    return [pscustomobject][ordered]@{
        path = $full; bytes = $bytes; sha256 = Get-BytesSha256 $bytes
        lengthBytes = [uint64]$bytes.Length; value = $value
    }
}

function Assert-ExactProperties {
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string[]]$Names,
        [string]$Label = 'object'
    )
    if ($null -eq $Value -or $Value -isnot [psobject]) { throw "$Label is not an object." }
    $actual = @($Value.PSObject.Properties.Name)
    if ($actual.Count -ne $Names.Count) { throw "$Label has missing or unknown fields." }
    foreach ($name in $Names) {
        if (-not ($actual -ccontains $name)) { throw "$Label is missing exact field $name." }
    }
}

function Assert-CanonicalUtc {
    param([Parameter(Mandatory = $true)][string]$Value, [string]$Label = 'timestamp')
    try {
        $parsed = [DateTimeOffset]::ParseExact(
            $Value, 'o', [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    } catch { throw "$Label is not round-trip ISO-8601." }
    $canonical = $parsed.UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    if ($Value -cne $canonical) { throw "$Label is not canonical UTC with Z suffix." }
    return $canonical
}

function Assert-FalseBoolean {
    param([Parameter(Mandatory = $true)]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [bool] -or $Value) { throw "$Label must be the JSON boolean false." }
}

function Assert-JsonInteger {
    param([Parameter(Mandatory = $true)]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [byte] -and $Value -isnot [sbyte] -and
        $Value -isnot [int16] -and $Value -isnot [uint16] -and
        $Value -isnot [int32] -and $Value -isnot [uint32] -and
        $Value -isnot [int64] -and $Value -isnot [uint64]) {
        throw "$Label must be a JSON integer."
    }
}

function Assert-SnapshotBinding {
    param(
        [Parameter(Mandatory = $true)]$Binding,
        [Parameter(Mandatory = $true)][string]$ExpectedLeaf,
        [Parameter(Mandatory = $true)][string]$BundlePath,
        [Parameter(Mandatory = $true)][string]$Label
    )
    Assert-ExactProperties $Binding @(
        'locator', 'locatorScope', 'sha256', 'lengthBytes', 'publicationAllowed') $Label
    if ([string]$Binding.locator -cne $ExpectedLeaf -or
        [string]$Binding.locatorScope -cne 'receipt_bundle' -or
        [string]$Binding.sha256 -notmatch '^sha256:[0-9a-f]{64}$') {
        throw "$Label locator or hash is not canonical."
    }
    Assert-FalseBoolean $Binding.publicationAllowed "$Label.publicationAllowed"
    Assert-JsonInteger $Binding.lengthBytes "$Label.lengthBytes"
    $reportedLength = [uint64]0
    if (-not [uint64]::TryParse(
        [string]$Binding.lengthBytes, [Globalization.NumberStyles]::None,
        [Globalization.CultureInfo]::InvariantCulture, [ref]$reportedLength)) {
        throw "$Label.lengthBytes is not an unsigned integer."
    }
    $document = Read-BoundedJsonOnce `
        -Path (Join-Path $BundlePath $ExpectedLeaf) `
        -MaximumBytes $script:MaximumDocumentBytes -Label $Label
    if ($document.sha256 -cne [string]$Binding.sha256 -or
        $document.lengthBytes -ne $reportedLength) {
        throw "$Label bytes drifted from the receipt binding."
    }
    return $document
}

function Test-InjectionReceiptBundle {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Leaf,
        [Parameter(Mandatory = $true)][string]$ReceiptSha256
    )
    if ($Leaf -notmatch '^[A-Za-z0-9][A-Za-z0-9_.-]{0,255}-injection-receipt$') {
        throw 'BundleLeaf is unsafe (path traversal, absolute paths and ADS are forbidden).'
    }
    if ($ReceiptSha256 -notmatch '^sha256:[0-9a-f]{64}$') {
        throw 'Expected receipt hash is not canonical.'
    }
    $rootPath = (Resolve-Path -LiteralPath $Root).Path
    Assert-NoReparseAncestor $rootPath 'evidence root'
    $bundlePath = [IO.Path]::GetFullPath((Join-Path $rootPath $Leaf))
    if (-not ([IO.Path]::GetDirectoryName($bundlePath)).Equals(
        $rootPath, [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $bundlePath -PathType Container)) {
        throw 'Receipt bundle is not an existing direct child of EvidenceRoot.'
    }
    $bundlePath = (Resolve-Path -LiteralPath $bundlePath).Path
    Assert-NoReparseAncestor $bundlePath 'injection receipt bundle'
    $entries = @(Get-ChildItem -LiteralPath $bundlePath -Force)
    if ($entries.Count -ne $script:ExpectedLeaves.Count) {
        throw 'Receipt bundle is partial or contains unexpected entries.'
    }
    foreach ($entry in $entries) {
        if ($entry.PSIsContainer -or
            ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            -not ($script:ExpectedLeaves -ccontains $entry.Name)) {
            throw 'Receipt bundle contains an unexpected, case-drifted or reparse entry.'
        }
    }

    $receiptDocument = Read-BoundedJsonOnce `
        -Path (Join-Path $bundlePath $script:ReceiptLeaf) `
        -MaximumBytes $script:MaximumReceiptBytes -Label 'injection receipt'
    if ($receiptDocument.sha256 -cne $ReceiptSha256) { throw 'Injection receipt hash drifted.' }
    $r = $receiptDocument.value
    Assert-ExactProperties $r @(
        'schema', 'mode', 'status', 'publicationAllowed', 'probeInitializationConfirmed',
        'discoveryStarted', 'recordingReady', 'deliverableReady', 'profileSelected',
        'profileGenerated', 'selectionBindingReady', 'bindingGenerated',
        'receipt', 'target', 'run', 'artifacts', 'injectorOutcome') 'receipt root'
    if ($r.schema -cne $script:Schema -or $r.mode -cne 'discovery_only' -or
        $r.status -cne 'guarded_module_load_succeeded') {
        throw 'Receipt schema or guarded-module-load status is invalid.'
    }
    foreach ($field in @(
        'publicationAllowed', 'probeInitializationConfirmed', 'discoveryStarted',
        'recordingReady', 'deliverableReady', 'profileSelected', 'profileGenerated',
        'selectionBindingReady', 'bindingGenerated')) {
        Assert-FalseBoolean $r.$field "receipt.$field"
    }
    Assert-ExactProperties $r.receipt @('locator', 'createdAtUtc') 'receipt.receipt'
    $expectedLocator = $Leaf + '/' + $script:ReceiptLeaf
    if ([string]$r.receipt.locator -cne $expectedLocator) { throw 'Receipt locator is not canonical.' }
    Assert-CanonicalUtc ([string]$r.receipt.createdAtUtc) 'receipt.createdAtUtc' | Out-Null

    Assert-ExactProperties $r.target @('gameId', 'steamAppId', 'steamBuildId', 'executable') 'receipt.target'
    Assert-ExactProperties $r.target.executable @('basename', 'sha256') 'receipt.target.executable'
    if ([string]$r.target.gameId -notmatch '^[a-z0-9][a-z0-9-]{0,127}$' -or
        [string]$r.target.steamAppId -notmatch '^[1-9][0-9]*$' -or
        [string]$r.target.steamBuildId -notmatch '^[0-9]+$' -or
        [string]$r.target.executable.basename -notmatch '^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\.exe$' -or
        [string]$r.target.executable.sha256 -notmatch '^sha256:[0-9a-f]{64}$') {
        throw 'Receipt target identity is not canonical.'
    }

    Assert-ExactProperties $r.run @(
        'discoveryId', 'outputLeaf', 'requestedOutputLocator', 'processId',
        'processStartTimeUtc', 'processStartTimeFiletime') 'receipt.run'
    if ([string]$r.run.discoveryId -notmatch '^[A-Za-z0-9_-]{1,128}$' -or
        [string]$r.run.outputLeaf -notmatch '^[A-Za-z0-9_-]{1,128}$' -or
        [string]$r.run.requestedOutputLocator -cne [string]$r.run.outputLeaf -or
        $Leaf -cne ([string]$r.run.outputLeaf + '-injection-receipt')) {
        throw 'Receipt run locator identity is invalid.'
    }
    $pidValue = [uint64]0
    $filetimeValue = [uint64]0
    Assert-JsonInteger $r.run.processId 'receipt.run.processId'
    if (-not [uint64]::TryParse([string]$r.run.processId, [ref]$pidValue) -or
        $pidValue -eq 0 -or $pidValue -gt [uint32]::MaxValue -or
        -not [uint64]::TryParse([string]$r.run.processStartTimeFiletime, [ref]$filetimeValue)) {
        throw 'Receipt process identity is invalid.'
    }
    $startUtc = Assert-CanonicalUtc ([string]$r.run.processStartTimeUtc) 'receipt.run.processStartTimeUtc'
    if ([uint64]([DateTime]::ParseExact(
        $startUtc, 'o', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind).ToFileTimeUtc()) -ne $filetimeValue) {
        throw 'Receipt process FILETIME does not match processStartTimeUtc.'
    }

    Assert-ExactProperties $r.artifacts @(
        'representativeSceneAttestationSnapshot', 'armRuntimeGateSnapshot',
        'preInjectionRuntimeGateSnapshot', 'armedConfigSnapshot', 'injector', 'probe') 'receipt.artifacts'
    $scene = Assert-SnapshotBinding $r.artifacts.representativeSceneAttestationSnapshot `
        $script:SceneLeaf $bundlePath 'representative-scene snapshot'
    $arm = Assert-SnapshotBinding $r.artifacts.armRuntimeGateSnapshot `
        $script:ArmLeaf $bundlePath 'arm runtime snapshot'
    $fresh = Assert-SnapshotBinding $r.artifacts.preInjectionRuntimeGateSnapshot `
        $script:FreshLeaf $bundlePath 'pre-injection runtime snapshot'
    $config = Assert-SnapshotBinding $r.artifacts.armedConfigSnapshot `
        $script:ConfigLeaf $bundlePath 'armed config snapshot'
    Assert-ExactProperties $r.artifacts.injector @('sha256') 'receipt.artifacts.injector'
    Assert-ExactProperties $r.artifacts.probe @('sha256') 'receipt.artifacts.probe'
    if ([string]$r.artifacts.injector.sha256 -notmatch '^sha256:[0-9a-f]{64}$' -or
        [string]$r.artifacts.probe.sha256 -notmatch '^sha256:[0-9a-f]{64}$') {
        throw 'Receipt binary hashes are not canonical.'
    }
    Assert-ExactProperties $r.injectorOutcome @('exitCode', 'loadedProbeSha256') 'receipt.injectorOutcome'
    Assert-JsonInteger $r.injectorOutcome.exitCode 'receipt.injectorOutcome.exitCode'
    if ([int]$r.injectorOutcome.exitCode -ne 0 -or
        [string]$r.injectorOutcome.loadedProbeSha256 -cne [string]$r.artifacts.probe.sha256) {
        throw 'Receipt injector outcome does not bind the selected probe.'
    }

    $sceneValue = $scene.value
    Assert-ExactProperties $sceneValue @(
        'schema', 'attestationKind', 'gameId', 'steamAppId', 'steamBuildId',
        'discoveryId', 'outputLeaf', 'executable', 'runtimeEvidence', 'window',
        'sceneKind', 'observedAtUtc', 'nonce') 'representative-scene snapshot root'
    Assert-ExactProperties $sceneValue.executable @('basename', 'sha256') `
        'representative-scene snapshot executable'
    Assert-ExactProperties $sceneValue.runtimeEvidence @(
        'sha256', 'processId', 'processStartTimeUtc') `
        'representative-scene snapshot runtimeEvidence'
    Assert-ExactProperties $sceneValue.window @(
        'className', 'clientWidth', 'clientHeight') `
        'representative-scene snapshot window'
    Assert-JsonInteger $sceneValue.runtimeEvidence.processId `
        'representative-scene snapshot runtimeEvidence.processId'
    Assert-JsonInteger $sceneValue.window.clientWidth `
        'representative-scene snapshot window.clientWidth'
    Assert-JsonInteger $sceneValue.window.clientHeight `
        'representative-scene snapshot window.clientHeight'
    if ($sceneValue.window.className -isnot [string] -or
        [string]$sceneValue.window.className -notmatch '^.{1,256}$' -or
        [uint64]$sceneValue.window.clientWidth -lt 1 -or
        [uint64]$sceneValue.window.clientWidth -gt 16384 -or
        [uint64]$sceneValue.window.clientHeight -lt 1 -or
        [uint64]$sceneValue.window.clientHeight -gt 16384 -or
        [string]$sceneValue.nonce -cnotmatch '^[0-9a-f]{64}$') {
        throw 'Representative-scene snapshot window or nonce wire is invalid.'
    }
    Assert-CanonicalUtc ([string]$sceneValue.observedAtUtc) `
        'representative-scene snapshot observedAtUtc' | Out-Null
    if ($sceneValue.schema -cne $script:RepresentativeSceneSchema -or
        $sceneValue.attestationKind -cne $script:RepresentativeSceneAttestationKind -or
        $sceneValue.sceneKind -cne $script:RepresentativeSceneKind -or
        [string]$sceneValue.gameId -cne [string]$r.target.gameId -or
        [string]$sceneValue.steamAppId -cne [string]$r.target.steamAppId -or
        [string]$sceneValue.steamBuildId -cne [string]$r.target.steamBuildId -or
        [string]$sceneValue.discoveryId -cne [string]$r.run.discoveryId -or
        [string]$sceneValue.outputLeaf -cne [string]$r.run.outputLeaf -or
        [string]$sceneValue.executable.basename -cne [string]$r.target.executable.basename -or
        [string]$sceneValue.executable.sha256 -cne [string]$r.target.executable.sha256 -or
        [string]$sceneValue.runtimeEvidence.sha256 -cne $arm.sha256 -or
        [uint64]$sceneValue.runtimeEvidence.processId -ne $pidValue -or
        [string]$sceneValue.runtimeEvidence.processStartTimeUtc -cne $startUtc) {
        throw 'Representative-scene snapshot does not bind the receipt target/run/arm evidence.'
    }
    foreach ($runtime in @($arm.value, $fresh.value)) {
        if ($runtime.schema -cne 'game-runtime-gate-evidence/v1' -or $runtime.status -cne 'pass' -or
            [uint64]$runtime.process.process_id -ne $pidValue -or
            [string]$runtime.process.executable_sha256 -cne [string]$r.target.executable.sha256 -or
            [string]$runtime.process.start_time_utc -cne $startUtc -or
            [IO.Path]::GetFileName([string]$runtime.process.executable_path) -cne
                [string]$r.target.executable.basename) {
            throw 'Runtime snapshot does not bind the receipt process identity.'
        }
    }
    $configuredOutput = [IO.Path]::GetFullPath([string]$config.value.output_dir)
    if (-not $config.value.enabled -or
        $config.value.session_id -cne ('dsv-discovery-' + [string]$r.run.discoveryId) -or
        $config.value.dsv_discovery.schema -cne 'd3d11-dsv-discovery-config/v2' -or
        $config.value.dsv_discovery.discovery_id -cne [string]$r.run.discoveryId -or
        $config.value.dsv_discovery.executable_sha256 -cne [string]$r.target.executable.sha256 -or
        $config.value.representative_scene_attestation_sha256 -cne $scene.sha256 -or
        -not ([IO.Path]::GetFullPath([string]$config.value.dsv_discovery.evidence_root)).Equals(
            $rootPath, [StringComparison]::OrdinalIgnoreCase) -or
        -not ([IO.Path]::GetDirectoryName($configuredOutput)).Equals(
            $rootPath, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($configuredOutput) -cne [string]$r.run.outputLeaf) {
        throw 'Armed config snapshot does not bind the receipt discovery identity.'
    }

    return [pscustomobject][ordered]@{
        status = 'pass'
        schema = $script:Schema
        receipt_sha256 = $receiptDocument.sha256
        guarded_module_load_succeeded = $true
        probe_initialization_confirmed = $false
        discovery_started = $false
        recording_ready = $false
        deliverable_ready = $false
    }
}

function Write-TestJson {
    param([string]$Path, $Value)
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Depth 20) + "`n")
    [IO.File]::WriteAllBytes($Path, $bytes)
    return [pscustomobject]@{ sha256 = Get-BytesSha256 $bytes; lengthBytes = [uint64]$bytes.Length }
}

function New-TestFixture {
    param([string]$Root, [string]$OutputLeaf = 'fixture-output')
    $bundleLeaf = $OutputLeaf + '-injection-receipt'
    $bundle = Join-Path $Root $bundleLeaf
    [IO.Directory]::CreateDirectory($bundle) | Out-Null
    $start = [DateTime]::UtcNow.AddSeconds(-1).ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    $exeHash = 'sha256:' + ('a' * 64)
    $armValue = [ordered]@{
        schema = 'game-runtime-gate-evidence/v1'; status = 'pass'
        process = [ordered]@{ process_id = 42; executable_path = 'C:\Games\Fixture.exe'; executable_sha256 = $exeHash; start_time_utc = $start }
    }
    $arm = Write-TestJson (Join-Path $bundle $script:ArmLeaf) $armValue
    $fresh = Write-TestJson (Join-Path $bundle $script:FreshLeaf) $armValue
    $sceneValue = [ordered]@{
        schema = $script:RepresentativeSceneSchema
        attestationKind = $script:RepresentativeSceneAttestationKind
        gameId = 'fixture'; steamAppId = '1'; steamBuildId = '2'; discoveryId = 'fixture'
        outputLeaf = $OutputLeaf; executable = [ordered]@{ basename = 'Fixture.exe'; sha256 = $exeHash }
        runtimeEvidence = [ordered]@{ sha256 = $arm.sha256; processId = 42; processStartTimeUtc = $start }
        window = [ordered]@{ className = 'FixtureWindow'; clientWidth = 1920; clientHeight = 1080 }
        sceneKind = $script:RepresentativeSceneKind
        observedAtUtc = [DateTime]::UtcNow.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
        nonce = 'd' * 64
    }
    $scene = Write-TestJson (Join-Path $bundle $script:SceneLeaf) $sceneValue
    $configValue = [ordered]@{
        enabled = $true; output_dir = (Join-Path $Root $OutputLeaf)
        session_id = 'dsv-discovery-fixture'; representative_scene_attestation_sha256 = $scene.sha256
        dsv_discovery = [ordered]@{
            schema = 'd3d11-dsv-discovery-config/v2'; discovery_id = 'fixture'
            executable_sha256 = $exeHash; evidence_root = $Root
        }
    }
    $config = Write-TestJson (Join-Path $bundle $script:ConfigLeaf) $configValue
    $binding = {
        param($leaf, $document)
        [ordered]@{
            locator = $leaf; locatorScope = 'receipt_bundle'; sha256 = $document.sha256
            lengthBytes = $document.lengthBytes; publicationAllowed = $false
        }
    }
    $receiptValue = [ordered]@{
        schema = $script:Schema; mode = 'discovery_only'; status = 'guarded_module_load_succeeded'
        publicationAllowed = $false
        probeInitializationConfirmed = $false; discoveryStarted = $false
        recordingReady = $false; deliverableReady = $false
        profileSelected = $false; profileGenerated = $false
        selectionBindingReady = $false; bindingGenerated = $false
        receipt = [ordered]@{ locator = "$bundleLeaf/receipt.json"; createdAtUtc = [DateTime]::UtcNow.ToString('o') }
        target = [ordered]@{
            gameId = 'fixture'; steamAppId = '1'; steamBuildId = '2'
            executable = [ordered]@{ basename = 'Fixture.exe'; sha256 = $exeHash }
        }
        run = [ordered]@{
            discoveryId = 'fixture'; outputLeaf = $OutputLeaf; requestedOutputLocator = $OutputLeaf
            processId = 42; processStartTimeUtc = $start
            processStartTimeFiletime = [string][uint64]([DateTime]::Parse($start).ToFileTimeUtc())
        }
        artifacts = [ordered]@{
            representativeSceneAttestationSnapshot = & $binding $script:SceneLeaf $scene
            armRuntimeGateSnapshot = & $binding $script:ArmLeaf $arm
            preInjectionRuntimeGateSnapshot = & $binding $script:FreshLeaf $fresh
            armedConfigSnapshot = & $binding $script:ConfigLeaf $config
            injector = [ordered]@{ sha256 = 'sha256:' + ('b' * 64) }
            probe = [ordered]@{ sha256 = 'sha256:' + ('c' * 64) }
        }
        injectorOutcome = [ordered]@{ exitCode = 0; loadedProbeSha256 = 'sha256:' + ('c' * 64) }
    }
    $receipt = Write-TestJson (Join-Path $bundle $script:ReceiptLeaf) $receiptValue
    return [pscustomobject]@{
        root = $Root; bundleLeaf = $bundleLeaf; bundle = $bundle
        receiptPath = Join-Path $bundle $script:ReceiptLeaf
        scenePath = Join-Path $bundle $script:SceneLeaf
        receiptSha256 = $receipt.sha256
    }
}

function Assert-TestBlocked {
    param([scriptblock]$Action, [string]$Name)
    $blocked = $false
    try { & $Action | Out-Null } catch { $blocked = $true }
    if (-not $blocked) { throw "self-test negative case passed unexpectedly: $Name" }
}

function Invoke-SelfTest {
    $testRoot = Join-Path ([IO.Path]::GetTempPath()) ('dm-injection-receipt-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($testRoot) | Out-Null
    $tests = 0
    try {
        $fixture = New-TestFixture $testRoot
        Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $fixture.receiptSha256 | Out-Null
        $tests++

        # The positive fixture above is the frozen wire emitted by
        # New-GenericD3D11RepresentativeSceneAttestation.ps1: local_operator, exact window
        # fields/types, canonical timestamps and a 32-byte lowercase-hex nonce. Rebind the
        # receipt after each mutation so these negatives exercise scene semantics, not hash drift.
        $sceneValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.scenePath -Raw -Encoding UTF8)
        $sceneValue.attestationKind = 'operator_observed'
        $changedScene = Write-TestJson $fixture.scenePath $sceneValue
        $receiptValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.receiptPath -Raw -Encoding UTF8)
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.sha256 = $changedScene.sha256
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.lengthBytes = $changedScene.lengthBytes
        $changedReceipt = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked {
            Test-InjectionReceiptBundle `
                $fixture.root $fixture.bundleLeaf $changedReceipt.sha256
        } 'representative-scene attestationKind drift'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $fixture = New-TestFixture $testRoot
        $sceneValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.scenePath -Raw -Encoding UTF8)
        $sceneValue.window.clientWidth = '1920'
        $changedScene = Write-TestJson $fixture.scenePath $sceneValue
        $receiptValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.receiptPath -Raw -Encoding UTF8)
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.sha256 = $changedScene.sha256
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.lengthBytes = $changedScene.lengthBytes
        $changedReceipt = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked {
            Test-InjectionReceiptBundle `
                $fixture.root $fixture.bundleLeaf $changedReceipt.sha256
        } 'representative-scene window field type drift'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $fixture = New-TestFixture $testRoot
        [IO.File]::AppendAllText((Join-Path $fixture.bundle $script:ArmLeaf), " ")
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $fixture.receiptSha256 } 'source hash drift'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $fixture = New-TestFixture $testRoot
        $receiptValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.receiptPath -Raw -Encoding UTF8)
        $receiptValue.target.gameId = 'drifted'
        $changed = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $changed.sha256 } 'receipt field drift'
        $tests++
        $receiptValue.target.gameId = 'fixture'
        $receiptValue | Add-Member -NotePropertyName unexpected -NotePropertyValue $true
        $changed = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $changed.sha256 } 'unknown receipt field'
        $tests++
        $receiptValue.PSObject.Properties.Remove('unexpected')
        $receiptValue.artifacts.armRuntimeGateSnapshot.locator = '../arm-runtime-gate.json'
        $changed = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $changed.sha256 } 'path escape locator'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $fixture = New-TestFixture $testRoot
        $scenePath = Join-Path $fixture.bundle $script:SceneLeaf
        $plainScene = [IO.File]::ReadAllBytes($scenePath)
        $bomScene = [byte[]]::new($plainScene.Length + 3)
        $bomScene[0] = 0xEF; $bomScene[1] = 0xBB; $bomScene[2] = 0xBF
        [Array]::Copy($plainScene, 0, $bomScene, 3, $plainScene.Length)
        [IO.File]::WriteAllBytes($scenePath, $bomScene)
        $receiptValue = ConvertFrom-JsonPreservingStrings `
            -Text (Get-Content -LiteralPath $fixture.receiptPath -Raw -Encoding UTF8)
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.sha256 = Get-BytesSha256 $bomScene
        $receiptValue.artifacts.representativeSceneAttestationSnapshot.lengthBytes = [uint64]$bomScene.Length
        $changed = Write-TestJson $fixture.receiptPath $receiptValue
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $changed.sha256 } 'UTF-8 BOM snapshot'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        foreach ($badLeaf in @('..\escape-injection-receipt', 'C:\absolute-injection-receipt', 'name:ads-injection-receipt')) {
            Assert-TestBlocked { Test-InjectionReceiptBundle $testRoot $badLeaf ('sha256:' + ('0' * 64)) } "unsafe bundle leaf $badLeaf"
            $tests++
        }

        $fixture = New-TestFixture $testRoot
        Remove-Item -LiteralPath (Join-Path $fixture.bundle $script:ConfigLeaf) -Force
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $fixture.receiptSha256 } 'partial bundle'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $fixture = New-TestFixture $testRoot
        Rename-Item -LiteralPath (Join-Path $fixture.bundle $script:ReceiptLeaf) -NewName 'receipt.tmp'
        Rename-Item -LiteralPath (Join-Path $fixture.bundle 'receipt.tmp') -NewName 'Receipt.json'
        Assert-TestBlocked { Test-InjectionReceiptBundle $fixture.root $fixture.bundleLeaf $fixture.receiptSha256 } 'case drift'
        $tests++
        Remove-Item -LiteralPath $fixture.bundle -Recurse -Force

        $outside = Join-Path ([IO.Path]::GetTempPath()) ('dm-injection-receipt-outside-' + [Guid]::NewGuid().ToString('N'))
        [IO.Directory]::CreateDirectory($outside) | Out-Null
        try {
            $junctionLeaf = 'junction-injection-receipt'
            $junction = Join-Path $testRoot $junctionLeaf
            New-Item -ItemType Junction -Path $junction -Target $outside | Out-Null
            Assert-TestBlocked { Test-InjectionReceiptBundle $testRoot $junctionLeaf ('sha256:' + ('0' * 64)) } 'reparse bundle'
            $tests++
            Remove-Item -LiteralPath $junction -Force
        } finally {
            if (Test-Path -LiteralPath $outside) { Remove-Item -LiteralPath $outside -Recurse -Force }
        }
    } finally {
        if (Test-Path -LiteralPath $testRoot) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
    }
    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

if ($SelfTest) {
    Invoke-SelfTest
    exit 0
}

Test-InjectionReceiptBundle `
    -Root $EvidenceRoot -Leaf $BundleLeaf -ReceiptSha256 $ExpectedReceiptSha256 |
    ConvertTo-Json -Depth 4

[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(ParameterSetName = 'Validate')]
    [string]$TargetManifestPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$ShippedGamesPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$RightsPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$CapabilitiesPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$FinalClassificationsPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [string]$EligibilityAuditPath = '',

    [Parameter(ParameterSetName = 'Validate')]
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,127}$')]
    [string]$GameId = '',

    [Parameter(ParameterSetName = 'Validate')]
    [ValidatePattern('^\d+$')]
    [string]$AppId = '',

    [Parameter(ParameterSetName = 'Validate')]
    [ValidatePattern('^\d+$')]
    [string]$BuildId = '',

    [Parameter(ParameterSetName = 'Validate')]
    [ValidatePattern('^[^\\/:*?"<>|]+\.exe$')]
    [string]$ExecutableBasename = '',

    [Parameter(ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-fA-F]{64}$')]
    [string]$ExecutableSha256 = '',

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:TargetSchema = 'dm-generic-d3d11-dsv-discovery-targets-2'
$script:CapabilitySchema = 'dm-game-depth-capabilities-3'
$script:ShippedSchema = 'dm-shipped-games-1'
$script:RightsSchema = 2
$script:FinalSchema = 1
$script:EligibilitySchema = 'dm-generic-d3d11-dsv-discovery-eligibility-1'
$script:EligibilityBlockers = @(
    'renderer_evidence_excludes_d3d11',
    'renderer_backend_unknown',
    'exact_build_renderer_runtime_missing',
    'exact_build_identity_missing',
    'exact_build_id_binding_missing',
    'representative_window_binding_missing',
    'final_classification_gate_unmodeled',
    'formal_depth_path_already_present',
    'current_launch_license_missing',
    'target_architecture_binding_missing',
    'official_protection_disabled_launch_binding_missing',
    'clean_official_install_surface_missing',
    'catalog_final_executable_identity_conflict',
    'existing_game_specific_path_preferred'
)
$script:EligibilityEvidencePurposes = @(
    'existing_game_specific_path',
    'target_architecture',
    'formal_depth_profile',
    'formal_camera_binding',
    'current_launch_license',
    'official_protection_disabled_launch'
)

function Get-Field {
    param([AllowNull()]$Object, [Parameter(Mandatory = $true)][string]$Name)
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Get-ArrayField {
    param([AllowNull()]$Object, [Parameter(Mandatory = $true)][string]$Name)
    if ($null -eq $Object) { throw "$Name owner is null." }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or $property.Value -isnot [Array]) {
        throw "$Name must be an array."
    }
    return , $property.Value
}

function Assert-ExactProperties {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string[]]$Required,
        [string[]]$Optional = @(),
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($null -eq $Object) { throw "$Label must be an object." }
    $names = @($Object.PSObject.Properties.Name)
    foreach ($name in $Required) {
        if ($names -cnotcontains $name) { throw "$Label is missing required field '$name'." }
    }
    $allowed = @($Required + $Optional)
    foreach ($name in $names) {
        if ($allowed -cnotcontains $name) { throw "$Label contains unknown field '$name'." }
    }
}

function Assert-NonBlankString {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace($Value)) {
        throw "$Label must be a non-empty string."
    }
}

function Assert-Boolean {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [bool]) { throw "$Label must be boolean." }
}

function Assert-IntegerRange {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory = $true)][int64]$Minimum,
        [Parameter(Mandatory = $true)][int64]$Maximum,
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($Value -isnot [sbyte] -and $Value -isnot [byte] -and
        $Value -isnot [int16] -and $Value -isnot [uint16] -and
        $Value -isnot [int32] -and $Value -isnot [uint32] -and
        $Value -isnot [int64] -and $Value -isnot [uint64]) {
        throw "$Label must be an integer."
    }
    if ([int64]$Value -lt $Minimum -or [int64]$Value -gt $Maximum) {
        throw "$Label is outside the admitted bound."
    }
}

function Assert-UniqueStringArray {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [Array] -or $Value.Count -eq 0) { throw "$Label must be a non-empty array." }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($item in $Value) {
        Assert-NonBlankString $item "$Label entry"
        if (-not $seen.Add([string]$item)) { throw "$Label contains duplicate '$item'." }
    }
}

function Assert-ExactStringSet {
    param(
        [AllowNull()]$Actual,
        [AllowNull()]$Expected,
        [Parameter(Mandatory = $true)][string]$Label
    )
    Assert-UniqueStringArray $Actual "$Label actual"
    Assert-UniqueStringArray $Expected "$Label expected"
    $actualSorted = @($Actual | Sort-Object)
    $expectedSorted = @($Expected | Sort-Object)
    if (($actualSorted -join "`u{1f}") -cne ($expectedSorted -join "`u{1f}")) {
        throw "$Label does not match the discovery target binding."
    }
}

function Assert-EmptyArray {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    if ($Value -isnot [Array] -or $Value.Count -ne 0) { throw "$Label must be an empty array." }
}

function Assert-RequiredBlocker {
    param(
        [Parameter(Mandatory = $true)][string[]]$Blockers,
        [Parameter(Mandatory = $true)][string]$Blocker,
        [Parameter(Mandatory = $true)][bool]$Required,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $present = $Blockers -ccontains $Blocker
    if ($Required -and -not $present) { throw "$Label must include blocker '$Blocker'." }
    if (-not $Required -and $present) { throw "$Label contains stale blocker '$Blocker'." }
}

function Test-IsReparsePoint {
    param([Parameter(Mandatory = $true)][IO.FileSystemInfo]$Item)
    return (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)
}

function Test-ApprovedRightsScopes {
    param(
        [Parameter(Mandatory = $true)]$RightsRecord,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $rightsScopes = Get-Field $RightsRecord 'scopes'
    foreach ($scopeName in @('instrumentation', 'capture', 'commercial_ml', 'delivery')) {
        $scope = Get-Field $rightsScopes $scopeName
        if ($null -eq $scope -or (Get-Field $scope 'approved') -ne $true -or
            [string]::IsNullOrWhiteSpace([string](Get-Field $scope 'boundaries'))) {
            throw "$Label rights scope '$scopeName' is not explicitly bounded and approved."
        }
    }
}

function Get-EvidenceSha256 {
    param([Parameter(Mandatory = $true)][IO.FileInfo]$File)
    if ($File.Extension -in @('.md', '.txt')) {
        $text = [IO.File]::ReadAllText($File.FullName, [Text.UTF8Encoding]::new($false))
        $canonical = $text.Replace("`r`n", "`n").Replace("`r", "`n")
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($canonical)
        $algorithm = [Security.Cryptography.SHA256]::Create()
        try {
            return 'sha256:' + ([BitConverter]::ToString($algorithm.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
        } finally {
            $algorithm.Dispose()
        }
    }
    return 'sha256:' + (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Test-EligibilityEvidenceBindings {
    param(
        [Parameter(Mandatory = $true)]$AuditRow,
        [Parameter(Mandatory = $true)]$Capability,
        [Parameter(Mandatory = $true)][string]$DataMinerRoot,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $bindings = Get-ArrayField $AuditRow 'evidenceBindings'
    $purposeCounts = @{}
    foreach ($purpose in $script:EligibilityEvidencePurposes) { $purposeCounts[$purpose] = 0 }
    $rootFull = [IO.Path]::GetFullPath($DataMinerRoot).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar

    for ($index = 0; $index -lt $bindings.Count; $index++) {
        $binding = $bindings[$index]
        $bindingLabel = "$Label.evidenceBindings[$index]"
        Assert-ExactProperties $binding @('purposes', 'path', 'sha256') @() $bindingLabel
        $purposes = Get-ArrayField $binding 'purposes'
        Assert-UniqueStringArray $purposes "$bindingLabel.purposes"
        foreach ($purpose in $purposes) {
            if ($script:EligibilityEvidencePurposes -cnotcontains $purpose) {
                throw "$bindingLabel contains unsupported purpose '$purpose'."
            }
            $purposeCounts[$purpose]++
        }
        Assert-NonBlankString $binding.path "$bindingLabel.path"
        if ([IO.Path]::IsPathRooted([string]$binding.path) -or [string]$binding.path -match '(^|[\\/])\.\.([\\/]|$)') {
            throw "$bindingLabel.path must remain relative to DataMiner without traversal."
        }
        Assert-NonBlankString $binding.sha256 "$bindingLabel.sha256"
        if ([string]$binding.sha256 -cnotmatch '^sha256:[0-9a-f]{64}$') {
            throw "$bindingLabel.sha256 must be canonical lowercase SHA-256."
        }

        $candidate = [IO.Path]::GetFullPath((Join-Path $DataMinerRoot ([string]$binding.path)))
        if (-not $candidate.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
            throw "$bindingLabel.path escapes the DataMiner root."
        }
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "$bindingLabel.path does not name a repository evidence file."
        }
        $item = Get-Item -LiteralPath $candidate -Force
        if (Test-IsReparsePoint $item) {
            throw "$bindingLabel.path and its repository ancestors must not be reparse points."
        }
        $currentItem = $item.Directory
        $rootWithoutSeparator = $rootFull.TrimEnd([IO.Path]::DirectorySeparatorChar)
        while ($null -ne $currentItem) {
            if (Test-IsReparsePoint $currentItem) {
                throw "$bindingLabel.path and its repository ancestors must not be reparse points."
            }
            if ($currentItem.FullName.TrimEnd([IO.Path]::DirectorySeparatorChar).Equals(
                    $rootWithoutSeparator, [StringComparison]::OrdinalIgnoreCase)) {
                break
            }
            $currentItem = $currentItem.Parent
        }
        $actualHash = Get-EvidenceSha256 $item
        if ($actualHash -cne [string]$binding.sha256) { throw "$bindingLabel evidence bytes drifted." }

        if ($purposes -ccontains 'existing_game_specific_path') {
            if ($item.Extension -cne '.json') { throw "$bindingLabel game-specific path evidence must be JSON." }
            $evidenceText = Get-Content -LiteralPath $candidate -Raw -Encoding UTF8
            $identity = Get-Field (Get-Field $Capability 'rendererEvidence') 'exactBuildIdentity'
            $exeHash = [string](Get-Field $identity 'executableSha256')
            if ([string]::IsNullOrWhiteSpace($exeHash)) {
                throw "$bindingLabel cannot bind a game-specific path without capability executable identity."
            }
            $acceptableHashHex = @($exeHash.Substring('sha256:'.Length))
            if ((Get-ArrayField $Capability 'blockers') -ccontains 'catalog_final_executable_identity_conflict') {
                $catalogGameProfilePath = Join-Path $DataMinerRoot "config/catalogs/dev/$($AuditRow.gameId)/game-profile.json"
                if (Test-Path -LiteralPath $catalogGameProfilePath -PathType Leaf) {
                    $catalogGameProfile = Get-Content -LiteralPath $catalogGameProfilePath -Raw -Encoding UTF8 | ConvertFrom-Json
                    $catalogHash = [string](Get-Field $catalogGameProfile 'executable_sha256')
                    if ($catalogHash -cmatch '^sha256:[0-9a-f]{64}$') {
                        $acceptableHashHex += $catalogHash.Substring('sha256:'.Length)
                    }
                }
            }
            $hasBoundHash = @($acceptableHashHex | Where-Object {
                    $evidenceText.IndexOf($_, [StringComparison]::OrdinalIgnoreCase) -ge 0
                }).Count -gt 0
            if (-not $hasBoundHash -or
                $evidenceText.IndexOf('depth', [StringComparison]::OrdinalIgnoreCase) -lt 0) {
                throw "$bindingLabel does not bind the exact executable and a Depth-specific result."
            }
        }
    }

    $blockers = @($AuditRow.blockers)
    $purposeRequirements = @{
        existing_game_specific_path = 'existing_game_specific_path_preferred'
        target_architecture = 'target_architecture_binding_missing'
        current_launch_license = 'current_launch_license_missing'
        official_protection_disabled_launch = 'official_protection_disabled_launch_binding_missing'
    }
    foreach ($entry in $purposeRequirements.GetEnumerator()) {
        $hasPurpose = $purposeCounts[$entry.Key] -gt 0
        $hasBlocker = $blockers -ccontains $entry.Value
        if ($hasPurpose -ne $hasBlocker) {
            throw "$Label evidence purpose '$($entry.Key)' and blocker '$($entry.Value)' must appear together."
        }
    }
    $formalPurposeCount = $purposeCounts.formal_depth_profile + $purposeCounts.formal_camera_binding
    $hasFormalBlocker = $blockers -ccontains 'formal_depth_path_already_present'
    if ($hasFormalBlocker -and
        ($purposeCounts.formal_depth_profile -ne 1 -or $purposeCounts.formal_camera_binding -ne 1)) {
        throw "$Label formal Depth exclusion must bind exactly one profile and one camera binding."
    }
    if (-not $hasFormalBlocker -and $formalPurposeCount -ne 0) {
        throw "$Label contains formal Depth evidence without the matching exclusion blocker."
    }
}

function Assert-TargetShape {
    param([Parameter(Mandatory = $true)]$Target, [Parameter(Mandatory = $true)][int]$Index)
    $label = "targets[$Index]"
    Assert-ExactProperties $Target @(
        'gameId', 'appId', 'steamBuildId', 'executableBasename', 'executableSha256',
        'rendererBackend', 'shippedAdmission', 'capabilityGate',
        'finalClassificationGate', 'representativeSceneGate', 'protectionGate'
    ) @() $label

    Assert-NonBlankString $Target.gameId "$label.gameId"
    if ($Target.gameId -notmatch '^[a-z0-9][a-z0-9-]{0,127}$') { throw "$label.gameId is not canonical." }
    foreach ($decimalField in @('appId', 'steamBuildId')) {
        $value = Get-Field $Target $decimalField
        Assert-NonBlankString $value "$label.$decimalField"
        if ($value -notmatch '^\d+$' -or [decimal]$value -le 0) { throw "$label.$decimalField must be a positive decimal string." }
    }
    Assert-NonBlankString $Target.executableBasename "$label.executableBasename"
    if ([IO.Path]::GetFileName($Target.executableBasename) -cne $Target.executableBasename -or
        -not $Target.executableBasename.EndsWith('.exe', [StringComparison]::OrdinalIgnoreCase)) {
        throw "$label.executableBasename must be a basename ending in .exe."
    }
    Assert-NonBlankString $Target.executableSha256 "$label.executableSha256"
    if ($Target.executableSha256 -cnotmatch '^sha256:[0-9a-f]{64}$') { throw "$label.executableSha256 must be canonical lowercase SHA-256." }
    if ($Target.rendererBackend -cne 'd3d11') { throw "$label.rendererBackend must be d3d11." }
    if ($Target.shippedAdmission -notin @('admitted', 'onboarding')) { throw "$label.shippedAdmission is not allowed." }

    Assert-ExactProperties $Target.capabilityGate @(
        'evidenceTier', 'rendererEvidence', 'capability', 'runtimeMode',
        'depthCaptureAvailable', 'recordingReady', 'blockers'
    ) @() "$label.capabilityGate"
    Assert-NonBlankString $Target.capabilityGate.evidenceTier "$label.capabilityGate.evidenceTier"
    Assert-ExactProperties $Target.capabilityGate.rendererEvidence @(
        'schema', 'graphicsApis', 'rendererMode', 'apiEvidenceTier', 'exactBuildIdentity', 'evidence'
    ) @() "$label.capabilityGate.rendererEvidence"
    if ($Target.capabilityGate.rendererEvidence.schema -cne 'dm-renderer-backend-evidence-v2' -or
        $Target.capabilityGate.rendererEvidence.rendererMode -cne 'single') {
        throw "$label capability renderer evidence is not a supported single-backend record."
    }
    $targetRendererApis = Get-ArrayField $Target.capabilityGate.rendererEvidence 'graphicsApis'
    Assert-ExactStringSet $targetRendererApis @('d3d11') "$label capability renderer APIs"
    Assert-NonBlankString $Target.capabilityGate.rendererEvidence.apiEvidenceTier "$label.capabilityGate.rendererEvidence.apiEvidenceTier"
    Assert-ExactProperties $Target.capabilityGate.rendererEvidence.exactBuildIdentity @(
        'appId', 'buildId', 'executableBasename', 'executableSha256'
    ) @() "$label.capabilityGate.rendererEvidence.exactBuildIdentity"
    if ([string]$Target.capabilityGate.rendererEvidence.exactBuildIdentity.appId -cne [string]$Target.appId -or
        [string]$Target.capabilityGate.rendererEvidence.exactBuildIdentity.buildId -cne [string]$Target.steamBuildId -or
        -not ([string]$Target.capabilityGate.rendererEvidence.exactBuildIdentity.executableBasename).Equals(
            [string]$Target.executableBasename, [StringComparison]::OrdinalIgnoreCase) -or
        [string]$Target.capabilityGate.rendererEvidence.exactBuildIdentity.executableSha256 -cne [string]$Target.executableSha256) {
        throw "$label renderer exact-build identity does not match the target identity."
    }
    Assert-ExactProperties $Target.capabilityGate.rendererEvidence.evidence @(
        'path', 'locator'
    ) @() "$label.capabilityGate.rendererEvidence.evidence"
    Assert-NonBlankString $Target.capabilityGate.rendererEvidence.evidence.path "$label.capabilityGate.rendererEvidence.evidence.path"
    Assert-NonBlankString $Target.capabilityGate.rendererEvidence.evidence.locator "$label.capabilityGate.rendererEvidence.evidence.locator"
    if ([IO.Path]::IsPathRooted([string]$Target.capabilityGate.rendererEvidence.evidence.path) -or
        [string]$Target.capabilityGate.rendererEvidence.evidence.path -notmatch '^docs/[A-Za-z0-9._-]+\.md$') {
        throw "$label renderer evidence path must be one repository docs/*.md path."
    }
    if ($Target.capabilityGate.capability -notin @('not_assessed', 'adaptation_blocked')) {
        throw "$label.capabilityGate.capability may authorize discovery only from not_assessed or adaptation_blocked."
    }
    if ($null -ne $Target.capabilityGate.runtimeMode) { throw "$label.capabilityGate.runtimeMode must remain null." }
    Assert-Boolean $Target.capabilityGate.depthCaptureAvailable "$label.capabilityGate.depthCaptureAvailable"
    Assert-Boolean $Target.capabilityGate.recordingReady "$label.capabilityGate.recordingReady"
    if ($Target.capabilityGate.depthCaptureAvailable -or $Target.capabilityGate.recordingReady) {
        throw "$label must not authorize discovery from an available or Ready capability."
    }
    Assert-UniqueStringArray $Target.capabilityGate.blockers "$label.capabilityGate.blockers"

    Assert-ExactProperties $Target.finalClassificationGate @('mode') @() "$label.finalClassificationGate"
    if ($Target.finalClassificationGate.mode -cne 'record_must_be_absent') {
        throw "$label.finalClassificationGate.mode must be record_must_be_absent."
    }

    Assert-ExactProperties $Target.representativeSceneGate @(
        'required', 'attestationSchema', 'attestationKind', 'sceneKind',
        'maximumAgeSeconds', 'expectedWindow'
    ) @() "$label.representativeSceneGate"
    Assert-Boolean $Target.representativeSceneGate.required "$label.representativeSceneGate.required"
    if (-not $Target.representativeSceneGate.required -or
        $Target.representativeSceneGate.attestationSchema -cne 'generic-d3d11-representative-scene-attestation/v1' -or
        $Target.representativeSceneGate.attestationKind -cne 'local_operator' -or
        $Target.representativeSceneGate.sceneKind -cne 'representative_offline_single_player') {
        throw "$label representative-scene attestation is not the required fail-closed local operator contract."
    }
    Assert-IntegerRange $Target.representativeSceneGate.maximumAgeSeconds 1 600 "$label.representativeSceneGate.maximumAgeSeconds"
    Assert-ExactProperties $Target.representativeSceneGate.expectedWindow @(
        'className', 'clientWidth', 'clientHeight'
    ) @() "$label.representativeSceneGate.expectedWindow"
    Assert-NonBlankString $Target.representativeSceneGate.expectedWindow.className "$label.representativeSceneGate.expectedWindow.className"
    foreach ($dimension in @('clientWidth', 'clientHeight')) {
        $value = Get-Field $Target.representativeSceneGate.expectedWindow $dimension
        Assert-IntegerRange $value 1 16384 "$label.representativeSceneGate.expectedWindow.$dimension"
    }

    Assert-ExactProperties $Target.protectionGate @(
        'runtimeEvidenceSchema', 'expectedGraphicsApi', 'requireCompleteModuleEnumeration',
        'requireCompleteServiceEnumeration', 'requireCompleteDriverEnumeration',
        'requireNoLoadedAntiCheat', 'officialLaunchScope'
    ) @() "$label.protectionGate"
    if ($Target.protectionGate.runtimeEvidenceSchema -cne 'game-runtime-gate-evidence/v1' -or
        $Target.protectionGate.expectedGraphicsApi -cne 'd3d11' -or
        $Target.protectionGate.officialLaunchScope -cne 'offline_single_player') {
        throw "$label protection contract is not the supported generic D3D11 offline lane."
    }
    foreach ($flag in @(
        'requireCompleteModuleEnumeration', 'requireCompleteServiceEnumeration',
        'requireCompleteDriverEnumeration', 'requireNoLoadedAntiCheat'
    )) {
        Assert-Boolean (Get-Field $Target.protectionGate $flag) "$label.protectionGate.$flag"
        if (-not (Get-Field $Target.protectionGate $flag)) { throw "$label.protectionGate.$flag must be true." }
    }
}

function Test-TargetBindings {
    param(
        [Parameter(Mandatory = $true)]$TargetManifest,
        [Parameter(Mandatory = $true)]$ShippedManifest,
        [Parameter(Mandatory = $true)]$RightsManifest,
        [Parameter(Mandatory = $true)]$CapabilitiesManifest,
        [Parameter(Mandatory = $true)]$FinalManifest,
        [string]$SelectedGameId = '',
        [string]$SelectedAppId = '',
        [string]$SelectedBuildId = '',
        [string]$SelectedExecutableBasename = '',
        [string]$SelectedExecutableSha256 = ''
    )

    Assert-ExactProperties $TargetManifest @('schemaVersion', 'targets') @('$comment') 'target manifest'
    if ($TargetManifest.schemaVersion -cne $script:TargetSchema) { throw 'target manifest schemaVersion is unsupported.' }
    $targets = Get-ArrayField $TargetManifest 'targets'
    if ($targets.Count -eq 0) { throw 'target manifest must contain at least one target.' }

    $seenGameIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $seenAppIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    for ($index = 0; $index -lt $targets.Count; $index++) {
        $target = $targets[$index]
        Assert-TargetShape $target $index
        if (-not $seenGameIds.Add([string]$target.gameId)) { throw "duplicate discovery target gameId: $($target.gameId)" }
        if (-not $seenAppIds.Add([string]$target.appId)) { throw "duplicate discovery target appId: $($target.appId)" }
    }

    if ($ShippedManifest.schemaVersion -cne $script:ShippedSchema) { throw 'shipped-games schemaVersion is unsupported.' }
    if ([int]$RightsManifest.schema_version -ne $script:RightsSchema) { throw 'rights schema_version is unsupported.' }
    if ($CapabilitiesManifest.schemaVersion -cne $script:CapabilitySchema) { throw 'capability schemaVersion is unsupported.' }
    if ([int]$FinalManifest.schema_version -ne $script:FinalSchema) { throw 'final-classification schema_version is unsupported.' }
    $shippedGames = Get-ArrayField $ShippedManifest 'games'
    $rightsRecords = Get-ArrayField $RightsManifest 'records'
    $capabilities = Get-ArrayField $CapabilitiesManifest 'games'
    $finalRecords = Get-ArrayField $FinalManifest 'records'

    foreach ($target in $targets) {
        $shipped = @($shippedGames | Where-Object { (Get-Field $_ 'gameId') -ceq $target.gameId })
        if ($shipped.Count -ne 1) { throw "target $($target.gameId) must match exactly one shipped game." }
        if ([string](Get-Field $shipped[0] 'appId') -cne $target.appId) { throw "target $($target.gameId) AppID does not match shipped-games." }
        if ([string](Get-Field $shipped[0] 'admission') -cne $target.shippedAdmission) { throw "target $($target.gameId) admission drifted." }

        $rights = @($rightsRecords | Where-Object { [string](Get-Field $_ 'app_id') -ceq $target.appId })
        if ($rights.Count -ne 1) { throw "target $($target.gameId) must match exactly one rights record." }
        $rights = $rights[0]
        if ([string](Get-Field $rights 'decision') -notin @('publisher_approved', 'counsel_approved')) {
            throw "target $($target.gameId) rights decision is not approved."
        }
        $rightsCatalogIds = Get-ArrayField $rights 'catalog_game_ids'
        if (-not ($rightsCatalogIds -ccontains $target.gameId)) {
            throw "target $($target.gameId) is not owned by its AppID rights record."
        }
        Test-ApprovedRightsScopes $rights "target $($target.gameId)"

        $capability = @($capabilities | Where-Object { (Get-Field $_ 'gameId') -ceq $target.gameId })
        if ($capability.Count -ne 1) { throw "target $($target.gameId) must match exactly one capability row." }
        $capability = $capability[0]
        foreach ($field in @('evidenceTier', 'capability')) {
            if ([string](Get-Field $capability $field) -cne [string](Get-Field $target.capabilityGate $field)) {
                throw "target $($target.gameId) capability field '$field' drifted."
            }
        }
        $renderer = Get-Field $capability 'rendererEvidence'
        if ($null -eq $renderer -or
            [string](Get-Field $renderer 'schema') -cne $target.capabilityGate.rendererEvidence.schema -or
            [string](Get-Field $renderer 'rendererMode') -cne $target.capabilityGate.rendererEvidence.rendererMode -or
            [string](Get-Field $renderer 'apiEvidenceTier') -cne $target.capabilityGate.rendererEvidence.apiEvidenceTier) {
            throw "target $($target.gameId) capability renderer evidence drifted."
        }
        $actualRendererApis = Get-ArrayField $renderer 'graphicsApis'
        $expectedRendererApis = Get-ArrayField $target.capabilityGate.rendererEvidence 'graphicsApis'
        Assert-ExactStringSet $actualRendererApis $expectedRendererApis "target $($target.gameId) renderer APIs"
        $actualRendererEvidence = Get-Field $renderer 'evidence'
        $expectedRendererEvidence = Get-Field $target.capabilityGate.rendererEvidence 'evidence'
        if ($null -eq $actualRendererEvidence -or $null -eq $expectedRendererEvidence -or
            [string](Get-Field $actualRendererEvidence 'path') -cne [string](Get-Field $expectedRendererEvidence 'path') -or
            [string](Get-Field $actualRendererEvidence 'locator') -cne [string](Get-Field $expectedRendererEvidence 'locator')) {
            throw "target $($target.gameId) renderer evidence reference drifted."
        }
        $actualRendererIdentity = Get-Field $renderer 'exactBuildIdentity'
        $expectedRendererIdentity = Get-Field $target.capabilityGate.rendererEvidence 'exactBuildIdentity'
        foreach ($identityField in @('appId', 'buildId', 'executableBasename', 'executableSha256')) {
            if ([string](Get-Field $actualRendererIdentity $identityField) -cne
                [string](Get-Field $expectedRendererIdentity $identityField)) {
                throw "target $($target.gameId) renderer exact-build identity field '$identityField' drifted."
            }
        }
        if ($null -ne (Get-Field $capability 'runtimeMode') -or $null -ne $target.capabilityGate.runtimeMode) {
            throw "target $($target.gameId) runtimeMode is not the bound null discovery state."
        }
        foreach ($field in @('depthCaptureAvailable', 'recordingReady')) {
            Assert-Boolean (Get-Field $capability $field) "capability.$field"
            if ((Get-Field $capability $field) -ne (Get-Field $target.capabilityGate $field)) {
                throw "target $($target.gameId) capability field '$field' drifted."
            }
        }
        $actualBlockers = Get-ArrayField $capability 'blockers'
        $expectedBlockers = Get-ArrayField $target.capabilityGate 'blockers'
        Assert-ExactStringSet $actualBlockers $expectedBlockers "target $($target.gameId) blockers"

        $final = @($finalRecords | Where-Object { [string](Get-Field $_ 'app_id') -ceq $target.appId })
        if ($target.finalClassificationGate.mode -eq 'record_must_be_absent' -and $final.Count -ne 0) {
            throw "target $($target.gameId) now has a final-classification record; discovery requires explicit re-review."
        }
    }

    $selectionValues = @(
        $SelectedGameId, $SelectedAppId, $SelectedBuildId,
        $SelectedExecutableBasename, $SelectedExecutableSha256
    )
    $selectionCount = @($selectionValues | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }).Count
    if ($selectionCount -notin @(0, 5)) { throw 'selection requires gameId, AppID, buildId, EXE basename, and EXE SHA-256 together.' }
    if ($selectionCount -eq 0) { return $null }

    $selected = @($targets | Where-Object { $_.gameId -ceq $SelectedGameId })
    if ($selected.Count -ne 1) { throw "gameId '$SelectedGameId' is not uniquely authorized for generic D3D11 discovery." }
    $selected = $selected[0]
    if ($selected.appId -cne $SelectedAppId -or
        $selected.steamBuildId -cne $SelectedBuildId -or
        -not $selected.executableBasename.Equals($SelectedExecutableBasename, [StringComparison]::OrdinalIgnoreCase) -or
        $selected.executableSha256 -cne $SelectedExecutableSha256.ToLowerInvariant()) {
        throw 'requested AppID/build/executable identity does not exactly match the discovery target allowlist.'
    }
    return $selected
}

function Test-EligibilityAuditBindings {
    param(
        [Parameter(Mandatory = $true)]$AuditManifest,
        [Parameter(Mandatory = $true)]$TargetManifest,
        [Parameter(Mandatory = $true)]$ShippedManifest,
        [Parameter(Mandatory = $true)]$RightsManifest,
        [Parameter(Mandatory = $true)]$CapabilitiesManifest,
        [Parameter(Mandatory = $true)]$FinalManifest,
        [Parameter(Mandatory = $true)][string]$DataMinerRoot
    )

    Assert-ExactProperties $AuditManifest @('schemaVersion', 'reviewedAt', 'summary', 'games') @('$comment') 'eligibility audit'
    if ($AuditManifest.schemaVersion -cne $script:EligibilitySchema) { throw 'eligibility audit schemaVersion is unsupported.' }
    Assert-NonBlankString $AuditManifest.reviewedAt 'eligibility audit reviewedAt'
    if ([string]$AuditManifest.reviewedAt -cnotmatch '^\d{4}-\d{2}-\d{2}$') {
        throw 'eligibility audit reviewedAt must be an ISO calendar date.'
    }
    Assert-ExactProperties $AuditManifest.summary @(
        'shippedGames', 'd3d11EvidenceGames', 'authorizedTargets', 'rejectedD3d11Games',
        'outOfBackendScopeGames', 'existingGameSpecificPathsPreferred', 'formalDepthPathsExcluded'
    ) @() 'eligibility audit summary'
    foreach ($field in $AuditManifest.summary.PSObject.Properties.Name) {
        Assert-IntegerRange (Get-Field $AuditManifest.summary $field) 0 10000 "eligibility audit summary.$field"
    }

    $auditRows = Get-ArrayField $AuditManifest 'games'
    $shippedGames = Get-ArrayField $ShippedManifest 'games'
    $targets = Get-ArrayField $TargetManifest 'targets'
    $rightsRecords = Get-ArrayField $RightsManifest 'records'
    $capabilities = Get-ArrayField $CapabilitiesManifest 'games'
    $finalRecords = Get-ArrayField $FinalManifest 'records'
    if ($auditRows.Count -ne $shippedGames.Count) {
        throw 'eligibility audit must contain exactly one row per shipped game.'
    }

    $auditIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    for ($index = 0; $index -lt $auditRows.Count; $index++) {
        $row = $auditRows[$index]
        $label = "eligibility audit games[$index]"
        Assert-ExactProperties $row @('gameId', 'appId', 'disposition', 'blockers', 'evidenceBindings') @() $label
        Assert-NonBlankString $row.gameId "$label.gameId"
        Assert-NonBlankString $row.appId "$label.appId"
        if ([string]$row.appId -cnotmatch '^\d+$') { throw "$label.appId must be a decimal string." }
        if (-not $auditIds.Add([string]$row.gameId)) { throw "duplicate eligibility audit gameId: $($row.gameId)" }
    }
    foreach ($shipped in $shippedGames) {
        if (-not $auditIds.Contains([string]$shipped.gameId)) {
            throw "eligibility audit is missing shipped game $($shipped.gameId)."
        }
    }

    $d3d11Count = 0
    $authorizedCount = 0
    $rejectedCount = 0
    $outOfScopeCount = 0
    $existingSpecificCount = 0
    $formalDepthCount = 0

    foreach ($row in $auditRows) {
        $label = "eligibility audit $($row.gameId)"
        $shipped = @($shippedGames | Where-Object { [string](Get-Field $_ 'gameId') -ceq $row.gameId })
        if ($shipped.Count -ne 1) { throw "$label must match exactly one shipped game." }
        $rights = @($rightsRecords | Where-Object { (Get-ArrayField $_ 'catalog_game_ids') -ccontains $row.gameId })
        if ($rights.Count -ne 1) { throw "$label must map to exactly one rights record." }
        if ([string]$rights[0].app_id -cne [string]$row.appId) { throw "$label AppID drifted from the rights ledger." }
        if ([string]$rights[0].decision -notin @('publisher_approved', 'counsel_approved')) {
            throw "$label rights decision is not approved."
        }
        Test-ApprovedRightsScopes $rights[0] $label
        $capability = @($capabilities | Where-Object { [string](Get-Field $_ 'gameId') -ceq $row.gameId })
        if ($capability.Count -ne 1) { throw "$label must match exactly one capability row." }
        $capability = $capability[0]
        $renderer = Get-Field $capability 'rendererEvidence'
        $apis = Get-ArrayField $renderer 'graphicsApis'
        $isD3d11 = $apis -ccontains 'd3d11'
        if ($isD3d11) { $d3d11Count++ }
        $target = @($targets | Where-Object { [string](Get-Field $_ 'gameId') -ceq $row.gameId })

        $blockersValue = Get-ArrayField $row 'blockers'
        $blockers = @($blockersValue)
        if ($blockers.Count -gt 0) {
            Assert-UniqueStringArray $blockers "$label.blockers"
            foreach ($blocker in $blockers) {
                if ($script:EligibilityBlockers -cnotcontains $blocker) {
                    throw "$label contains unsupported blocker '$blocker'."
                }
            }
        }

        if ($target.Count -eq 1) {
            $authorizedCount++
            if ([string]$row.disposition -cne 'authorized_target') {
                throw "$label is in the target registry and must be authorized_target."
            }
            Assert-EmptyArray $blockersValue "$label.blockers"
            Assert-EmptyArray (Get-ArrayField $row 'evidenceBindings') "$label.evidenceBindings"
            continue
        }
        if ($target.Count -ne 0) { throw "$label matches duplicate target rows." }
        if ([string]$row.disposition -ceq 'authorized_target') {
            throw "$label is not in the target registry and cannot be authorized_target."
        }

        if (-not $isD3d11) {
            $outOfScopeCount++
            if ([string]$row.disposition -cne 'out_of_backend_scope') {
                throw "$label has no D3D11 renderer evidence and must remain out_of_backend_scope."
            }
            $expectedRendererBlocker = if ($apis.Count -eq 1 -and $apis[0] -ceq 'unknown') {
                'renderer_backend_unknown'
            } else {
                'renderer_evidence_excludes_d3d11'
            }
            Assert-ExactStringSet $blockers @($expectedRendererBlocker) "$label blockers"
            Assert-EmptyArray (Get-ArrayField $row 'evidenceBindings') "$label.evidenceBindings"
            continue
        }

        $rejectedCount++
        if ([string]$row.disposition -cne 'rejected_d3d11') {
            throw "$label is a non-target D3D11 game and must remain rejected_d3d11."
        }
        if ($blockers.Count -eq 0) { throw "$label rejected_d3d11 requires at least one blocker." }

        $identity = Get-Field $renderer 'exactBuildIdentity'
        $identityMissing = $null -eq $identity
        $buildMissing = -not $identityMissing -and $null -eq (Get-Field $identity 'buildId')
        Assert-RequiredBlocker $blockers 'exact_build_identity_missing' $identityMissing $label
        Assert-RequiredBlocker $blockers 'exact_build_id_binding_missing' $buildMissing $label
        $runtimeReportMissing = [string](Get-Field $renderer 'apiEvidenceTier') -cne 'exact_build_runtime_report'
        Assert-RequiredBlocker $blockers 'exact_build_renderer_runtime_missing' $runtimeReportMissing $label

        $final = @($finalRecords | Where-Object { [string](Get-Field $_ 'app_id') -ceq [string]$row.appId })
        Assert-RequiredBlocker $blockers 'final_classification_gate_unmodeled' ($final.Count -gt 0) $label

        $catalogDir = [string](Get-Field $shipped[0] 'catalogDir')
        Assert-NonBlankString $catalogDir "$label shipped catalogDir"
        $gameProfilePath = Join-Path $DataMinerRoot "config/catalogs/dev/$catalogDir/game-profile.json"
        if (-not (Test-Path -LiteralPath $gameProfilePath -PathType Leaf)) {
            throw "$label shipped game profile is missing."
        }
        Read-JsonFile $gameProfilePath "$label shipped game profile" | Out-Null
        # A class-only catalog row is not the target registry's exact class + client-size scene binding.
        Assert-RequiredBlocker $blockers 'representative_window_binding_missing' $true $label

        $identityConflict = (Get-ArrayField $capability 'blockers') -ccontains 'catalog_final_executable_identity_conflict'
        Assert-RequiredBlocker $blockers 'catalog_final_executable_identity_conflict' $identityConflict $label

        $cleanOfficialInstallMissing = (Get-ArrayField $capability 'blockers') -ccontains 'clean_official_install_surface_missing'
        Assert-RequiredBlocker $blockers 'clean_official_install_surface_missing' $cleanOfficialInstallMissing $label

        $formalDepth = [string](Get-Field $capability 'capability') -ceq 'catalog_ready'
        Assert-RequiredBlocker $blockers 'formal_depth_path_already_present' $formalDepth $label
        if ($formalDepth) { $formalDepthCount++ }
        if ($blockers -ccontains 'existing_game_specific_path_preferred') { $existingSpecificCount++ }
        Test-EligibilityEvidenceBindings $row $capability $DataMinerRoot $label
    }

    $computed = [ordered]@{
        shippedGames = $shippedGames.Count
        d3d11EvidenceGames = $d3d11Count
        authorizedTargets = $authorizedCount
        rejectedD3d11Games = $rejectedCount
        outOfBackendScopeGames = $outOfScopeCount
        existingGameSpecificPathsPreferred = $existingSpecificCount
        formalDepthPathsExcluded = $formalDepthCount
    }
    foreach ($entry in $computed.GetEnumerator()) {
        if ([int64](Get-Field $AuditManifest.summary $entry.Key) -ne [int64]$entry.Value) {
            throw "eligibility audit summary.$($entry.Key) must equal $($entry.Value)."
        }
    }
}

function Read-JsonFile {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Label)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label does not exist: $Path" }
    return Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
}

function Copy-JsonObject {
    param([Parameter(Mandatory = $true)]$Value)
    return ($Value | ConvertTo-Json -Depth 20 | ConvertFrom-Json)
}

function Invoke-SelfTest {
    $target = [pscustomobject]@{
        gameId = 'fixture-game-steam'; appId = '100'; steamBuildId = '200'
        executableBasename = 'Game.exe'
        executableSha256 = 'sha256:' + ('a' * 64)
        rendererBackend = 'd3d11'; shippedAdmission = 'onboarding'
        capabilityGate = [pscustomobject]@{
            evidenceTier = 'exact_build_planning_only'
            rendererEvidence = [pscustomobject]@{
                schema = 'dm-renderer-backend-evidence-v2'; graphicsApis = @('d3d11')
                rendererMode = 'single'; apiEvidenceTier = 'exact_build_observation'
                exactBuildIdentity = [pscustomobject]@{
                    appId = '100'; buildId = '200'; executableBasename = 'Game.exe'
                    executableSha256 = 'sha256:' + ('a' * 64)
                }
                evidence = [pscustomobject]@{ path = 'docs/fixture.md'; locator = '§1 fixture renderer observation' }
            }
            capability = 'not_assessed'; runtimeMode = $null
            depthCaptureAvailable = $false; recordingReady = $false
            blockers = @('d3d11_dsv_discovery_missing', 'depth_profile_missing', 'camera_binding_missing')
        }
        finalClassificationGate = [pscustomobject]@{ mode = 'record_must_be_absent' }
        representativeSceneGate = [pscustomobject]@{
            required = $true
            attestationSchema = 'generic-d3d11-representative-scene-attestation/v1'
            attestationKind = 'local_operator'
            sceneKind = 'representative_offline_single_player'
            maximumAgeSeconds = 120
            expectedWindow = [pscustomobject]@{
                className = 'FixtureWindow'; clientWidth = 1920; clientHeight = 1080
            }
        }
        protectionGate = [pscustomobject]@{
            runtimeEvidenceSchema = 'game-runtime-gate-evidence/v1'; expectedGraphicsApi = 'd3d11'
            requireCompleteModuleEnumeration = $true; requireCompleteServiceEnumeration = $true
            requireCompleteDriverEnumeration = $true; requireNoLoadedAntiCheat = $true
            officialLaunchScope = 'offline_single_player'
        }
    }
    $targetManifest = [pscustomobject]@{ schemaVersion = $script:TargetSchema; targets = @($target) }
    $shipped = [pscustomobject]@{ schemaVersion = $script:ShippedSchema; games = @([pscustomobject]@{ gameId = 'fixture-game-steam'; appId = '100'; admission = 'onboarding' }) }
    $rights = [pscustomobject]@{ schema_version = $script:RightsSchema; records = @([pscustomobject]@{
        app_id = '100'; decision = 'counsel_approved'; catalog_game_ids = @('fixture-game-steam')
        scopes = [pscustomobject]@{
            instrumentation = [pscustomobject]@{ approved = $true; boundaries = 'Bounded instrumentation.' }
            capture = [pscustomobject]@{ approved = $true; boundaries = 'Bounded capture.' }
            commercial_ml = [pscustomobject]@{ approved = $true; boundaries = 'Bounded ML.' }
            delivery = [pscustomobject]@{ approved = $true; boundaries = 'Bounded delivery.' }
        }
    }) }
    $capabilities = [pscustomobject]@{ schemaVersion = $script:CapabilitySchema; games = @([pscustomobject]@{
        gameId = 'fixture-game-steam'; evidenceTier = 'exact_build_planning_only'; capability = 'not_assessed'
        rendererEvidence = [pscustomobject]@{
            schema = 'dm-renderer-backend-evidence-v2'; graphicsApis = @('d3d11')
            rendererMode = 'single'; apiEvidenceTier = 'exact_build_observation'
            exactBuildIdentity = [pscustomobject]@{
                appId = '100'; buildId = '200'; executableBasename = 'Game.exe'
                executableSha256 = 'sha256:' + ('a' * 64)
            }
            evidence = [pscustomobject]@{ path = 'docs/fixture.md'; locator = '§1 fixture renderer observation' }
        }
        runtimeMode = $null; depthCaptureAvailable = $false; recordingReady = $false
        blockers = @('d3d11_dsv_discovery_missing', 'depth_profile_missing', 'camera_binding_missing')
    }) }
    $final = [pscustomobject]@{ schema_version = 1; records = @([pscustomobject]@{ app_id = '999'; classification = 'blocked' }) }

    $selected = Test-TargetBindings `
        -TargetManifest $targetManifest -ShippedManifest $shipped -RightsManifest $rights `
        -CapabilitiesManifest $capabilities -FinalManifest $final `
        -SelectedGameId 'fixture-game-steam' -SelectedAppId '100' -SelectedBuildId '200' `
        -SelectedExecutableBasename 'Game.exe' -SelectedExecutableSha256 ('sha256:' + ('a' * 64))
    if ($null -eq $selected -or $selected.gameId -cne 'fixture-game-steam') { throw 'self-test happy path did not select the target.' }
    $tests = 1

    $negativeCases = @(
        @{ Name = 'unknown target field'; Mutate = { param($t, $s, $r, $c, $f) $t.targets[0] | Add-Member unexpected $true } },
        @{ Name = 'duplicate target'; Mutate = { param($t, $s, $r, $c, $f) $t.targets = @($t.targets[0], (Copy-JsonObject $t.targets[0])) } },
        @{ Name = 'shipped AppID drift'; Mutate = { param($t, $s, $r, $c, $f) $s.games[0].appId = '101' } },
        @{ Name = 'rights gameId drift'; Mutate = { param($t, $s, $r, $c, $f) $r.records[0].catalog_game_ids = @('other-game-steam') } },
        @{ Name = 'capability promotion'; Mutate = { param($t, $s, $r, $c, $f) $c.games[0].capability = 'catalog_ready' } },
        @{ Name = 'Ready promotion'; Mutate = { param($t, $s, $r, $c, $f) $c.games[0].recordingReady = $true } },
        @{ Name = 'blocker drift'; Mutate = { param($t, $s, $r, $c, $f) $c.games[0].blockers = @('depth_profile_missing') } },
        @{ Name = 'renderer evidence locator drift'; Mutate = { param($t, $s, $r, $c, $f) $c.games[0].rendererEvidence.evidence.locator = '§9 unrelated renderer observation' } },
        @{ Name = 'renderer exact-build identity drift'; Mutate = { param($t, $s, $r, $c, $f) $c.games[0].rendererEvidence.exactBuildIdentity.buildId = '201' } },
        @{ Name = 'final classification appeared'; Mutate = { param($t, $s, $r, $c, $f) $f.records = @([pscustomobject]@{ app_id = '100'; classification = 'blocked'; no_injection = $false }) } },
        @{ Name = 'representative-scene attestation disabled'; Mutate = { param($t, $s, $r, $c, $f) $t.targets[0].representativeSceneGate.required = $false } },
        @{ Name = 'representative-scene kind weakened'; Mutate = { param($t, $s, $r, $c, $f) $t.targets[0].representativeSceneGate.sceneKind = 'main_menu' } },
        @{ Name = 'representative-scene window unbound'; Mutate = { param($t, $s, $r, $c, $f) $t.targets[0].representativeSceneGate.expectedWindow.className = '' } },
        @{ Name = 'anti-cheat gate disabled'; Mutate = { param($t, $s, $r, $c, $f) $t.targets[0].protectionGate.requireNoLoadedAntiCheat = $false } }
    )
    foreach ($case in $negativeCases) {
        $t = Copy-JsonObject $targetManifest
        $s = Copy-JsonObject $shipped
        $r = Copy-JsonObject $rights
        $c = Copy-JsonObject $capabilities
        $f = Copy-JsonObject $final
        & $case.Mutate $t $s $r $c $f
        $blocked = $false
        try {
            Test-TargetBindings -TargetManifest $t -ShippedManifest $s -RightsManifest $r `
                -CapabilitiesManifest $c -FinalManifest $f | Out-Null
        } catch { $blocked = $true }
        if (-not $blocked) { throw "self-test negative case passed unexpectedly: $($case.Name)" }
        $tests++
    }

    $wrongSelectionBlocked = $false
    try {
        Test-TargetBindings `
            -TargetManifest $targetManifest -ShippedManifest $shipped -RightsManifest $rights `
            -CapabilitiesManifest $capabilities -FinalManifest $final `
            -SelectedGameId 'fixture-game-steam' -SelectedAppId '100' -SelectedBuildId '201' `
            -SelectedExecutableBasename 'Game.exe' -SelectedExecutableSha256 ('sha256:' + ('a' * 64)) | Out-Null
    } catch { $wrongSelectionBlocked = $true }
    if (-not $wrongSelectionBlocked) { throw 'self-test wrong build selection passed unexpectedly.' }
    $tests++

    $dataMinerRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
    $realTargets = Read-JsonFile (Join-Path $dataMinerRoot 'config/generic-d3d11-dsv-discovery-targets.json') 'self-test target manifest'
    $realAudit = Read-JsonFile (Join-Path $dataMinerRoot 'config/generic-d3d11-dsv-discovery-eligibility.json') 'self-test eligibility audit'
    $realShipped = Read-JsonFile (Join-Path $dataMinerRoot 'config/catalogs/shipped-games.json') 'self-test shipped manifest'
    $realRights = Read-JsonFile (Join-Path $dataMinerRoot 'config/game-depth-rights-gates.json') 'self-test rights manifest'
    $realCapabilities = Read-JsonFile (Join-Path $dataMinerRoot 'config/game-depth-capabilities.json') 'self-test capabilities manifest'
    $realFinal = Read-JsonFile (Join-Path $dataMinerRoot 'config/game-depth-final-classifications.json') 'self-test final manifest'
    Test-EligibilityAuditBindings `
        -AuditManifest $realAudit -TargetManifest $realTargets -ShippedManifest $realShipped `
        -RightsManifest $realRights -CapabilitiesManifest $realCapabilities -FinalManifest $realFinal `
        -DataMinerRoot $dataMinerRoot
    $tests++

    $eligibilityNegativeCases = @(
        @{ Name = 'unknown eligibility row field'; Mutate = {
                param($a) $a.games[0] | Add-Member unexpected $true
            } },
        @{ Name = 'missing shipped eligibility row'; Mutate = {
                param($a) $a.games = @($a.games | Select-Object -Skip 1)
            } },
        @{ Name = 'non-target authorization'; Mutate = {
                param($a) ($a.games | Where-Object gameId -ceq 'call-of-duty-steam-release-test').disposition = 'authorized_target'
            } },
        @{ Name = 'eligibility AppID drift'; Mutate = {
                param($a) ($a.games | Where-Object gameId -ceq 'euro-truck-simulator-2-steam').appId = '1'
            } },
        @{ Name = 'missing exact build blocker'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'euro-truck-simulator-2-steam'; $row.blockers = @($row.blockers | Where-Object { $_ -cne 'exact_build_id_binding_missing' })
            } },
        @{ Name = 'class-only window blocker missing'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'euro-truck-simulator-2-steam'; $row.blockers = @($row.blockers | Where-Object { $_ -cne 'representative_window_binding_missing' })
            } },
        @{ Name = 'target gains blocker'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'assassins-creed-iii-remastered-steam'; $row.blockers = @('exact_build_id_binding_missing')
            } },
        @{ Name = 'evidence hash drift'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'bioshock-infinite-steam-e2e'; $row.evidenceBindings[0].sha256 = 'sha256:' + ('0' * 64)
            } },
        @{ Name = 'evidence path traversal'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'bioshock-infinite-steam-e2e'; $row.evidenceBindings[0].path = '../outside.json'
            } },
        @{ Name = 'existing path purpose detached'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'halo-reach-mcc-s0'; $row.blockers = @($row.blockers | Where-Object { $_ -cne 'existing_game_specific_path_preferred' })
            } },
        @{ Name = 'formal Depth pair incomplete'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'fallout4-steam-release-test'; $row.evidenceBindings = @($row.evidenceBindings | Where-Object { $_.purposes -cnotcontains 'formal_camera_binding' })
            } },
        @{ Name = 'final classification blocker missing'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'bioshock-infinite-steam-e2e'; $row.blockers = @($row.blockers | Where-Object { $_ -cne 'final_classification_gate_unmodeled' })
            } },
        @{ Name = 'rights capture scope unbounded'; Mutate = {
                param($a, $t, $s, $r) $record = $r.records | Where-Object { $_.catalog_game_ids -ccontains 'euro-truck-simulator-2-steam' }; $record.scopes.capture.boundaries = ''
            } },
        @{ Name = 'clean official install blocker missing'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'gta5-legacy-pc'; $row.blockers = @($row.blockers | Where-Object { $_ -cne 'clean_official_install_surface_missing' })
            } },
        @{ Name = 'unknown renderer overstated'; Mutate = {
                param($a) $row = $a.games | Where-Object gameId -ceq 'prey-2017-pc'; $row.blockers = @('renderer_evidence_excludes_d3d11')
            } },
        @{ Name = 'eligibility summary drift'; Mutate = {
                param($a) $a.summary.authorizedTargets = 2
            } }
    )
    foreach ($case in $eligibilityNegativeCases) {
        $auditCopy = Copy-JsonObject $realAudit
        $targetsCopy = Copy-JsonObject $realTargets
        $shippedCopy = Copy-JsonObject $realShipped
        $rightsCopy = Copy-JsonObject $realRights
        $capabilitiesCopy = Copy-JsonObject $realCapabilities
        $finalCopy = Copy-JsonObject $realFinal
        & $case.Mutate $auditCopy $targetsCopy $shippedCopy $rightsCopy $capabilitiesCopy $finalCopy
        $blocked = $false
        try {
            Test-EligibilityAuditBindings `
                -AuditManifest $auditCopy -TargetManifest $targetsCopy -ShippedManifest $shippedCopy `
                -RightsManifest $rightsCopy -CapabilitiesManifest $capabilitiesCopy -FinalManifest $finalCopy `
                -DataMinerRoot $dataMinerRoot
        } catch { $blocked = $true }
        if (-not $blocked) { throw "self-test eligibility negative case passed unexpectedly: $($case.Name)" }
        $tests++
    }

    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

try {
    if ($SelfTest) { Invoke-SelfTest; exit 0 }

    if ([string]::IsNullOrWhiteSpace($TargetManifestPath)) { $TargetManifestPath = Join-Path $PSScriptRoot '..\..\config\generic-d3d11-dsv-discovery-targets.json' }
    if ([string]::IsNullOrWhiteSpace($ShippedGamesPath)) { $ShippedGamesPath = Join-Path $PSScriptRoot '..\..\config\catalogs\shipped-games.json' }
    if ([string]::IsNullOrWhiteSpace($RightsPath)) { $RightsPath = Join-Path $PSScriptRoot '..\..\config\game-depth-rights-gates.json' }
    if ([string]::IsNullOrWhiteSpace($CapabilitiesPath)) { $CapabilitiesPath = Join-Path $PSScriptRoot '..\..\config\game-depth-capabilities.json' }
    if ([string]::IsNullOrWhiteSpace($FinalClassificationsPath)) { $FinalClassificationsPath = Join-Path $PSScriptRoot '..\..\config\game-depth-final-classifications.json' }
    if ([string]::IsNullOrWhiteSpace($EligibilityAuditPath)) { $EligibilityAuditPath = Join-Path $PSScriptRoot '..\..\config\generic-d3d11-dsv-discovery-eligibility.json' }

    $targetManifest = Read-JsonFile $TargetManifestPath 'target manifest'
    $shipped = Read-JsonFile $ShippedGamesPath 'shipped-games manifest'
    $rights = Read-JsonFile $RightsPath 'rights manifest'
    $capabilities = Read-JsonFile $CapabilitiesPath 'capability manifest'
    $final = Read-JsonFile $FinalClassificationsPath 'final-classification manifest'
    $eligibility = Read-JsonFile $EligibilityAuditPath 'eligibility audit'
    $selected = Test-TargetBindings `
        -TargetManifest $targetManifest -ShippedManifest $shipped -RightsManifest $rights `
        -CapabilitiesManifest $capabilities -FinalManifest $final `
        -SelectedGameId $GameId -SelectedAppId $AppId -SelectedBuildId $BuildId `
        -SelectedExecutableBasename $ExecutableBasename -SelectedExecutableSha256 $ExecutableSha256
    $dataMinerRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
    Test-EligibilityAuditBindings `
        -AuditManifest $eligibility -TargetManifest $targetManifest -ShippedManifest $shipped `
        -RightsManifest $rights -CapabilitiesManifest $capabilities -FinalManifest $final `
        -DataMinerRoot $dataMinerRoot

    [pscustomobject][ordered]@{
        status = 'pass'
        schema_version = $script:TargetSchema
        target_count = @($targetManifest.targets).Count
        eligibility_schema_version = $script:EligibilitySchema
        eligibility_game_count = @($eligibility.games).Count
        selected_target = $selected
    } | ConvertTo-Json -Depth 12
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}

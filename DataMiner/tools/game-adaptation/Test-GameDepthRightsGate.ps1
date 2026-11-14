[CmdletBinding()]
param(
    [string]$ManifestPath,
    [string]$ShippedGamesPath,
    [string[]]$AppId = @(),
    [datetime]$AsOfDate = (Get-Date).Date,
    [string]$OutputPath,
    [switch]$ValidateOnly,
    [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ManifestPath)) {
    $ManifestPath = Join-Path $PSScriptRoot '..\..\config\game-depth-rights-gates.json'
}
if ([string]::IsNullOrWhiteSpace($ShippedGamesPath)) {
    $ShippedGamesPath = Join-Path $PSScriptRoot '..\..\config\catalogs\shipped-games.json'
}
$AppId = @(
    $AppId |
        ForEach-Object { $_ -split ',' } |
        ForEach-Object { $_.Trim() } |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
)

$script:ManifestSchemaVersion = 2
$script:AllowedDecisions = @(
    'not_requested',
    'requested',
    'publisher_approved',
    'counsel_approved',
    'denied',
    'expired'
)
$script:ApprovedDecisions = @('publisher_approved', 'counsel_approved')
$script:ScopeNames = @('instrumentation', 'capture', 'commercial_ml', 'delivery')

# The unified decision recorded in issue #855 §E retired the old `project_research_only`
# lane. Two contradictory policies must never be active at the same time, so an active
# top-level key by that name is a hard manifest error rather than an ignored leftover.
$script:RetiredManifestKeys = @('project_research_only')

# `catalog_game_ids` is an array precisely so one rights holder can own several catalog
# variants. That flexibility would otherwise let two genuinely distinct titles be merged
# into a single rights record, which issue #855 §E.3 forbids for the Halo identities.
$script:SeparateRightsIdentities = @(
    [pscustomobject]@{
        Left = 'halo-infinite-steam-release-test'
        Right = 'halo-reach-mcc-s0'
        Reason = 'Halo Infinite and Halo: The Master Chief Collection are different app_ids and must not share one rights record (issue #855 §E.3)'
    }
)

# `synthetic` is a regression fixture, not a game; it must never receive a rights record.
$script:NonGameCatalogIds = @('synthetic')

function Get-Field {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

# PowerShell unrolls collections returned from a function, so `Get-Field` would hand back a
# bare string for a one-element JSON array and `$null` for an empty one. Array-valued fields
# must go through this accessor, which pins the array with the unary comma operator.
function Get-ArrayField {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    $value = $property.Value
    if ($value -isnot [Array]) { return $null }
    return , $value
}

function Test-HasProperty {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $Object) { return $false }
    return $null -ne $Object.PSObject.Properties[$Name]
}

function Test-NonBlankString {
    param([AllowNull()]$Value)
    return ($Value -is [string]) -and -not [string]::IsNullOrWhiteSpace($Value)
}

function Test-IsoDate {
    param(
        [AllowNull()]$Value,
        [ref]$Parsed
    )

    if (-not (Test-NonBlankString $Value)) { return $false }
    $candidate = [datetime]::MinValue
    $ok = [datetime]::TryParseExact(
        $Value,
        'yyyy-MM-dd',
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::None,
        [ref]$candidate
    )
    if ($ok) { $Parsed.Value = $candidate.Date }
    return $ok
}

function Test-AbsoluteHttpUrl {
    param([AllowNull()]$Value)

    if (-not (Test-NonBlankString $Value)) { return $false }
    $uri = $null
    if (-not [uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri)) { return $false }
    return $uri.Scheme -in @('https', 'http')
}

# issue #855 §E.1: the written counsel document is still being produced, so a per-game
# `evidence_url` may be absent, null or blank without blocking the gate. A value that IS
# present must still be a real absolute http(s) link — an obviously broken reference must
# not be able to masquerade as evidence.
function Test-OptionalEvidenceUrl {
    param([AllowNull()]$Value)

    if ($null -eq $Value) { return $true }
    if ($Value -isnot [string]) { return $false }
    if ([string]::IsNullOrWhiteSpace($Value)) { return $true }
    return (Test-AbsoluteHttpUrl $Value)
}

function Test-UnifiedRightsDecision {
    param(
        [AllowNull()]$Decision,
        [Parameter(Mandatory = $true)][datetime]$EffectiveAsOf
    )

    $schemaErrors = [Collections.Generic.List[string]]::new()
    if ($null -eq $Decision) {
        $schemaErrors.Add('unified_rights_decision object is required')
        return [pscustomobject][ordered]@{
            schema_valid = $false
            schema_errors = @($schemaErrors)
            declared_by = $null
            declared_at = $null
            reference_url = $null
            supersedes = @()
            note = $null
        }
    }

    $declaredBy = Get-Field $Decision 'declared_by'
    $declaredAt = Get-Field $Decision 'declared_at'
    $referenceUrl = Get-Field $Decision 'reference_url'
    $supersedes = Get-ArrayField $Decision 'supersedes'
    $note = Get-Field $Decision 'note'

    foreach ($field in @(
        @{ Name = 'declared_by'; Value = $declaredBy },
        @{ Name = 'note'; Value = $note }
    )) {
        if (-not (Test-NonBlankString $field.Value)) {
            $schemaErrors.Add("unified_rights_decision.$($field.Name) must be a non-empty string")
        }
    }
    if (-not (Test-AbsoluteHttpUrl $referenceUrl)) {
        $schemaErrors.Add('unified_rights_decision.reference_url must be an absolute http(s) link')
    }

    $parsedDeclaredAt = [datetime]::MinValue
    if (-not (Test-IsoDate $declaredAt ([ref]$parsedDeclaredAt))) {
        $schemaErrors.Add('unified_rights_decision.declared_at must use yyyy-MM-dd')
    } elseif ($parsedDeclaredAt -gt $EffectiveAsOf.Date) {
        $schemaErrors.Add('unified_rights_decision.declared_at is in the future')
    }

    if ($null -eq $supersedes) {
        $schemaErrors.Add('unified_rights_decision.supersedes must be an array')
        $supersedes = @()
    } else {
        foreach ($retired in $script:RetiredManifestKeys) {
            if ($retired -notin $supersedes) {
                $schemaErrors.Add("unified_rights_decision.supersedes must record the retired '$retired' lane")
            }
        }
    }

    return [pscustomobject][ordered]@{
        schema_valid = ($schemaErrors.Count -eq 0)
        schema_errors = @($schemaErrors)
        declared_by = $declaredBy
        declared_at = $declaredAt
        reference_url = $referenceUrl
        supersedes = @($supersedes)
        note = $note
    }
}

function Test-RightsRecord {
    param(
        [Parameter(Mandatory = $true)]$Record,
        [Parameter(Mandatory = $true)][datetime]$EffectiveAsOf
    )

    $schemaErrors = [Collections.Generic.List[string]]::new()
    $gateReasons = [Collections.Generic.List[string]]::new()

    $recordAppId = Get-Field $Record 'app_id'
    $game = Get-Field $Record 'game'
    $rightsOwner = Get-Field $Record 'rights_owner'
    $decision = Get-Field $Record 'decision'
    $scopes = Get-Field $Record 'scopes'
    $modeRestrictions = Get-Field $Record 'mode_restrictions'
    $effectiveDate = Get-Field $Record 'effective_date'
    $reviewDate = Get-Field $Record 'expiry_or_review_date'
    $evidenceUrl = Get-Field $Record 'evidence_url'
    $reviewer = Get-Field $Record 'reviewer'
    $intakeRoute = Get-Field $Record 'intake_route'
    $catalogGameIds = Get-ArrayField $Record 'catalog_game_ids'

    if (-not (Test-NonBlankString $recordAppId) -or $recordAppId -notmatch '^\d+$') {
        $schemaErrors.Add('app_id must be a non-empty decimal string')
    }
    foreach ($field in @(
        @{ Name = 'game'; Value = $game },
        @{ Name = 'rights_owner'; Value = $rightsOwner },
        @{ Name = 'mode_restrictions'; Value = $modeRestrictions }
    )) {
        if (-not (Test-NonBlankString $field.Value)) {
            $schemaErrors.Add("$($field.Name) must be a non-empty string")
        }
    }
    # `intake_route` answers "where would a request be sent"; it is only actionable for
    # records that have not been decided yet. It stays optional rather than being padded
    # with invented contact routes for titles already covered by the unified decision.
    if ($null -ne $intakeRoute -and -not (Test-NonBlankString $intakeRoute)) {
        $schemaErrors.Add('intake_route must be a non-empty string or null')
    }
    # `note` is where the date semantics are spelled out for anyone reading one record in
    # isolation. It is optional, but a blank one is worse than none: it looks documented.
    $recordNote = Get-Field $Record 'note'
    if ($null -ne $recordNote -and -not (Test-NonBlankString $recordNote)) {
        $schemaErrors.Add('note must be a non-empty string or null')
    }
    if (-not (Test-NonBlankString $decision) -or $decision -notin $script:AllowedDecisions) {
        $schemaErrors.Add('decision is not an allowed value')
    }
    if ($null -eq $scopes) {
        $schemaErrors.Add('scopes object is required')
    }

    $normalizedCatalogGameIds = @()
    if ($null -eq $catalogGameIds) {
        $schemaErrors.Add('catalog_game_ids must be an array of catalog game ids (it may be empty)')
    } else {
        $seenCatalogGameIds = @{}
        foreach ($catalogGameId in $catalogGameIds) {
            if (-not (Test-NonBlankString $catalogGameId)) {
                $schemaErrors.Add('catalog_game_ids entries must be non-empty strings')
                continue
            }
            if ($seenCatalogGameIds.ContainsKey($catalogGameId)) {
                $schemaErrors.Add("catalog_game_ids contains a duplicate entry: $catalogGameId")
                continue
            }
            $seenCatalogGameIds[$catalogGameId] = $true
        }
        $normalizedCatalogGameIds = @($catalogGameIds | Where-Object { Test-NonBlankString $_ })
    }

    $scopeResults = [ordered]@{}
    foreach ($scopeName in $script:ScopeNames) {
        $scope = Get-Field $scopes $scopeName
        $approved = Get-Field $scope 'approved'
        $boundaries = Get-Field $scope 'boundaries'
        $scopeSchemaValid = $true
        if ($null -eq $scope) {
            $schemaErrors.Add("scopes.$scopeName is required")
            $scopeSchemaValid = $false
        } elseif ($approved -isnot [bool]) {
            $schemaErrors.Add("scopes.$scopeName.approved must be boolean")
            $scopeSchemaValid = $false
        } elseif ($approved -and -not (Test-NonBlankString $boundaries)) {
            $schemaErrors.Add("scopes.$scopeName.boundaries is required when approved")
            $scopeSchemaValid = $false
        } elseif (-not $approved -and $null -ne $boundaries -and $boundaries -isnot [string]) {
            $schemaErrors.Add("scopes.$scopeName.boundaries must be a string or null")
            $scopeSchemaValid = $false
        }

        $scopePass = $scopeSchemaValid -and ($approved -eq $true) -and (Test-NonBlankString $boundaries)
        if (-not $scopePass) { $gateReasons.Add("$scopeName scope is not approved with explicit boundaries") }
        $scopeResults[$scopeName] = [ordered]@{
            approved = ($approved -eq $true)
            boundaries = $boundaries
            pass = $scopePass
        }
    }

    if ($decision -notin $script:ApprovedDecisions) {
        $gateReasons.Add("decision '$decision' is not an approval")
    }
    if (-not (Test-OptionalEvidenceUrl $evidenceUrl)) {
        $gateReasons.Add('evidence_url must be absent, null, blank, or an absolute http(s) link')
    }
    if (-not (Test-NonBlankString $reviewer)) {
        $gateReasons.Add('reviewer is required')
    }

    $parsedEffective = [datetime]::MinValue
    if (-not (Test-IsoDate $effectiveDate ([ref]$parsedEffective))) {
        $gateReasons.Add('effective_date must use yyyy-MM-dd')
    } elseif ($parsedEffective -gt $EffectiveAsOf.Date) {
        $gateReasons.Add('approval is not effective yet')
    }

    $parsedReview = [datetime]::MinValue
    if (-not (Test-IsoDate $reviewDate ([ref]$parsedReview))) {
        $gateReasons.Add('expiry_or_review_date must use yyyy-MM-dd')
    } elseif ($parsedReview -lt $EffectiveAsOf.Date) {
        $gateReasons.Add('approval is expired or past its review date')
    }

    return [pscustomobject][ordered]@{
        app_id = $recordAppId
        game = $game
        decision = $decision
        catalog_game_ids = @($normalizedCatalogGameIds)
        schema_valid = ($schemaErrors.Count -eq 0)
        gate_pass = ($schemaErrors.Count -eq 0 -and $gateReasons.Count -eq 0)
        schema_errors = @($schemaErrors)
        gate_reasons = @($gateReasons)
        scopes = [pscustomobject]$scopeResults
        evidence_url = $evidenceUrl
        evidence_url_pending = (-not (Test-NonBlankString $evidenceUrl))
        reviewer = $reviewer
        effective_date = $effectiveDate
        expiry_or_review_date = $reviewDate
    }
}

# issue #855 §E.4: `shipped-games.json` is the single source of truth for catalog scope.
# Every shipped game_id must be covered by exactly one rights record; extra non-shipped
# records (DragonSword) may exist but never count towards the shipped set.
function Test-CatalogReconciliation {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][array]$Records,
        [AllowNull()]$ShippedGameIds,
        [hashtable]$ExpectedAppIdsByGameId = @{}
    )

    $errors = [Collections.Generic.List[string]]::new()
    if ($null -eq $ShippedGameIds) {
        $errors.Add('shipped games manifest could not be read; catalog reconciliation cannot run')
        return [pscustomobject][ordered]@{
            pass = $false
            errors = @($errors)
            shipped_game_count = 0
            covered_shipped_game_count = 0
            uncovered_shipped_game_ids = @()
            non_shipped_catalog_game_ids = @()
            non_shipped_record_app_ids = @()
            app_id_mismatches = @()
        }
    }

    $shipped = @($ShippedGameIds)
    $owners = @{}
    $nonShippedCatalogGameIds = [Collections.Generic.List[string]]::new()
    $nonShippedRecordAppIds = [Collections.Generic.List[string]]::new()
    $appIdMismatches = [Collections.Generic.List[string]]::new()

    foreach ($record in $Records) {
        $recordAppId = Get-Field $record 'app_id'
        $label = if (Test-NonBlankString $recordAppId) { $recordAppId } else { '<missing app_id>' }
        $catalogGameIds = Get-ArrayField $record 'catalog_game_ids'
        if ($null -eq $catalogGameIds) { continue }

        $shippedHits = 0
        foreach ($catalogGameId in $catalogGameIds) {
            if (-not (Test-NonBlankString $catalogGameId)) { continue }
            if ($catalogGameId -in $script:NonGameCatalogIds) {
                $errors.Add("catalog_game_ids must not reference the non-game catalog '$catalogGameId' (record $label)")
                continue
            }
            if ($catalogGameId -notin $shipped) {
                $nonShippedCatalogGameIds.Add($catalogGameId)
                $errors.Add("catalog_game_ids references '$catalogGameId', which is not in shipped-games.json (record $label)")
                continue
            }
            if ($ExpectedAppIdsByGameId.ContainsKey($catalogGameId)) {
                $expectedAppId = [string]$ExpectedAppIdsByGameId[$catalogGameId]
                if ($label -ne $expectedAppId) {
                    $appIdMismatches.Add($catalogGameId)
                    $errors.Add("shipped game '$catalogGameId' requires rights record app_id $expectedAppId, got $label")
                }
            }
            $shippedHits++
            if ($owners.ContainsKey($catalogGameId)) {
                $errors.Add("shipped game '$catalogGameId' is claimed by more than one rights record ($($owners[$catalogGameId]) and $label)")
            } else {
                $owners[$catalogGameId] = $label
            }
        }
        if ($shippedHits -eq 0) { $nonShippedRecordAppIds.Add($label) }
    }

    $uncovered = @($shipped | Where-Object { -not $owners.ContainsKey($_) })
    foreach ($missing in $uncovered) {
        $errors.Add("shipped game '$missing' has no rights record")
    }

    foreach ($pair in $script:SeparateRightsIdentities) {
        if ($owners.ContainsKey($pair.Left) -and $owners.ContainsKey($pair.Right) -and
            $owners[$pair.Left] -eq $owners[$pair.Right]) {
            $errors.Add("$($pair.Left) and $($pair.Right) share rights record $($owners[$pair.Left]): $($pair.Reason)")
        }
    }

    return [pscustomobject][ordered]@{
        pass = ($errors.Count -eq 0)
        errors = @($errors)
        shipped_game_count = $shipped.Count
        covered_shipped_game_count = $owners.Count
        uncovered_shipped_game_ids = @($uncovered)
        non_shipped_catalog_game_ids = @($nonShippedCatalogGameIds)
        non_shipped_record_app_ids = @($nonShippedRecordAppIds)
        app_id_mismatches = @($appIdMismatches)
    }
}

function Get-ShippedGameIds {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $shipped = Get-Content -Raw -Encoding UTF8 -LiteralPath $Path | ConvertFrom-Json
    $games = Get-ArrayField $shipped 'games'
    if ($null -eq $games) { return $null }
    return @($games | ForEach-Object { Get-Field $_ 'gameId' } | Where-Object { Test-NonBlankString $_ })
}

function Get-ShippedGameAppIdMap {
    param([Parameter(Mandatory = $true)][string]$Path)

    $result = @{}
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $result }
    $shipped = Get-Content -Raw -Encoding UTF8 -LiteralPath $Path | ConvertFrom-Json
    $games = Get-ArrayField $shipped 'games'
    if ($null -eq $games) { return $result }
    foreach ($game in $games) {
        if (-not (Test-HasProperty $game 'appId')) { continue }
        $gameId = Get-Field $game 'gameId'
        $appId = Get-Field $game 'appId'
        if (-not (Test-NonBlankString $gameId) -or -not (Test-NonBlankString $appId) -or $appId -notmatch '^\d+$') {
            throw 'shipped-games.json optional appId bindings require a non-empty gameId and decimal-string appId'
        }
        if ($result.ContainsKey($gameId)) { throw "duplicate shipped appId binding for gameId: $gameId" }
        $result[$gameId] = $appId
    }
    return $result
}

function New-RightsGateReport {
    param(
        [Parameter(Mandatory = $true)]$Manifest,
        [AllowNull()]$ShippedGameIds,
        [hashtable]$ExpectedAppIdsByGameId = @{},
        [Parameter(Mandatory = $true)][datetime]$EffectiveAsOf,
        [string[]]$SelectedAppIds = @(),
        [string]$ResolvedManifestPath = '',
        [bool]$ValidateOnlyMode = $false
    )

    $schemaErrors = [Collections.Generic.List[string]]::new()
    $unifiedDecision = Test-UnifiedRightsDecision (Get-Field $Manifest 'unified_rights_decision') $EffectiveAsOf
    foreach ($unifiedSchemaError in @($unifiedDecision.schema_errors)) {
        $schemaErrors.Add($unifiedSchemaError)
    }

    if ((Get-Field $Manifest 'schema_version') -ne $script:ManifestSchemaVersion) {
        $schemaErrors.Add("schema_version must equal $($script:ManifestSchemaVersion)")
    }
    foreach ($retired in $script:RetiredManifestKeys) {
        if (Test-HasProperty $Manifest $retired) {
            $schemaErrors.Add("retired policy '$retired' must not be present; it was superseded by unified_rights_decision")
        }
    }

    # No `@()` wrapper here: `Get-ArrayField` already returns the array as a single pipeline
    # item, and wrapping it again would nest it one level deeper.
    $allRecords = Get-ArrayField $Manifest 'records'
    if ($null -eq $allRecords) {
        $allRecords = @()
        $schemaErrors.Add('records must be an array')
    }
    if ($allRecords.Count -eq 0) { $schemaErrors.Add('records must not be empty') }

    $seenAppIds = @{}
    foreach ($record in $allRecords) {
        $recordAppId = Get-Field $record 'app_id'
        if (Test-NonBlankString $recordAppId) {
            if ($seenAppIds.ContainsKey($recordAppId)) {
                $schemaErrors.Add("duplicate app_id: $recordAppId")
            } else {
                $seenAppIds[$recordAppId] = $true
            }
        }
    }

    $reconciliation = Test-CatalogReconciliation -Records $allRecords -ShippedGameIds $ShippedGameIds -ExpectedAppIdsByGameId $ExpectedAppIdsByGameId
    foreach ($reconciliationError in @($reconciliation.errors)) {
        $schemaErrors.Add($reconciliationError)
    }

    $records = $allRecords
    if ($SelectedAppIds.Count -gt 0) {
        $unknown = @($SelectedAppIds | Where-Object { -not $seenAppIds.ContainsKey($_) })
        foreach ($unknownAppId in $unknown) { $schemaErrors.Add("app_id not found: $unknownAppId") }
        $records = @($allRecords | Where-Object { (Get-Field $_ 'app_id') -in $SelectedAppIds })
    }

    $results = @($records | ForEach-Object { Test-RightsRecord $_ $EffectiveAsOf })
    $recordSchemaValid = @($results | Where-Object { -not $_.schema_valid }).Count -eq 0
    $allApproved = $results.Count -gt 0 -and @($results | Where-Object { -not $_.gate_pass }).Count -eq 0
    $manifestValid = $schemaErrors.Count -eq 0 -and $recordSchemaValid

    $status = 'blocked'
    if (-not $manifestValid) { $status = 'invalid' }
    elseif ($allApproved) { $status = 'pass' }

    return [pscustomobject][ordered]@{
        schema_version = $script:ManifestSchemaVersion
        manifest_path = $ResolvedManifestPath
        as_of_date = $EffectiveAsOf.ToString('yyyy-MM-dd')
        mode = $(if ($ValidateOnlyMode) { 'validate_only' } else { 'require_approved' })
        status = $status
        manifest_valid = $manifestValid
        all_approved = $allApproved
        publisher_or_counsel_approval = $allApproved
        formal_delivery_permitted = $allApproved
        # The rights decision never speaks for the runtime. Anti-cheat stays a separate,
        # independently enforced red line (issue #855 §E, "明确不做").
        runtime_anti_cheat_gate_still_required = $true
        selected_record_count = $results.Count
        total_record_count = $allRecords.Count
        schema_errors = @($schemaErrors)
        unified_rights_decision = $unifiedDecision
        catalog_reconciliation = $reconciliation
        records = $results
    }
}

function New-SelfTestRecord {
    return [pscustomobject]@{
        app_id = '1'
        game = 'Fixture Game'
        rights_owner = 'Fixture Publisher'
        decision = 'counsel_approved'
        catalog_game_ids = @('fixture-game-pc')
        scopes = [pscustomobject]@{
            instrumentation = [pscustomobject]@{ approved = $true; boundaries = 'Offline user-mode D3D11 instrumentation only.' }
            capture = [pscustomobject]@{ approved = $true; boundaries = 'RGB, depth, projection and pose capture only.' }
            commercial_ml = [pscustomobject]@{ approved = $true; boundaries = 'Named internal commercial ML project only.' }
            delivery = [pscustomobject]@{ approved = $true; boundaries = 'Delivery to the named customer only.' }
        }
        mode_restrictions = 'Offline single-player; no anti-cheat may be active.'
        effective_date = '2026-01-01'
        expiry_or_review_date = '2026-12-31'
        evidence_url = 'https://example.invalid/approval/1'
        reviewer = 'Authorized Reviewer'
        intake_route = 'https://example.invalid/contact'
        note = 'Fixture record.'
    }
}

function New-SelfTestUnifiedDecision {
    return [pscustomobject]@{
        declared_by = 'Fixture Project Owner'
        declared_at = '2026-08-01'
        reference_url = 'https://example.invalid/issues/855'
        supersedes = @('project_research_only')
        note = 'Fixture unified decision.'
    }
}

function New-SelfTestManifest {
    param([Parameter(Mandatory = $true)][AllowEmptyCollection()][array]$Records)

    return [pscustomobject]@{
        schema_version = $script:ManifestSchemaVersion
        unified_rights_decision = New-SelfTestUnifiedDecision
        records = $Records
    }
}

function Assert-SelfTest {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )
    if (-not $Condition) { throw "self-test failed: $Message" }
}

function Write-JsonReport {
    param(
        [Parameter(Mandatory = $true)]$Report,
        [string]$Path
    )

    $json = $Report | ConvertTo-Json -Depth 12
    if ([string]::IsNullOrWhiteSpace($Path)) {
        Write-Output $json
        return
    }

    $absolute = [IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $absolute) { throw "refusing to overwrite report: $absolute" }
    $parent = Split-Path -Parent $absolute
    if (-not (Test-Path -LiteralPath $parent)) { throw "report parent does not exist: $parent" }
    [IO.File]::WriteAllText($absolute, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

if ($SelfTest) {
    $asOf = [datetime]'2026-08-10'
    $testCount = 0

    $approved = New-SelfTestRecord
    $approvedResult = Test-RightsRecord $approved $asOf
    Assert-SelfTest $approvedResult.schema_valid 'complete approval should be schema-valid'
    Assert-SelfTest $approvedResult.gate_pass 'complete approval should pass'
    $testCount += 2

    $partial = New-SelfTestRecord
    $partial.scopes.delivery.approved = $false
    $partial.scopes.delivery.boundaries = $null
    $partialResult = Test-RightsRecord $partial $asOf
    Assert-SelfTest $partialResult.schema_valid 'partial approval should remain schema-valid'
    Assert-SelfTest (-not $partialResult.gate_pass) 'partial approval must fail closed'
    $testCount += 2

    # issue #855 §E.1: an absent/null/blank evidence link no longer blocks; a present but
    # malformed link still does.
    foreach ($blankEvidence in @($null, '', '   ')) {
        $pendingEvidence = New-SelfTestRecord
        $pendingEvidence.evidence_url = $blankEvidence
        $pendingResult = Test-RightsRecord $pendingEvidence $asOf
        Assert-SelfTest $pendingResult.schema_valid 'pending evidence link should stay schema-valid'
        Assert-SelfTest $pendingResult.gate_pass 'complete approval with a pending evidence link must still pass'
        Assert-SelfTest $pendingResult.evidence_url_pending 'pending evidence link must be reported as pending'
        $testCount += 3
    }

    foreach ($brokenEvidence in @('not-a-url', 'ftp://example.invalid/approval', '/relative/path', 'example.invalid/approval')) {
        $brokenEvidenceRecord = New-SelfTestRecord
        $brokenEvidenceRecord.evidence_url = $brokenEvidence
        Assert-SelfTest (-not (Test-RightsRecord $brokenEvidenceRecord $asOf).gate_pass) "non-empty invalid evidence URL '$brokenEvidence' must fail closed"
        $testCount++
    }

    $expired = New-SelfTestRecord
    $expired.expiry_or_review_date = '2026-08-09'
    Assert-SelfTest (-not (Test-RightsRecord $expired $asOf).gate_pass) 'expired approval must fail closed'
    $testCount++

    $notYetEffective = New-SelfTestRecord
    $notYetEffective.effective_date = '2026-08-11'
    Assert-SelfTest (-not (Test-RightsRecord $notYetEffective $asOf).gate_pass) 'approval that is not yet effective must fail closed'
    $testCount++

    foreach ($blockedDecision in @('requested', 'not_requested', 'denied', 'expired')) {
        $blocked = New-SelfTestRecord
        $blocked.decision = $blockedDecision
        $blockedResult = Test-RightsRecord $blocked $asOf
        Assert-SelfTest $blockedResult.schema_valid "$blockedDecision should remain schema-valid"
        Assert-SelfTest (-not $blockedResult.gate_pass) "$blockedDecision is not an approval"
        $testCount += 2
    }

    $invalid = New-SelfTestRecord
    $invalid.decision = 'assumed_approved'
    Assert-SelfTest (-not (Test-RightsRecord $invalid $asOf).schema_valid) 'unknown decision must be rejected'
    $testCount++

    $missingReviewer = New-SelfTestRecord
    $missingReviewer.reviewer = $null
    Assert-SelfTest (-not (Test-RightsRecord $missingReviewer $asOf).gate_pass) 'missing reviewer must fail closed'
    $testCount++

    $approvedWithoutBoundaries = New-SelfTestRecord
    $approvedWithoutBoundaries.scopes.commercial_ml.boundaries = ' '
    Assert-SelfTest (-not (Test-RightsRecord $approvedWithoutBoundaries $asOf).schema_valid) 'approved scope without boundaries must be rejected'
    $testCount++

    # `intake_route` is only actionable before a decision exists, so it is optional now.
    $noIntakeRoute = New-SelfTestRecord
    $noIntakeRoute.intake_route = $null
    Assert-SelfTest ((Test-RightsRecord $noIntakeRoute $asOf).gate_pass) 'a decided record may omit the intake route'
    $testCount++

    $blankIntakeRoute = New-SelfTestRecord
    $blankIntakeRoute.intake_route = '   '
    Assert-SelfTest (-not (Test-RightsRecord $blankIntakeRoute $asOf).schema_valid) 'a blank intake route must be rejected'
    $testCount++

    $noNote = New-SelfTestRecord
    $noNote.note = $null
    Assert-SelfTest ((Test-RightsRecord $noNote $asOf).gate_pass) 'note is optional'
    $testCount++

    $blankNote = New-SelfTestRecord
    $blankNote.note = '   '
    Assert-SelfTest (-not (Test-RightsRecord $blankNote $asOf).schema_valid) 'a blank note must be rejected rather than pass as documentation'
    $testCount++

    $noCatalogIds = New-SelfTestRecord
    $noCatalogIds.PSObject.Properties.Remove('catalog_game_ids')
    Assert-SelfTest (-not (Test-RightsRecord $noCatalogIds $asOf).schema_valid) 'catalog_game_ids is required'
    $testCount++

    $scalarCatalogIds = New-SelfTestRecord
    $scalarCatalogIds.catalog_game_ids = 'fixture-game-pc'
    Assert-SelfTest (-not (Test-RightsRecord $scalarCatalogIds $asOf).schema_valid) 'catalog_game_ids must be an array'
    $testCount++

    $blankCatalogId = New-SelfTestRecord
    $blankCatalogId.catalog_game_ids = @('fixture-game-pc', '  ')
    Assert-SelfTest (-not (Test-RightsRecord $blankCatalogId $asOf).schema_valid) 'catalog_game_ids entries must be non-empty'
    $testCount++

    $duplicateCatalogId = New-SelfTestRecord
    $duplicateCatalogId.catalog_game_ids = @('fixture-game-pc', 'fixture-game-pc')
    Assert-SelfTest (-not (Test-RightsRecord $duplicateCatalogId $asOf).schema_valid) 'catalog_game_ids must not repeat an entry'
    $testCount++

    # DragonSword shape: a real rights record that owns no shipped catalog.
    $nonShipped = New-SelfTestRecord
    $nonShipped.catalog_game_ids = @()
    $nonShippedResult = Test-RightsRecord $nonShipped $asOf
    Assert-SelfTest $nonShippedResult.schema_valid 'a non-shipped rights record may carry an empty catalog_game_ids'
    Assert-SelfTest $nonShippedResult.gate_pass 'a non-shipped rights record still passes on its own merits'
    $testCount += 2

    # ---- catalog reconciliation (issue #855 §E.3/§E.4) ----
    $shippedFixture = @('alpha-pc', 'beta-pc', 'halo-infinite-steam-release-test', 'halo-reach-mcc-s0')
    $alpha = New-SelfTestRecord; $alpha.app_id = '11'; $alpha.game = 'Alpha'; $alpha.catalog_game_ids = @('alpha-pc')
    $beta = New-SelfTestRecord; $beta.app_id = '12'; $beta.game = 'Beta'; $beta.catalog_game_ids = @('beta-pc')
    $haloInfinite = New-SelfTestRecord; $haloInfinite.app_id = '13'; $haloInfinite.game = 'Halo Infinite'; $haloInfinite.catalog_game_ids = @('halo-infinite-steam-release-test')
    $haloMcc = New-SelfTestRecord; $haloMcc.app_id = '14'; $haloMcc.game = 'Halo: The Master Chief Collection'; $haloMcc.catalog_game_ids = @('halo-reach-mcc-s0')
    $extraNonShipped = New-SelfTestRecord; $extraNonShipped.app_id = '15'; $extraNonShipped.catalog_game_ids = @()
    $happyRecords = @($alpha, $beta, $haloInfinite, $haloMcc, $extraNonShipped)

    $expectedFixtureAppIds = @{ 'alpha-pc' = '11' }
    $happy = Test-CatalogReconciliation -Records $happyRecords -ShippedGameIds $shippedFixture -ExpectedAppIdsByGameId $expectedFixtureAppIds
    Assert-SelfTest $happy.pass 'full shipped coverage plus an extra non-shipped record must reconcile'
    Assert-SelfTest ($happy.covered_shipped_game_count -eq 4) 'reconciliation must count every shipped game exactly once'
    Assert-SelfTest ($happy.non_shipped_record_app_ids -contains '15') 'an extra non-shipped record must be reported, not counted'
    $testCount += 3

    $mislinkedAlpha = New-SelfTestRecord
    $mislinkedAlpha.app_id = '20'
    $mislinkedAlpha.game = 'DragonSword: Awakening'
    $mislinkedAlpha.catalog_game_ids = @('alpha-pc')
    $mislinked = Test-CatalogReconciliation -Records @($mislinkedAlpha, $beta, $haloInfinite, $haloMcc) -ShippedGameIds $shippedFixture -ExpectedAppIdsByGameId $expectedFixtureAppIds
    Assert-SelfTest (-not $mislinked.pass) 'a catalog id pasted onto an unrelated AppID/title must fail reconciliation'
    Assert-SelfTest ($mislinked.app_id_mismatches -contains 'alpha-pc') 'a cross-AppID catalog link must be reported explicitly'
    $testCount += 2

    $uncoveredRecords = @($alpha, $haloInfinite, $haloMcc)
    Assert-SelfTest (-not (Test-CatalogReconciliation -Records $uncoveredRecords -ShippedGameIds $shippedFixture).pass) 'an uncovered shipped game must fail'
    $testCount++

    $doubleClaim = New-SelfTestRecord; $doubleClaim.app_id = '16'; $doubleClaim.game = 'Alpha'; $doubleClaim.catalog_game_ids = @('alpha-pc')
    Assert-SelfTest (-not (Test-CatalogReconciliation -Records @($alpha, $beta, $haloInfinite, $haloMcc, $doubleClaim) -ShippedGameIds $shippedFixture).pass) 'a shipped game claimed twice must fail'
    $testCount++

    $unknownCatalog = New-SelfTestRecord; $unknownCatalog.app_id = '17'; $unknownCatalog.catalog_game_ids = @('not-a-shipped-game')
    Assert-SelfTest (-not (Test-CatalogReconciliation -Records @($alpha, $beta, $haloInfinite, $haloMcc, $unknownCatalog) -ShippedGameIds $shippedFixture).pass) 'an unknown catalog game id must fail'
    $testCount++

    $syntheticRecord = New-SelfTestRecord; $syntheticRecord.app_id = '18'; $syntheticRecord.catalog_game_ids = @('synthetic')
    Assert-SelfTest (-not (Test-CatalogReconciliation -Records @($alpha, $beta, $haloInfinite, $haloMcc, $syntheticRecord) -ShippedGameIds $shippedFixture).pass) 'synthetic must never receive a rights record'
    $testCount++

    $mergedHalo = New-SelfTestRecord
    $mergedHalo.app_id = '19'
    $mergedHalo.game = 'Halo'
    $mergedHalo.catalog_game_ids = @('halo-infinite-steam-release-test', 'halo-reach-mcc-s0')
    Assert-SelfTest (-not (Test-CatalogReconciliation -Records @($alpha, $beta, $mergedHalo) -ShippedGameIds $shippedFixture).pass) 'Halo Infinite and Halo MCC must not share one rights record'
    $testCount++

    Assert-SelfTest (-not (Test-CatalogReconciliation -Records $happyRecords -ShippedGameIds $null).pass) 'an unreadable shipped-games manifest must fail closed'
    $testCount++

    # ---- manifest level ----
    $goodManifest = New-SelfTestManifest -Records $happyRecords
    $goodReport = New-RightsGateReport -Manifest $goodManifest -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf
    Assert-SelfTest $goodReport.manifest_valid 'a complete manifest should be valid'
    Assert-SelfTest ($goodReport.status -eq 'pass') 'a fully approved manifest should pass'
    $testCount += 2

    # The retired research-only lane must not be able to come back to life alongside the
    # unified decision (issue #855 §E.5).
    $retiredLaneManifest = New-SelfTestManifest -Records $happyRecords
    $retiredLaneManifest | Add-Member -NotePropertyName 'project_research_only' -NotePropertyValue ([pscustomobject]@{ decision = 'project_owner_research_only' })
    $retiredLaneReport = New-RightsGateReport -Manifest $retiredLaneManifest -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf
    Assert-SelfTest (-not $retiredLaneReport.manifest_valid) 'an active project_research_only block must be rejected'
    Assert-SelfTest ($retiredLaneReport.status -eq 'invalid') 'a manifest carrying the retired lane must be invalid'
    $testCount += 2

    $noUnifiedDecision = New-SelfTestManifest -Records $happyRecords
    $noUnifiedDecision.PSObject.Properties.Remove('unified_rights_decision')
    Assert-SelfTest (-not (New-RightsGateReport -Manifest $noUnifiedDecision -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf).manifest_valid) 'unified_rights_decision is required'
    $testCount++

    $unrecordedSupersede = New-SelfTestManifest -Records $happyRecords
    $unrecordedSupersede.unified_rights_decision.supersedes = @()
    Assert-SelfTest (-not (New-RightsGateReport -Manifest $unrecordedSupersede -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf).manifest_valid) 'the retired lane must be recorded in supersedes'
    $testCount++

    $wrongSchemaVersion = New-SelfTestManifest -Records $happyRecords
    $wrongSchemaVersion.schema_version = 1
    Assert-SelfTest (-not (New-RightsGateReport -Manifest $wrongSchemaVersion -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf).manifest_valid) 'the previous schema version must be rejected'
    $testCount++

    $duplicateAppId = New-SelfTestRecord; $duplicateAppId.app_id = '11'; $duplicateAppId.catalog_game_ids = @()
    $duplicateManifest = New-SelfTestManifest -Records @($happyRecords + $duplicateAppId)
    Assert-SelfTest (-not (New-RightsGateReport -Manifest $duplicateManifest -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf).manifest_valid) 'duplicate app_ids must be rejected'
    $testCount++

    $deniedAlpha = New-SelfTestRecord
    $deniedAlpha.app_id = '11'
    $deniedAlpha.catalog_game_ids = @('alpha-pc')
    $deniedAlpha.decision = 'denied'
    $blockedManifest = New-SelfTestManifest -Records @($deniedAlpha, $beta, $haloInfinite, $haloMcc, $extraNonShipped)
    $blockedReport = New-RightsGateReport -Manifest $blockedManifest -ShippedGameIds $shippedFixture -EffectiveAsOf $asOf
    Assert-SelfTest $blockedReport.manifest_valid 'a denied record keeps the manifest structurally valid'
    Assert-SelfTest ($blockedReport.status -eq 'blocked') 'a denied record must block the default gate'
    Assert-SelfTest (-not $blockedReport.publisher_or_counsel_approval) 'a denied record must not report approval'
    Assert-SelfTest $blockedReport.runtime_anti_cheat_gate_still_required 'the runtime anti-cheat gate is never satisfied by a rights decision'
    $testCount += 4

    Write-JsonReport ([pscustomobject][ordered]@{
        status = 'pass'
        tests = $testCount
        as_of_date = $asOf.ToString('yyyy-MM-dd')
    }) $OutputPath
    exit 0
}

if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    throw "rights manifest not found: $ManifestPath"
}

$manifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $ManifestPath | ConvertFrom-Json
$shippedGameIds = Get-ShippedGameIds -Path $ShippedGamesPath
$shippedGameAppIds = Get-ShippedGameAppIdMap -Path $ShippedGamesPath

$report = New-RightsGateReport `
    -Manifest $manifest `
    -ShippedGameIds $shippedGameIds `
    -ExpectedAppIdsByGameId $shippedGameAppIds `
    -EffectiveAsOf $AsOfDate `
    -SelectedAppIds $AppId `
    -ResolvedManifestPath ([IO.Path]::GetFullPath($ManifestPath)) `
    -ValidateOnlyMode ([bool]$ValidateOnly)

Write-JsonReport $report $OutputPath
if (-not $report.manifest_valid) { exit 1 }
if (-not $ValidateOnly -and -not $report.all_approved) { exit 2 }
exit 0

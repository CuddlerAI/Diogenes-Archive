[CmdletBinding()]
param(
    [string]$UserSettingsPath = (Join-Path $env:LOCALAPPDATA 'CD Projekt Red\Cyberpunk 2077\UserSettings.json'),
    [int]$ExpectedSettingsVersion = 140,
    [ValidateSet('formal', 'metric_calibration')]
    [string]$Purpose = 'formal'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$defaultUserSettingsPath = Join-Path $env:LOCALAPPDATA 'CD Projekt Red\Cyberpunk 2077\UserSettings.json'
$isLiveSettingsPath = [IO.Path]::GetFullPath($UserSettingsPath) -eq [IO.Path]::GetFullPath($defaultUserSettingsPath)

function Assert-LiveSettingsWritable {
    if ($isLiveSettingsPath -and (Get-Process -Name Cyberpunk2077 -ErrorAction SilentlyContinue)) {
        throw 'Cyberpunk 2077 is running. Save and exit normally before applying capture settings.'
    }
}

Assert-LiveSettingsWritable
if (-not (Test-Path -LiteralPath $UserSettingsPath -PathType Leaf)) {
    throw "CP2077 UserSettings.json not found: $UserSettingsPath"
}

function Find-Setting {
    param(
        [Parameter(Mandatory = $true)][object]$Document,
        [Parameter(Mandatory = $true)][string]$Group,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $groups = @($Document.data | Where-Object { $_.group_name -eq $Group })
    if ($groups.Count -ne 1) {
        throw "expected exactly one settings group '$Group', found $($groups.Count)"
    }
    $options = @($groups[0].options | Where-Object { $_.name -eq $Name })
    if ($options.Count -ne 1) {
        throw "expected exactly one setting '$Group/$Name', found $($options.Count)"
    }
    return $options[0]
}

function Set-SettingValue {
    param(
        [Parameter(Mandatory = $true)][object]$Document,
        [Parameter(Mandatory = $true)][string]$Group,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][object]$Value,
        [int]$Index = -1
    )
    $setting = Find-Setting -Document $Document -Group $Group -Name $Name
    $script:settingReplacements.Add([pscustomobject]@{
        Group = $Group
        Name = $Name
        Value = $Value
        Index = $Index
        HasIndex = $null -ne $setting.PSObject.Properties['index']
    })
}

$rawSettingsText = [IO.File]::ReadAllText($UserSettingsPath, [Text.UTF8Encoding]::new($false))
$settings = $rawSettingsText | ConvertFrom-Json
$settingReplacements = [Collections.Generic.List[object]]::new()
if ([int]$settings.version -ne $ExpectedSettingsVersion) {
    throw "settings version changed: expected $ExpectedSettingsVersion, actual $($settings.version); re-audit before writing"
}

# Native, stable render path for RGB/depth alignment.
Set-SettingValue $settings '/video/display' 'Resolution' '1920x1080' 15
# The WGC delivery timeline is fixed at 30 fps and the exact-Present depth
# stream is sampled at 15 Hz.  Cap the game itself at 30 fps so every depth
# Present is representable in the RGB timeline; an uncapped ~20 fps run was
# observed to leave discrete valid depth Presents with no corresponding RGB
# frame even though both individual streams passed their own QC.
Set-SettingValue $settings '/video/display' 'MaximumFPS_OnOff' $true
Set-SettingValue $settings '/video/display' 'MaximumFPS_Value' 32
Set-SettingValue $settings '/graphics/presets' 'ResolutionScaling' 'Off' 0
Set-SettingValue $settings '/graphics/presets' 'DynamicResolutionScaling' $false
Set-SettingValue $settings '/graphics/presets' 'FrameGeneration' 'Off' 0
Set-SettingValue $settings '/graphics/presets' 'DLSSFrameGen' $false
Set-SettingValue $settings '/graphics/presets' 'FSR3_FrameGeneration' $false
Set-SettingValue $settings '/graphics/presets' 'XESS_FrameGeneration' $false
Set-SettingValue $settings '/graphics/raytracing' 'RayTracing' $false

# Keep enough GPU headroom for the pinned 30 fps Present cadence while retaining
# native 1080p geometry. These options change shading quality only; resolution
# scaling, dynamic resolution and generated frames remain prohibited above.
Set-SettingValue $settings '/graphics/advanced' 'ShadowMeshQuality' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'LocalShadowsQuality' 'Low' 1
Set-SettingValue $settings '/graphics/advanced' 'CascadedShadowsRange' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'CascadedShadowsResolution' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'DistantShadowsResolution' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'VolumetricFogResolution' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'VolumetricCloudsQuality' 'Off' 0
Set-SettingValue $settings '/graphics/advanced' 'ScreenSpaceReflectionsQuality' 'Off' 0
Set-SettingValue $settings '/graphics/advanced' 'AmbientOcclusion' 'Off' 0
Set-SettingValue $settings '/graphics/advanced' 'MirrorQuality' 'Low' 0
Set-SettingValue $settings '/graphics/advanced' 'LODPreset' 'Low' 0

# PR #727 FOV/HUD recipe and image-space effects. Metric calibration is the
# one explicit exception that needs the objective distance marker on screen.
# Its distinct attestation can never be accepted by the formal PR #727 gate.
Set-SettingValue $settings '/graphics/basic' 'FieldOfView' ([double]75.0)
Set-SettingValue $settings '/interface/hud' 'hud_presets' $(if ($Purpose -eq 'metric_calibration') { 'WholeHUDVisible' } else { 'WholeHUDHidden' }) $(if ($Purpose -eq 'metric_calibration') { 0 } else { 1 })
# The preset token does not apply its individual switches when edited on disk.
# Pin the persisted components too; the game otherwise kept its minimap/quest HUD.
foreach ($component in @('minimap', 'healthbar', 'stamina_oxygen', 'npc_healthbar', 'ammo_counter', 'input_hints', 'action_buttons', 'activity_log', 'quest_tracker', 'johnny_hud', 'object_markers', 'npc_names', 'wanted_level', 'npc_nameplates', 'crouch_indicator', 'vehicle_hud')) {
    Set-SettingValue $settings '/interface/hud' $component ($Purpose -eq 'metric_calibration')
}
Set-SettingValue $settings '/interface/hud' 'crosshairs' $false
Set-SettingValue $settings '/interface/hud' 'hud_markers' ($Purpose -eq 'metric_calibration')
Set-SettingValue $settings '/interface' 'HitIndicator' $false
Set-SettingValue $settings '/gameplay/misc' 'Tutorials' $false

# PR #727 requires every independently persisted subtitle surface to be off.
# Overheads is the live setting that controls NPC/background chatter text; the
# two ForceKiroshi flags must also remain false so accessibility overrides do
# not re-enable cinematic or overhead text.
Set-SettingValue $settings '/accessibility/subtitles' 'Cinematic' $false
Set-SettingValue $settings '/accessibility/subtitles' 'Radio' $false
Set-SettingValue $settings '/accessibility/subtitles' 'Overheads' $false
Set-SettingValue $settings '/accessibility/subtitles' 'CinematicForceKiroshiTexts' $false
Set-SettingValue $settings '/accessibility/subtitles' 'OverheadsForceKiroshiTexts' $false
Set-SettingValue $settings '/accessibility/subtitles' 'BackgroundOpacity' ([double]0.0)

# Replace dispatcher/fixer holocalls with text notifications, as required by
# the PR #727 recording recipe. The capture route still avoids phone UI.
Set-SettingValue $settings '/gameplay/muteholocalls' 'ncpd_dispatcher' $true
Set-SettingValue $settings '/gameplay/muteholocalls' 'fixer_briefs_and_debriefs' $true

Set-SettingValue $settings '/graphics/basic' 'FilmGrain' $false
Set-SettingValue $settings '/graphics/basic' 'ChromaticAberration' $false
Set-SettingValue $settings '/graphics/basic' 'DepthOfField' $false
Set-SettingValue $settings '/graphics/basic' 'LensFlares' $false
Set-SettingValue $settings '/graphics/basic' 'MotionBlur' 'Off' 0
Set-SettingValue $settings '/graphics/basic' 'Vignette' $false

$directory = Split-Path -Parent ([IO.Path]::GetFullPath($UserSettingsPath))
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$nameStem = $(if ($Purpose -eq 'metric_calibration') { 'metric-calibration' } else { 'issue735' })
$backupPath = Join-Path $directory "UserSettings.$nameStem-before-$stamp.json"
$temporaryPath = Join-Path $directory "UserSettings.$nameStem-write-$stamp.tmp"
$snapshotPath = Join-Path $directory "UserSettings.$nameStem-applied-$stamp.json"
$attestationPath = Join-Path $directory "UserSettings.$nameStem-attestation-$stamp.json"
if ((Test-Path -LiteralPath $backupPath) -or
    (Test-Path -LiteralPath $temporaryPath) -or
    (Test-Path -LiteralPath $snapshotPath) -or
    (Test-Path -LiteralPath $attestationPath)) {
    throw 'refusing to overwrite an existing issue735 settings backup, snapshot, attestation, or temporary file'
}

function ConvertTo-NativeJsonLiteral {
    param([Parameter(Mandatory = $true)][object]$Value)
    if ($Value -is [bool]) { return $(if ($Value) { 'true' } else { 'false' }) }
    if ($Value -is [double] -or $Value -is [single] -or $Value -is [decimal]) {
        return ([double]$Value).ToString('0.0###############', [Globalization.CultureInfo]::InvariantCulture)
    }
    if ($Value -is [string]) { return ($Value | ConvertTo-Json -Compress) }
    return ([Convert]::ToString($Value, [Globalization.CultureInfo]::InvariantCulture))
}

# Preserve CP2077's native typed JSON representation. PowerShell's full-object
# serializer rewrites every option and collapses float tokens such as 75.0 to
# integer-looking 75, which the game's typed settings loader reports as corrupt.
$json = $rawSettingsText
$jsonValuePattern = '"(?:\\.|[^"\\])*"|true|false|null|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?'
foreach ($replacement in $settingReplacements) {
    $escapedName = [regex]::Escape($replacement.Name)
    $optionPattern = '(?s)\{\s*"name"\s*:\s*"' + $escapedName + '"\s*,.*?\n\s*\}'
    $matches = [regex]::Matches($json, $optionPattern)
    if ($matches.Count -ne 1) {
        throw "expected exactly one native JSON option block for '$($replacement.Group)/$($replacement.Name)', found $($matches.Count)"
    }
    $block = $matches[0].Value
    $valuePattern = '("value"\s*:\s*)(' + $jsonValuePattern + ')'
    if ([regex]::Matches($block, $valuePattern).Count -ne 1) {
        throw "expected exactly one native JSON value for '$($replacement.Group)/$($replacement.Name)'"
    }
    $literal = ConvertTo-NativeJsonLiteral $replacement.Value
    $updated = [regex]::Replace($block, $valuePattern, ('${1}' + $literal), 1)
    if ($replacement.Index -ge 0 -and $replacement.HasIndex) {
        $indexPattern = '("index"\s*:\s*)(' + $jsonValuePattern + ')'
        if ([regex]::Matches($updated, $indexPattern).Count -ne 1) {
            throw "expected exactly one native JSON index for '$($replacement.Group)/$($replacement.Name)'"
        }
        $updated = [regex]::Replace($updated, $indexPattern, ('${1}' + $replacement.Index), 1)
    }
    $json = $json.Substring(0, $matches[0].Index) + $updated + $json.Substring($matches[0].Index + $matches[0].Length)
}
$settingsBytes = [Text.UTF8Encoding]::new($false).GetBytes($json)
[IO.File]::WriteAllBytes($temporaryPath, $settingsBytes)
try {
    # Recheck immediately before the only operation that mutates the live
    # settings file. The earlier check protects the read/authoring phase; this
    # check refuses a game launch that occurred while the replacement document
    # was being prepared.
    Assert-LiveSettingsWritable
    # File.Replace keeps the original as a recoverable backup and swaps the new
    # document atomically on the same volume.
    [IO.File]::Replace($temporaryPath, $UserSettingsPath, $backupPath, $true)
} finally {
    if (Test-Path -LiteralPath $temporaryPath) {
        Remove-Item -LiteralPath $temporaryPath -Force
    }
}

$reportText = & (Join-Path $PSScriptRoot 'Test-Cp2077CaptureSettings.ps1') -UserSettingsPath $UserSettingsPath -Purpose $Purpose
$report = $reportText | ConvertFrom-Json
if (-not $report.settings_passed) {
    throw 'capture settings were written but post-write settings validation failed; restore the reported backup'
}

$sha256 = [Security.Cryptography.SHA256]::Create()
try {
    $settingsHash = 'sha256:' + ([BitConverter]::ToString($sha256.ComputeHash($settingsBytes))).Replace('-', '').ToLowerInvariant()
} finally {
    $sha256.Dispose()
}

# These two create-new files bind the exact settings document to the next game
# process. The runtime coordinator verifies their hash and filesystem times; it
# never trusts a stale snapshot from an earlier launch.
$snapshotStream = [IO.File]::Open($snapshotPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $snapshotStream.Write($settingsBytes, 0, $settingsBytes.Length)
    $snapshotStream.Flush($true)
} finally {
    $snapshotStream.Dispose()
}
$attestation = [pscustomobject][ordered]@{
    schema = $(if ($Purpose -eq 'metric_calibration') { 'dm-cp2077-metric-calibration-settings-attestation/v1' } else { 'dm-cp2077-capture-settings-attestation/v1' })
    purpose = $Purpose
    settings_version = [int]$settings.version
    applied_utc = [DateTimeOffset]::UtcNow.ToString('O')
    settings_snapshot_file_name = Split-Path -Leaf $snapshotPath
    settings_sha256 = $settingsHash
    live_settings_last_write_100ns = [IO.File]::GetLastWriteTimeUtc($UserSettingsPath).ToFileTimeUtc()
    settings_passed = $true
    settings_ready = $true
    recording_ready = $false
}
$attestationBytes = [Text.UTF8Encoding]::new($false).GetBytes(($attestation | ConvertTo-Json -Depth 4) + [Environment]::NewLine)
$attestationStream = [IO.File]::Open($attestationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $attestationStream.Write($attestationBytes, 0, $attestationBytes.Length)
    $attestationStream.Flush($true)
} finally {
    $attestationStream.Dispose()
}

# If the game was launched during the post-write validation/attestation phase,
# fail instead of emitting an apparently usable apply report. The coordinator's
# process-time checks independently reject an after-launch attestation.
Assert-LiveSettingsWritable

[pscustomobject][ordered]@{
    schema_version = 'dm-cp2077-capture-settings-apply-1'
    purpose = $Purpose
    settings_applied = $true
    settings_passed = $report.settings_passed
    settings_ready = $report.settings_ready
    recording_ready = $report.recording_ready
    status = $report.status
    backup_file_name = Split-Path -Leaf $backupPath
    applied_snapshot_file_name = Split-Path -Leaf $snapshotPath
    attestation_file_name = Split-Path -Leaf $attestationPath
    settings_sha256 = $settingsHash
    note = $(if ($Purpose -eq 'metric_calibration') { 'The objective HUD marker is visible only for this local metric-calibration run. Formal capture must reapply the hidden-HUD recipe.' } else { 'Only audited capture graphics, HUD, subtitle, and call-display settings changed; Steam, network, cloud-save, telemetry, and save-game settings were not modified.' })
} | ConvertTo-Json -Depth 4

if (-not $report.settings_passed) { exit 2 }
exit 0

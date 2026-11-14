[CmdletBinding()]
param(
    [string]$UserSettingsPath = (Join-Path $env:LOCALAPPDATA 'CD Projekt Red\Cyberpunk 2077\UserSettings.json'),
    [ValidateSet('formal', 'metric_calibration')]
    [string]$Purpose = 'formal',
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

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

function Add-ExpectedSetting {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][Collections.Generic.List[object]]$Checks,
        [Parameter(Mandatory = $true)][object]$Document,
        [Parameter(Mandatory = $true)][string]$Group,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][object]$Expected
    )

    $setting = Find-Setting -Document $Document -Group $Group -Name $Name
    $actual = $setting.value
    $passed = if ($Expected -is [double]) {
        [Math]::Abs(([double]$actual) - $Expected) -le 0.001
    } else {
        $actual -ceq $Expected
    }
    $Checks.Add([pscustomobject][ordered]@{
        group = $Group
        name = $Name
        passed = $passed
        actual = $actual
        expected = $Expected
    })
    # String-list settings persist both a value and an index. The game can use
    # the index on restart, so checking only the text accepts a setting that
    # silently changes next launch (LocalShadowsQuality: Off=0, Low=1).
    # Some display lists (such as Resolution) omit their runtime-generated
    # choices from UserSettings.json; only compare choices present in the file.
    if ($setting.type -eq 'string_list' -and $null -ne $setting.PSObject.Properties['values']) {
        $index = [int]$setting.index
        $values = @($setting.values)
        $indexValid = $index -ge 0 -and $index -lt $values.Count
        $indexedValue = if ($indexValid) { $values[$index] } else { $null }
        $Checks.Add([pscustomobject][ordered]@{
            group = $Group
            name = $Name + '_value_index_consistency'
            passed = $indexValid -and $indexedValue -ceq $Expected
            actual = $indexedValue
            expected = $Expected
        })
    }
}

if (-not (Test-Path -LiteralPath $UserSettingsPath -PathType Leaf)) {
    throw "CP2077 UserSettings.json not found: $UserSettingsPath"
}

$settings = Get-Content -Raw -Encoding UTF8 -LiteralPath $UserSettingsPath | ConvertFrom-Json
$checks = [Collections.Generic.List[object]]::new()

# Geometry/depth stability: render at the delivery resolution without temporal
# upscaling, dynamic resolution, generated frames, or ray tracing.
Add-ExpectedSetting $checks $settings '/video/display' 'Resolution' '1920x1080'
Add-ExpectedSetting $checks $settings '/video/display' 'MaximumFPS_OnOff' $true
Add-ExpectedSetting $checks $settings '/video/display' 'MaximumFPS_Value' 32
Add-ExpectedSetting $checks $settings '/graphics/presets' 'ResolutionScaling' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/presets' 'DynamicResolutionScaling' $false
Add-ExpectedSetting $checks $settings '/graphics/presets' 'FrameGeneration' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/presets' 'DLSSFrameGen' $false
Add-ExpectedSetting $checks $settings '/graphics/presets' 'FSR3_FrameGeneration' $false
Add-ExpectedSetting $checks $settings '/graphics/presets' 'XESS_FrameGeneration' $false
Add-ExpectedSetting $checks $settings '/graphics/raytracing' 'RayTracing' $false
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'ShadowMeshQuality' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'LocalShadowsQuality' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'CascadedShadowsRange' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'CascadedShadowsResolution' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'DistantShadowsResolution' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'VolumetricFogResolution' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'VolumetricCloudsQuality' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'ScreenSpaceReflectionsQuality' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'AmbientOcclusion' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'MirrorQuality' 'Low'
Add-ExpectedSetting $checks $settings '/graphics/advanced' 'LODPreset' 'Low'

# PR #727 capture recipe. Metric calibration deliberately keeps the objective
# HUD marker visible; its distinct attestation can never satisfy the formal
# hidden-HUD recording gate.
Add-ExpectedSetting $checks $settings '/graphics/basic' 'FieldOfView' ([double]75.0)
Add-ExpectedSetting $checks $settings '/interface/hud' 'hud_presets' $(if ($Purpose -eq 'metric_calibration') { 'WholeHUDVisible' } else { 'WholeHUDHidden' })
foreach ($component in @('minimap', 'healthbar', 'stamina_oxygen', 'npc_healthbar', 'ammo_counter', 'input_hints', 'action_buttons', 'activity_log', 'quest_tracker', 'johnny_hud', 'object_markers', 'npc_names', 'wanted_level', 'npc_nameplates', 'crouch_indicator', 'vehicle_hud')) {
    Add-ExpectedSetting $checks $settings '/interface/hud' $component ($Purpose -eq 'metric_calibration')
}
Add-ExpectedSetting $checks $settings '/interface/hud' 'crosshairs' $false
Add-ExpectedSetting $checks $settings '/interface/hud' 'hud_markers' ($Purpose -eq 'metric_calibration')
Add-ExpectedSetting $checks $settings '/interface' 'HitIndicator' $false
Add-ExpectedSetting $checks $settings '/gameplay/misc' 'Tutorials' $false

# Subtitle surfaces from PR #727: cinematic, radio, and Overheads (the live
# option that owns NPC/background-chatter text), plus both forced-text
# overrides. Opacity is pinned to zero as a fail-closed residual-UI guard even
# though all emitting booleans are off.
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'Cinematic' $false
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'Radio' $false
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'Overheads' $false
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'CinematicForceKiroshiTexts' $false
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'OverheadsForceKiroshiTexts' $false
Add-ExpectedSetting $checks $settings '/accessibility/subtitles' 'BackgroundOpacity' ([double]0.0)

# These true values mean "mute holocall and use a text notification" in the
# game's dedicated muteholocalls group; phone UI remains prohibited in-run.
Add-ExpectedSetting $checks $settings '/gameplay/muteholocalls' 'ncpd_dispatcher' $true
Add-ExpectedSetting $checks $settings '/gameplay/muteholocalls' 'fixer_briefs_and_debriefs' $true

# Avoid image-space effects that obscure RGB/depth alignment or stable features.
Add-ExpectedSetting $checks $settings '/graphics/basic' 'FilmGrain' $false
Add-ExpectedSetting $checks $settings '/graphics/basic' 'ChromaticAberration' $false
Add-ExpectedSetting $checks $settings '/graphics/basic' 'DepthOfField' $false
Add-ExpectedSetting $checks $settings '/graphics/basic' 'LensFlares' $false
Add-ExpectedSetting $checks $settings '/graphics/basic' 'MotionBlur' 'Off'
Add-ExpectedSetting $checks $settings '/graphics/basic' 'Vignette' $false

$failed = @($checks | Where-Object { -not $_.passed })
$report = [pscustomobject][ordered]@{
    schema_version = 'dm-cp2077-capture-settings-report-1'
    purpose = $Purpose
    settings_version = $settings.version
    settings_passed = $failed.Count -eq 0
    failed_check_count = $failed.Count
    required_depth_backend = 'd3d12'
    depth_backend_gate = 'validated_separately'
    settings_ready = $failed.Count -eq 0
    recording_ready = $false
    passed = $failed.Count -eq 0
    status = if ($failed.Count -ne 0) {
        'blocked_capture_settings'
    } elseif ($Purpose -eq 'metric_calibration') {
        'metric_calibration_settings_ready_hud_visible'
    } else {
        'settings_ready_depth_runtime_separately_gated'
    }
    checks = @($checks)
    restrictions = @(
        'This report intentionally excludes the absolute settings path and device identifiers.',
        'D3D12 Depth, frame identity, pose, projection, metric scale, E-geo, and Loop are validated by separate fail-closed gates.',
        $(if ($Purpose -eq 'metric_calibration') { 'Visible HUD is allowed only for the local metric-calibration run and is rejected by the formal recording gate.' } else { 'Formal capture requires the hidden-HUD recipe.' }),
        'A settings pass does not prove that Depth, camera pose, player pose, or frame identity capture is ready.'
    )
}

$json = $report | ConvertTo-Json -Depth 8
Write-Output $json
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $absoluteOutput = [IO.Path]::GetFullPath($OutputPath)
    if (Test-Path -LiteralPath $absoluteOutput) {
        throw "refusing to overwrite report: $absoluteOutput"
    }
    $parent = Split-Path -Parent $absoluteOutput
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
        [IO.Directory]::CreateDirectory($parent) | Out-Null
    }
    [IO.File]::WriteAllText($absoluteOutput, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

if (-not $report.settings_ready) { exit 2 }
exit 0

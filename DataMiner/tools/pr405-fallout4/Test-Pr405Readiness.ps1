[CmdletBinding()]
param(
    [string]$DepthProfilePath = '',
    [string]$CameraBindingPath = '',
    [string]$PerfCertificationManifestPath = '',
    [string]$ProbeRuntimeManifestPath = '',
    [string]$PerfCertificationKeyPath = '',
    [string]$PerfHardwarePath = '',
    [string]$PlayerTelemetryPath = '',
    [string]$ArtifactRoot = '',
    [string]$FalloutPath = '',
    [string]$FalloutPrefsPath = '',
    [string]$ExpectedFalloutHash = '428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$probeConfigPath = Join-Path $env:ProgramData 'DataMiner\depth-lab\probe-config.json'
if (-not $FalloutPrefsPath) {
    $FalloutPrefsPath = Join-Path $env:USERPROFILE 'Documents\My Games\Fallout4\Fallout4Prefs.ini'
}
if (-not $FalloutPath) {
    $FalloutPath = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Steam\steamapps\common\Fallout 4\Fallout4.exe'
}
if (-not $DepthProfilePath) {
    $DepthProfilePath = Join-Path $dataMinerRoot 'config\catalogs\dev\fallout4-steam-release-test\depth-profile.json'
}
if (-not $CameraBindingPath) {
    $CameraBindingPath = Join-Path $dataMinerRoot 'config\catalogs\dev\fallout4-steam-release-test\camera-binding.json'
}
if (-not $ArtifactRoot) {
    $ArtifactRoot = Join-Path $dataMinerRoot 'target\release'
}

function Add-Check {
    param(
        [System.Collections.Generic.List[object]]$Checks,
        [string]$Name,
        [bool]$Passed,
        [string]$Detail
    )
    $Checks.Add([ordered]@{ name = $Name; passed = $Passed; detail = $Detail })
}

function Read-IniInt {
    param([string]$Path, [string]$Name)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $match = Select-String -LiteralPath $Path -Pattern "^$([regex]::Escape($Name))=(.+)$" | Select-Object -Last 1
    if ($null -eq $match) { return $null }
    return [int]$match.Matches[0].Groups[1].Value.Trim()
}

$checks = [System.Collections.Generic.List[object]]::new()

foreach ($artifact in @(
    'dataminer-client.exe',
    'dm_depth_probe.dll',
    'dm-depth-inject.exe',
    'depth-lab.exe'
)) {
    $path = Join-Path $ArtifactRoot $artifact
    Add-Check $checks "release:$artifact" (Test-Path -LiteralPath $path -PathType Leaf) $path
}

$display = Get-CimInstance Win32_VideoController |
    Where-Object { $null -ne $_.CurrentHorizontalResolution } |
    Select-Object -First 1
$displayOk = $null -ne $display -and
    $display.CurrentHorizontalResolution -ge 1920 -and
    $display.CurrentVerticalResolution -ge 1080
$displayDetail = if ($null -eq $display) { 'no active display reported' } else {
    "$($display.CurrentHorizontalResolution)x$($display.CurrentVerticalResolution)"
}
Add-Check $checks 'system-display-supports-1920x1080' $displayOk $displayDetail

$width = Read-IniInt $FalloutPrefsPath 'iSize W'
$height = Read-IniInt $FalloutPrefsPath 'iSize H'
$borderless = Read-IniInt $FalloutPrefsPath 'bBorderless'
$fullscreen = Read-IniInt $FalloutPrefsPath 'bFull Screen'
$gameDisplayOk = $width -eq 1920 -and $height -eq 1080 -and $borderless -eq 1 -and $fullscreen -eq 0
Add-Check $checks 'fallout4-borderless-fullscreen-1920x1080' $gameDisplayOk "size=${width}x${height}; borderless=$borderless; fullscreen=$fullscreen"

$falloutHash = if (Test-Path -LiteralPath $FalloutPath -PathType Leaf) {
    (Get-FileHash -LiteralPath $FalloutPath -Algorithm SHA256).Hash.ToLowerInvariant()
} else { '' }
Add-Check $checks 'fallout4-executable-pinned' ($falloutHash -eq $ExpectedFalloutHash) "sha256:$falloutHash"

$profile = $null
if ($DepthProfilePath -and (Test-Path -LiteralPath $DepthProfilePath -PathType Leaf)) {
    $profile = Get-Content -LiteralPath $DepthProfilePath -Raw | ConvertFrom-Json
}
$profileCertified = $null -ne $profile -and
    [string]$profile.cadence -eq 'every_present' -and
    $null -ne $profile.projection_a -and
    $null -ne $profile.projection_b -and
    $null -ne $profile.metric_scale -and
    $null -ne $profile.metric_scale.cross_check -and
    ([string]$profile.executable_sha256).ToLowerInvariant().Replace('sha256:', '') -eq $ExpectedFalloutHash
Add-Check $checks 'certified-depth-profile' $profileCertified $(if ($DepthProfilePath) { $DepthProfilePath } else { 'not supplied' })

$bindingOk = $false
if ($CameraBindingPath -and (Test-Path -LiteralPath $CameraBindingPath -PathType Leaf)) {
    $binding = Get-Content -LiteralPath $CameraBindingPath -Raw | ConvertFrom-Json
    $bindingOk = $binding.schema -eq 'camera-binding/v1' -and
        $binding.binding_id -eq 'fallout4-steam-1.11.221-main-camera-v3-render-hybrid' -and
        $binding.binding_version -eq '3' -and
        @($binding.view_sources).Count -eq 1 -and
        @($binding.projection_sources).Count -eq 1 -and
        $binding.game_camera.provider -eq 'fallout4-player-camera-root-exact-build/v1' -and
        [uint64]$binding.game_camera.camera_singleton_relocation -eq 0x030DBDD8 -and
        [uint32]$binding.game_camera.camera_root_offset -eq 0x20 -and
        [uint32]$binding.game_camera.world_transform_offset -eq 0x70 -and
        [string]$binding.game_camera.metres_per_game_unit -eq '0.0142875' -and
        $binding.game_camera.axis_mapping -eq 'x_z_y' -and
        $binding.view_sources[0].shader_stage -eq 'vertex' -and
        [uint32]$binding.view_sources[0].bind_slot -eq 12 -and
        [uint32]$binding.view_sources[0].pass_ordinal -eq 0 -and
        [uint32]$binding.view_sources[0].draw_ordinal -eq 6 -and
        [uint32]$binding.view_sources[0].buffer_bytes -eq 752 -and
        [uint32]$binding.view_sources[0].offset -eq 0 -and
        $binding.view_sources[0].layout -eq 'row_major' -and
        $binding.view_sources[0].role -eq 'view' -and
        $binding.view_sources[0].translation -eq 'row3' -and
        [uint32]$binding.projection_sources[0].bind_slot -eq 12 -and
        [uint32]$binding.projection_sources[0].pass_ordinal -eq 0 -and
        [uint32]$binding.projection_sources[0].draw_ordinal -eq 6 -and
        [uint32]$binding.projection_sources[0].buffer_bytes -eq 752 -and
        [uint32]$binding.projection_sources[0].offset -eq 64 -and
        ([string]$binding.executable_sha256).ToLowerInvariant().Replace('sha256:', '') -eq $ExpectedFalloutHash
}
Add-Check $checks 'production-camera-binding' $bindingOk $(if ($CameraBindingPath) { $CameraBindingPath } else { 'not supplied' })

$perfOk = $false
if ($PerfCertificationManifestPath -and $ProbeRuntimeManifestPath -and
    $PerfCertificationKeyPath -and $PerfHardwarePath -and
    (Test-Path -LiteralPath $PerfCertificationManifestPath -PathType Leaf) -and
    (Test-Path -LiteralPath $ProbeRuntimeManifestPath -PathType Leaf) -and
    (Test-Path -LiteralPath $PerfCertificationKeyPath -PathType Leaf) -and
    (Test-Path -LiteralPath $PerfHardwarePath -PathType Leaf)) {
    $depthLabPath = Join-Path $ArtifactRoot 'depth-lab.exe'
    $probeDllPath = Join-Path $ArtifactRoot 'dm_depth_probe.dll'
    if (Test-Path -LiteralPath $depthLabPath -PathType Leaf) {
        $probeDllSha = 'sha256:' + (Get-FileHash -LiteralPath $probeDllPath -Algorithm SHA256).Hash.ToLowerInvariant()
        & $depthLabPath perf-certification-check `
            --manifest $PerfCertificationManifestPath `
            --runtime $ProbeRuntimeManifestPath `
            --dll-sha256 $probeDllSha `
            --key $PerfCertificationKeyPath `
            --hardware $PerfHardwarePath | Out-Null
        $perfOk = $LASTEXITCODE -eq 0
    }
}
$perfDetail = "manifest=$PerfCertificationManifestPath; runtime=$ProbeRuntimeManifestPath; key=$PerfCertificationKeyPath; hardware=$PerfHardwarePath"
Add-Check $checks 'matching-performance-certification' $perfOk $perfDetail

$playerOk = $false
if ($PlayerTelemetryPath -and (Test-Path -LiteralPath $PlayerTelemetryPath -PathType Leaf)) {
    $playerRows = @(Get-Content -LiteralPath $PlayerTelemetryPath | ForEach-Object { $_ | ConvertFrom-Json })
    if ($playerRows.Count -ge 2) {
        $playerOk = $true
        for ($i = 0; $i -lt $playerRows.Count; $i++) {
            $row = $playerRows[$i]
            $playerOk = $playerOk -and
                $row.schema -eq 'player-pose-index/v1' -and
                $row.source -eq 'fallout4-player-root-exact-build/v1' -and
                ([string]$row.executable_sha256).ToLowerInvariant().Replace('sha256:', '') -eq $ExpectedFalloutHash -and
                @($row.player_position).Count -eq 3 -and
                @($row.player_rotation_quaternion).Count -eq 4 -and
                @($row.player_speed).Count -eq 3
            if ($i -gt 0) {
                $playerOk = $playerOk -and
                    [uint64]$row.present_seq -eq ([uint64]$playerRows[$i - 1].present_seq + 1) -and
                    [int64]$row.present_qpc -gt [int64]$playerRows[$i - 1].present_qpc
            }
        }
    }
}
Add-Check $checks 'authoritative-fallout-player-telemetry' ([bool]$playerOk) $(if ($PlayerTelemetryPath) { $PlayerTelemetryPath } else { 'not supplied' })

$finalAssemblerPath = Join-Path $dataMinerRoot 'tools\depth-lab\src\final_action_io.rs'
$finalAssemblerSource = Get-Content -LiteralPath $finalAssemblerPath -Raw
$finalAssemblerOk = $finalAssemblerSource -match 'assemble_final_pose_rows' -and
    $finalAssemblerSource -match 'PlayerPoseRow' -and
    $finalAssemblerSource -match 'join_camera_action'
Add-Check $checks 'final-action-camera-assembler' $finalAssemblerOk $finalAssemblerPath

$captureDisabled = $true
if (Test-Path -LiteralPath $probeConfigPath -PathType Leaf) {
    $probeConfig = Get-Content -LiteralPath $probeConfigPath -Raw | ConvertFrom-Json
    $captureDisabled = -not [bool]$probeConfig.enabled
}
Add-Check $checks 'formal-capture-remains-disabled-until-green' $captureDisabled $probeConfigPath

$failed = @($checks | Where-Object { -not $_.passed })
$report = [ordered]@{
    schema = 'pr405-readiness/v1'
    checked_at = (Get-Date).ToString('o')
    ready_for_formal_capture = $failed.Count -eq 0
    checks = $checks
    blockers = @($failed | ForEach-Object { $_.name })
}

$report | ConvertTo-Json -Depth 6
if ($failed.Count -ne 0) { exit 2 }

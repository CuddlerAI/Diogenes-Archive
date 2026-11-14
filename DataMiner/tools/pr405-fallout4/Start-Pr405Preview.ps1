[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [uint32]$TargetProcessId,
    [string]$SessionId = ("issue-544-pr405-preview-" + (Get-Date -Format 'yyyyMMdd-HHmmss')),
    [uint64]$Generation = 1,
    [uint64]$MaxFrames = 120,
    [ValidateRange(0, 12)]
    [uint32]$UnbindFromEnd = 4,
    [ValidateRange(1.0, 30.0)]
    [double]$TargetDepthHz = 30.0,
    [switch]$PassIdentityTrace,
    [string]$OutputParent = (Join-Path $env:LOCALAPPDATA 'DataMiner\pr405-captures'),
    [string]$DepthProfilePath = '',
    [string]$CameraBindingPath = '',
    [Parameter(Mandatory = $true)]
    [string]$PerfCertificationManifestPath,
    [Parameter(Mandatory = $true)]
    [string]$ProbeRuntimeManifestPath,
    [Parameter(Mandatory = $true)]
    [string]$PerfCertificationKeyPath,
    [Parameter(Mandatory = $true)]
    [string]$PerfHardwarePath,
    [string]$ArtifactRoot = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $ArtifactRoot) {
    $ArtifactRoot = Join-Path $repo 'target\release'
}
$ArtifactRoot = (Resolve-Path -LiteralPath $ArtifactRoot).Path
if (-not $DepthProfilePath) {
    $DepthProfilePath = Join-Path $repo 'config\catalogs\dev\fallout4-steam-release-test\depth-profile.json'
}
if (-not $CameraBindingPath) {
    $CameraBindingPath = Join-Path $repo 'config\catalogs\dev\fallout4-steam-release-test\camera-binding.json'
}
$DepthProfilePath = (Resolve-Path -LiteralPath $DepthProfilePath).Path
$CameraBindingPath = (Resolve-Path -LiteralPath $CameraBindingPath).Path
$probeDllPath = (Resolve-Path -LiteralPath (Join-Path $ArtifactRoot 'dm_depth_probe.dll')).Path
$injectorPath = (Resolve-Path -LiteralPath (Join-Path $ArtifactRoot 'dm-depth-inject.exe')).Path
$depthLabPath = (Resolve-Path -LiteralPath (Join-Path $ArtifactRoot 'depth-lab.exe')).Path
$PerfCertificationManifestPath = (Resolve-Path -LiteralPath $PerfCertificationManifestPath).Path
$ProbeRuntimeManifestPath = (Resolve-Path -LiteralPath $ProbeRuntimeManifestPath).Path
$PerfCertificationKeyPath = (Resolve-Path -LiteralPath $PerfCertificationKeyPath).Path
$PerfHardwarePath = (Resolve-Path -LiteralPath $PerfHardwarePath).Path
$probeDllSha = 'sha256:' + (Get-FileHash -LiteralPath $probeDllPath -Algorithm SHA256).Hash.ToLowerInvariant()
& $depthLabPath perf-certification-check `
    --manifest $PerfCertificationManifestPath `
    --runtime $ProbeRuntimeManifestPath `
    --dll-sha256 $probeDllSha `
    --key $PerfCertificationKeyPath `
    --hardware $PerfHardwarePath | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'the authoring manifest does not certify this generation key and explicit hardware'
}
$perfKey = Get-Content -LiteralPath $PerfCertificationKeyPath -Raw -Encoding UTF8 | ConvertFrom-Json
$perfHardware = Get-Content -LiteralPath $PerfHardwarePath -Raw -Encoding UTF8 | ConvertFrom-Json
$probeRuntime = Get-Content -LiteralPath $ProbeRuntimeManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
$depthProfileSha = 'sha256:' + (Get-FileHash -LiteralPath $DepthProfilePath -Algorithm SHA256).Hash.ToLowerInvariant()
$cameraBindingSha = 'sha256:' + (Get-FileHash -LiteralPath $CameraBindingPath -Algorithm SHA256).Hash.ToLowerInvariant()
$injectOutput = @(& $injectorPath --pid $TargetProcessId --dll $probeDllPath --expected-sha256 $probeDllSha --allow-existing 2>&1)
if ($LASTEXITCODE -ne 0) {
    throw "exact probe injection/reuse failed: $($injectOutput -join [Environment]::NewLine)"
}
$loadedProbeSha = @($injectOutput | ForEach-Object { [string]$_ } |
    Where-Object { $_ -like 'loaded_probe_sha256=*' } |
    ForEach-Object { $_.Substring('loaded_probe_sha256='.Length) } | Select-Object -Last 1)
if ($loadedProbeSha.Count -ne 1 -or $loadedProbeSha[0] -ne $probeDllSha) {
    throw "injector did not prove the exact resident probe hash: expected=$probeDllSha output=$($injectOutput -join '; ')"
}
$depthProfile = Get-Content -LiteralPath $DepthProfilePath -Raw | ConvertFrom-Json
$everyPresent = [string]$depthProfile.cadence -eq 'every_present'
$expectedPresentStep = if ($everyPresent) { 1 } else { 2 }
$controlDir = Join-Path $env:ProgramData 'DataMiner\depth-lab'
$controlPath = Join-Path $controlDir 'probe-control.json'
$outputDir = Join-Path $OutputParent $SessionId
$runId = 'r0001'

if (Test-Path -LiteralPath $outputDir) {
    throw "preview output already exists: $outputDir"
}
New-Item -ItemType Directory -Path $controlDir -Force | Out-Null
New-Item -ItemType Directory -Path $OutputParent -Force | Out-Null

function Write-Control([object]$Envelope) {
    $temp = Join-Path $controlDir ("probe-control.{0}.tmp" -f [Guid]::NewGuid().ToString('N'))
    $json = $Envelope | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($temp, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temp -Destination $controlPath -Force
}

function Read-Acks {
    $ackDir = Join-Path $outputDir 'probe-control\acks'
    if (-not (Test-Path -LiteralPath $ackDir -PathType Container)) { return @() }
    return @(Get-ChildItem -LiteralPath $ackDir -Filter '*.json' | Sort-Object Name | ForEach-Object {
        Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
    })
}

$identity = [ordered]@{
    session_id = $SessionId
    run_id = $runId
    generation = $Generation
}
$startId = "start-$SessionId"
Write-Control ([ordered]@{
    schema = 'depth-probe-control/v1'
    command_id = $startId
    identity = $identity
    command = [ordered]@{
        type = 'start_run'
        target_process_id = $TargetProcessId
        output_dir = $outputDir
        unbind_from_end = $UnbindFromEnd
        max_frames = $MaxFrames
        target_depth_hz = $TargetDepthHz
        depth_profile_path = $DepthProfilePath
        depth_profile_sha256 = $depthProfileSha
        camera_binding_path = $CameraBindingPath
        camera_binding_sha256 = $cameraBindingSha
        pass_identity_trace = [bool]$PassIdentityTrace
    }
})

$deadline = (Get-Date).AddSeconds(45)
do {
    Start-Sleep -Milliseconds 200
    $acks = Read-Acks
    $fault = $acks | Where-Object { $_.ack.type -eq 'faulted' } | Select-Object -Last 1
    if ($null -ne $fault) { throw "probe faulted before Ready: $($fault.ack | ConvertTo-Json -Compress -Depth 8)" }
    $ready = $acks | Where-Object { $_.ack.type -eq 'ready' } | Select-Object -Last 1
} until ($null -ne $ready -or (Get-Date) -ge $deadline)
if ($null -eq $ready) { throw 'probe did not reach Ready within 45 seconds' }
if ($ready.ack.actual_binding.binding_id -ne 'fallout4-steam-1.11.221-main-camera-v3-render-hybrid' -or
    $ready.ack.actual_binding.binding_version -ne '3') {
    throw "probe echoed unexpected camera binding: $($ready.ack.actual_binding.binding_id)"
}
if ($ready.ack.depth_profile_sha256 -ne $depthProfileSha -or
    $ready.ack.camera_binding_sha256 -ne $cameraBindingSha) {
    throw "probe Ready profile/binding hashes differ from the requested immutable files"
}
$actualAdapter = $ready.ack.actual_swapchain_adapter
$expectedGpuModel = (([string]$perfHardware.gpuModel).Trim() -split '\s+' -join ' ')
$actualGpuModel = (([string]$actualAdapter.description).Trim() -split '\s+' -join ' ')
if ($actualGpuModel -cne $expectedGpuModel -or
    ([string]$actualAdapter.driver_version).Trim() -cne ([string]$perfHardware.gpuDriverVersion).Trim() -or
    [int32]$actualAdapter.luid_high -ne [int32]$perfHardware.gpuAdapterLuidHigh -or
    [uint32]$actualAdapter.luid_low -ne [uint32]$perfHardware.gpuAdapterLuidLow) {
    throw "probe Ready swapchain adapter differs from the supplied hardware audit identity"
}
if (([string]$probeRuntime.dllSha256).ToLowerInvariant() -ne $probeDllSha -or
    ([string]$ready.ack.probe_version).Trim() -cne ([string]$probeRuntime.runtimeVersion).Trim() -or
    ([string]$perfKey.depthProfileSha256).ToLowerInvariant() -ne $depthProfileSha -or
    ([string]$perfKey.cameraBindingSha256).ToLowerInvariant() -ne $cameraBindingSha -or
    [uint32]$perfKey.unbindFromEnd -ne $UnbindFromEnd) {
    throw 'preview artifacts or unbind configuration differ from the generation certification inputs'
}

$depthPath = Join-Path $outputDir 'depth-index.jsonl'
$captureTimeoutSeconds = [Math]::Max(90, [Math]::Ceiling($MaxFrames / $TargetDepthHz) + 120)
$deadline = (Get-Date).AddSeconds($captureTimeoutSeconds)
do {
    Start-Sleep -Milliseconds 250
    $depthCount = if (Test-Path -LiteralPath $depthPath) {
        @(Get-Content -LiteralPath $depthPath).Count
    } else { 0 }
} until ($depthCount -ge $MaxFrames -or (Get-Date) -ge $deadline)
$captureTimedOut = $depthCount -lt $MaxFrames

$stopId = "stop-$SessionId"
Write-Control ([ordered]@{
    schema = 'depth-probe-control/v1'
    command_id = $stopId
    identity = $identity
    command = [ordered]@{
        type = 'stop_run'
        cutoff_qpc = [Diagnostics.Stopwatch]::GetTimestamp()
        reason = 'production_preview_complete'
    }
})

$deadline = (Get-Date).AddSeconds(45)
do {
    Start-Sleep -Milliseconds 200
    $acks = Read-Acks
    $terminal = $acks | Where-Object { $_.ack.type -in @('flushed', 'faulted') } | Select-Object -Last 1
} until ($null -ne $terminal -or (Get-Date) -ge $deadline)
if ($null -eq $terminal) { throw 'probe did not flush within 45 seconds' }
if ($terminal.ack.type -ne 'flushed') {
    throw "probe faulted during flush: $($terminal.ack | ConvertTo-Json -Compress -Depth 8)"
}

$cameraPath = Join-Path $outputDir 'camera-pose-index.jsonl'
$playerPath = Join-Path $outputDir 'player-pose-index.jsonl'
$diagnosticPath = Join-Path $outputDir 'camera-pose-diagnostics.jsonl'
$passIdentityPath = Join-Path $outputDir 'depth-pass-identity-trace.jsonl'
$depthRows = @(Get-Content -LiteralPath $depthPath | ForEach-Object { $_ | ConvertFrom-Json })
$cameraRows = @(Get-Content -LiteralPath $cameraPath | ForEach-Object { $_ | ConvertFrom-Json })
$playerRows = @(Get-Content -LiteralPath $playerPath | ForEach-Object { $_ | ConvertFrom-Json })
$cameraBySeq = @{}
$playerBySeq = @{}
foreach ($row in $cameraRows) { $cameraBySeq[[uint64]$row.present_seq] = $row }
foreach ($row in $playerRows) { $playerBySeq[[uint64]$row.present_seq] = $row }

$errors = [System.Collections.Generic.List[string]]::new()
$passIdentityRows = @()
$passIdentityJoinCounts = @{}
if ($PassIdentityTrace) {
    if (-not (Test-Path -LiteralPath $passIdentityPath -PathType Leaf)) {
        $errors.Add('pass identity trace was requested but the JSONL file is missing')
    } else {
        $passIdentityRows = @(Get-Content -LiteralPath $passIdentityPath | ForEach-Object { $_ | ConvertFrom-Json })
        if ($passIdentityRows.Count -ne $depthRows.Count) {
            $errors.Add("pass identity/depth row counts disagree: $($passIdentityRows.Count)/$($depthRows.Count)")
        }
        for ($index = 0; $index -lt $passIdentityRows.Count; $index++) {
            $trace = $passIdentityRows[$index]
            if ($trace.schema -ne 'depth-pass-identity-trace/v1') {
                $errors.Add("pass identity row $index has wrong schema")
            }
            if ($index -lt $depthRows.Count -and [uint64]$trace.present_seq -ne [uint64]$depthRows[$index].present_seq) {
                $errors.Add("pass identity/depth present_seq mismatch at row $index")
            }
            $join = [string]$trace.join
            if (-not $passIdentityJoinCounts.ContainsKey($join)) { $passIdentityJoinCounts[$join] = 0 }
            $passIdentityJoinCounts[$join]++
        }
    }
}
if ($captureTimedOut) { $errors.Add("only $depthCount/$MaxFrames depth frames arrived within $captureTimeoutSeconds seconds") }
$previousDepthSeq = $null
$depthCadenceQpc = [System.Collections.Generic.List[long]]::new()
foreach ($row in $depthRows) {
    $seq = [uint64]$row.present_seq
    if ($row.schema -ne 'depth-index/v6') { $errors.Add("depth $seq is not v6") }
    $defectCount = if ($row.health.PSObject.Properties.Name -contains 'defects') {
        @($row.health.defects).Count
    } else { 0 }
    if ($defectCount -ne 0) { $errors.Add("depth $seq has health defects") }
    if ([bool]$row.health.raw.uniform -or [bool]$row.health.stored.uniform) {
        $errors.Add("depth $seq is uniform")
    }
    if ([uint64]$row.health.raw.unique_sampled -le 2048) { $errors.Add("depth $seq has <=2048 raw unique samples") }
    if (-not $everyPresent) {
        if ($null -eq $depthPresentParity) { $depthPresentParity = $seq % 2 }
        if (($seq % 2) -ne $depthPresentParity) { $errors.Add("depth $seq changed cadence parity") }
    }
    if ($null -ne $previousDepthSeq -and $seq -ne ($previousDepthSeq + $expectedPresentStep)) {
        $errors.Add("depth sequence gap: $previousDepthSeq -> $seq")
    }
    $previousDepthSeq = $seq
    if ([uint32]$row.width -ne 1280 -or [uint32]$row.height -ne 720) { $errors.Add("depth $seq has wrong dimensions") }
    if ($row.storage.encoding -ne 'exr-half-zip' -or $row.storage.channel -ne 'Z' -or
        $row.storage.transform -ne 'one_minus_d') { $errors.Add("depth $seq has wrong storage contract") }
    if ($row.projection.proj_source -ne 'constant_buffer' -or
        $row.projection.proj_scope -ne 'observed_this_frame' -or
        $null -eq $row.projection.proj_a -or
        $null -eq $row.projection.proj_b -or $null -eq $row.projection.metric_scale) {
        $errors.Add("depth $seq lacks per-frame projection evidence")
    }
    if (-not $cameraBySeq.ContainsKey($seq)) { $errors.Add("depth $seq has no camera pose") }
    if (-not $playerBySeq.ContainsKey($seq)) {
        $errors.Add("depth $seq has no player pose")
    } else {
        $depthCadenceQpc.Add([long]$playerBySeq[$seq].present_qpc)
    }
}
$qpcFrequency = [double][Diagnostics.Stopwatch]::Frequency
$targetPeriodQpc = $qpcFrequency / $TargetDepthHz
$observedDepthHz = $null
$largestDepthIntervalMs = $null
if ($depthCadenceQpc.Count -ge 2) {
    $largestGapQpc = 0L
    for ($index = 1; $index -lt $depthCadenceQpc.Count; $index++) {
        $gapQpc = $depthCadenceQpc[$index] - $depthCadenceQpc[$index - 1]
        if ($gapQpc -le 0) {
            $errors.Add("depth cadence QPC is not strictly increasing at sample $index")
            continue
        }
        if ($gapQpc -gt $largestGapQpc) { $largestGapQpc = $gapQpc }
        if ($gapQpc -gt (3.0 * $targetPeriodQpc)) {
            $errors.Add("depth cadence gap at sample $index exceeds three target periods")
        }
    }
    $elapsedQpc = $depthCadenceQpc[$depthCadenceQpc.Count - 1] - $depthCadenceQpc[0]
    if ($elapsedQpc -gt 0) {
        $observedDepthHz = (($depthCadenceQpc.Count - 1) * $qpcFrequency) / $elapsedQpc
        $relativeCadenceError = [Math]::Abs($observedDepthHz - $TargetDepthHz) / $TargetDepthHz
        if ($relativeCadenceError -gt 0.10) {
            $errors.Add("depth cadence $([Math]::Round($observedDepthHz, 3)) Hz differs from target $TargetDepthHz Hz by more than 10%")
        }
    }
    $largestDepthIntervalMs = 1000.0 * $largestGapQpc / $qpcFrequency
}
$expectedDepthRows = [int][Math]::Min([double]$MaxFrames, [double]$depthCount)
if ($depthRows.Count -ne $expectedDepthRows) { $errors.Add("unexpected depth row count $($depthRows.Count), expected $expectedDepthRows") }
foreach ($row in $cameraRows) {
    if ($row.source -ne 'game_telemetry' -or
        $row.provenance.kind -ne 'game_telemetry_main_camera_projection' -or
        $row.provenance.game_camera.provider -ne 'fallout4-player-camera-root-exact-build/v1') {
        $errors.Add("camera $($row.present_seq) has wrong provenance")
    }
}
$depthFiles = @(Get-ChildItem -LiteralPath (Join-Path $outputDir 'raw') -Filter '*.exr').Count
if ($depthFiles -ne $depthRows.Count) { $errors.Add("depth rows/files disagree: $($depthRows.Count)/$depthFiles") }
$diagnosticCount = if (Test-Path -LiteralPath $diagnosticPath) {
    @(Get-Content -LiteralPath $diagnosticPath).Count
} else { 0 }

$report = [ordered]@{
    schema = 'pr405-production-preview-validation/v3'
    validated_at = (Get-Date).ToString('o')
    passed = $errors.Count -eq 0
    output_dir = $outputDir
    identity = $identity
    cadence = if ($everyPresent) { 'every_present' } else { 'target_hz' }
    ready_first_present_seq = [uint64]$ready.ack.first_present_seq
    depth_cadence = [ordered]@{
        target_hz = $TargetDepthHz
        observed_hz = $observedDepthHz
        qpc_source = 'player-pose-index/v1'
        qpc_frequency_hz = [uint64]$qpcFrequency
        largest_interval_ms = $largestDepthIntervalMs
    }
    cutoff_present_seq = [uint64]$terminal.ack.summary.cutoff_present_seq
    depth_row_count = $depthRows.Count
    depth_file_count = $depthFiles
    camera_pose_row_count = $cameraRows.Count
    camera_diagnostic_row_count = $diagnosticCount
    player_pose_row_count = $playerRows.Count
    pass_identity_trace = [ordered]@{
        enabled = [bool]$PassIdentityTrace
        row_count = $passIdentityRows.Count
        join_counts = $passIdentityJoinCounts
    }
    errors = @($errors)
}
$reportPath = Join-Path $outputDir 'preview-validation.json'
$reportJson = $report | ConvertTo-Json -Depth 8
[IO.File]::WriteAllText($reportPath, $reportJson, [Text.UTF8Encoding]::new($false))
$report | ConvertTo-Json -Depth 8
if ($errors.Count -ne 0) { exit 2 }

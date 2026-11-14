[CmdletBinding()]
param(
    [Parameter(Mandatory)] [uint32]$ProcessId,
    [Parameter(Mandatory)] [uint64]$Generation,
    [Parameter(Mandatory)] [string]$OutputDir,
    [ValidateRange(1, 10000)] [uint64]$MaxFrames = 60,
    [ValidateRange(1.0, 240.0)] [double]$TargetDepthHz = 30.0
)

$ErrorActionPreference = 'Stop'
$profilePath = Join-Path $PSScriptRoot 'halo-reach-mcc-1.3528.0.0.lane30.json'
$bindingPath = Join-Path $PSScriptRoot 'halo-reach-mcc-camera-binding.json'
$profileHash = 'sha256:' + (Get-FileHash -LiteralPath $profilePath -Algorithm SHA256).Hash.ToLowerInvariant()
$bindingHash = 'sha256:' + (Get-FileHash -LiteralPath $bindingPath -Algorithm SHA256).Hash.ToLowerInvariant()
$controlPath = 'C:\ProgramData\DataMiner\depth-lab\probe-control.json'
$expectedExeHash = 'be70d6dcd1a884f10ceb342a7a2dcb35ee0fa43181b66a1d19c3d830e9834691'
$expectedReachHash = '738dd2d24ea3aea12e1ee9aa4a61094bf116027d42004c35a19e5048608b0894'
$identity = [ordered]@{
    session_id = 'halo-reach-depth-camera-cert-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
    run_id = 'r0001'
    generation = $Generation
}

if (Test-Path -LiteralPath $OutputDir) { throw "Output directory already exists: $OutputDir" }
$process = Get-Process -Id $ProcessId -ErrorAction Stop
$actualHash = (Get-FileHash -LiteralPath $process.Path -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $expectedExeHash) {
    throw "Executable hash mismatch: expected $expectedExeHash, got $actualHash"
}
$reachModule = @($process.Modules | Where-Object { $_.ModuleName -ieq 'haloreach.dll' })
if ($reachModule.Count -ne 1) { throw 'Halo: Reach title module is not uniquely loaded' }
$actualReachHash = (Get-FileHash -LiteralPath $reachModule[0].FileName -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualReachHash -ne $expectedReachHash) {
    throw "Halo: Reach title-module hash mismatch: expected $expectedReachHash, got $actualReachHash"
}
if (-not @($process.Modules | Where-Object { $_.ModuleName -ieq 'd3d11.dll' })) {
    throw 'Halo: Reach certification requires the observed D3D11 runtime'
}
$antiModules = @($process.Modules | Where-Object { $_.ModuleName -match '(?i)^(ace|anti.?cheat|easyanticheat|eac)' })
$antiServices = @(Get-CimInstance Win32_Service | Where-Object {
    ($_.Name -match '(?i)AntiCheatExpert|EasyAntiCheat|^ACE') -and $_.State -ne 'Stopped'
})
$antiDrivers = @(Get-CimInstance Win32_SystemDriver | Where-Object {
    ($_.Name -match '(?i)^ACE-|ace-game|EasyAntiCheat|AntiCheat') -and $_.State -ne 'Stopped'
})
if ($antiModules.Count -ne 0 -or $antiServices.Count -ne 0 -or $antiDrivers.Count -ne 0) {
    throw 'Anti-cheat runtime gate is not clean; use the official Anti-Cheat Disabled launch item'
}

function Write-Control([System.Collections.IDictionary]$Envelope) {
    $directory = Split-Path -Parent $controlPath
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $temporary = $controlPath + '.tmp'
    [IO.File]::WriteAllText(
        $temporary,
        ($Envelope | ConvertTo-Json -Depth 12),
        [Text.UTF8Encoding]::new($false)
    )
    Move-Item -LiteralPath $temporary -Destination $controlPath -Force
}

function Wait-Ack([string]$Path, [int]$TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if (Test-Path -LiteralPath $Path) { return }
        $fault = Get-ChildItem -LiteralPath (Split-Path -Parent $Path) -Filter '*-faulted.json' -ErrorAction SilentlyContinue |
            Sort-Object Name | Select-Object -Last 1
        if ($null -ne $fault) {
            throw "Probe faulted: $((Get-Content -LiteralPath $fault.FullName -Raw | ConvertFrom-Json).ack | ConvertTo-Json -Compress -Depth 12)"
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "Timed out waiting for $Path"
}

function Stop-Run([string]$Reason) {
    Write-Control ([ordered]@{
        schema = 'depth-probe-control/v1'
        command_id = 'stop-' + $identity.session_id
        identity = $identity
        command = [ordered]@{
            type = 'stop_run'
            cutoff_qpc = [System.Diagnostics.Stopwatch]::GetTimestamp()
            reason = $Reason
        }
    })
}

Write-Control ([ordered]@{
    schema = 'depth-probe-control/v1'
    command_id = 'start-' + $identity.session_id
    identity = $identity
    command = [ordered]@{
        type = 'start_run'
        target_process_id = $ProcessId
        output_dir = $OutputDir
        unbind_from_end = 1
        max_frames = $MaxFrames
        target_depth_hz = $TargetDepthHz
        depth_profile_path = $profilePath
        depth_profile_sha256 = $profileHash
        camera_binding_path = $bindingPath
        camera_binding_sha256 = $bindingHash
        pass_identity_trace = $false
    }
})

$ackDir = Join-Path $OutputDir 'probe-control\acks'
Wait-Ack (Join-Path $ackDir '0002-ready.json') 90
$depthIndex = Join-Path $OutputDir 'depth-index.jsonl'
$deadline = (Get-Date).AddSeconds(90)
do {
    $depthRows = if (Test-Path -LiteralPath $depthIndex) { @(Get-Content -LiteralPath $depthIndex).Count } else { 0 }
    if ($depthRows -ge $MaxFrames) { break }
    Start-Sleep -Milliseconds 250
} while ((Get-Date) -lt $deadline)
if ($depthRows -ne $MaxFrames) {
    Stop-Run "bounded Halo: Reach certification timed out after capturing $depthRows of $MaxFrames depth rows"
    throw "Expected $MaxFrames depth rows, got $depthRows"
}

Stop-Run 'bounded Halo: Reach depth/camera certification sample complete'
$flushedPath = Join-Path $ackDir '0004-flushed.json'
Wait-Ack $flushedPath 120
$summary = (Get-Content -LiteralPath $flushedPath -Raw | ConvertFrom-Json).ack.summary
if ($summary.writer_queue_depth -ne 0 -or $summary.last_present_seq -ne $summary.cutoff_present_seq) {
    throw 'Flush barrier did not close on the cutoff present'
}
if ($summary.depth_row_count -ne $MaxFrames -or $summary.depth_file_count -ne $MaxFrames) {
    throw "Depth count mismatch: $($summary | ConvertTo-Json -Compress)"
}
if ($summary.camera_diagnostic_row_count -ne 0 -or @($summary.errors).Count -ne 0) {
    throw "Probe reported camera diagnostics or writer errors: $($summary | ConvertTo-Json -Compress)"
}

$cameraPresents = [Collections.Generic.HashSet[uint64]]::new()
Get-Content -LiteralPath (Join-Path $OutputDir 'camera-pose-index.jsonl') | ForEach-Object {
    [void]$cameraPresents.Add([uint64](($_ | ConvertFrom-Json).present_seq))
}
$invalidDepth = 0
Get-Content -LiteralPath $depthIndex | ForEach-Object {
    $row = $_ | ConvertFrom-Json
    if ($row.health.raw.uniform -or $row.health.stored.uniform -or
        $row.projection.proj_scope -ne 'observed_this_frame' -or
        -not $row.pass.selection.operator_exact_pin -or
        -not $cameraPresents.Contains([uint64]$row.present_seq)) {
        $invalidDepth++
    }
}
if ($invalidDepth -ne 0) { throw "$invalidDepth depth rows failed depth/camera invariants" }

[pscustomobject]@{
    OutputDir = $OutputDir
    Generation = $Generation
    DepthRows = $summary.depth_row_count
    CameraRows = $summary.camera_pose_row_count
    PlayerRows = $summary.player_pose_row_count
    Errors = @($summary.errors).Count
    Status = 'research_only_depth_camera_ready_player_pose_blocked'
}

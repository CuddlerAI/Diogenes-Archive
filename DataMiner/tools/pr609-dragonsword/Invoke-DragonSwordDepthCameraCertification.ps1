[CmdletBinding()]
param(
    [Parameter(Mandatory)] [uint32]$ProcessId,
    [Parameter(Mandatory)] [uint64]$Generation,
    [Parameter(Mandatory)] [string]$OutputDir,
    [ValidateRange(1, 10000)] [uint64]$MaxFrames = 60,
    [ValidateRange(1.0, 240.0)] [double]$TargetDepthHz = 30.0,
    [ValidateRange(1, 65536)] [int]$MinRawUniqueSampled = 1000,
    [ValidateRange(1, 65536)] [int]$MinStoredUniqueSampled = 500
)

$ErrorActionPreference = 'Stop'
$profilePath = Join-Path $PSScriptRoot 'dragonsword-1.0.8.lane30.json'
$bindingPath = Join-Path $PSScriptRoot 'dragonsword-camera-binding.json'
$profileHash = 'sha256:' + (Get-FileHash -LiteralPath $profilePath -Algorithm SHA256).Hash.ToLowerInvariant()
$bindingHash = 'sha256:' + (Get-FileHash -LiteralPath $bindingPath -Algorithm SHA256).Hash.ToLowerInvariant()
$controlPath = 'C:\ProgramData\DataMiner\depth-lab\probe-control.json'
$expectedExeHash = '0c9d54a35d7160e671a3db10c5e645140fdad15a7293314681e9eda7e360c1fe'
$identity = [ordered]@{
    session_id = 'dragonsword-depth-camera-cert-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
    run_id = 'r0001'
    generation = $Generation
}

if (Test-Path -LiteralPath $OutputDir) { throw "Output directory already exists: $OutputDir" }
$process = Get-Process -Id $ProcessId -ErrorAction Stop
$actualHash = (Get-FileHash -LiteralPath $process.Path -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $expectedExeHash) {
    throw "Executable hash mismatch: expected $expectedExeHash, got $actualHash"
}
$commandLine = (Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId").CommandLine
if ($commandLine -notmatch '(?i)(^|\s)-dx11(\s|$)') { throw 'DragonSword certification requires the audited -dx11 launch path' }
$antiModules = @($process.Modules | Where-Object { $_.ModuleName -match '(?i)^(ace|anti.?cheat|easyanticheat|eac)' })
$antiServices = @(Get-CimInstance Win32_Service | Where-Object {
    ($_.Name -match '(?i)AntiCheatExpert|EasyAntiCheat|^ACE') -and $_.State -ne 'Stopped'
})
$antiDrivers = @(Get-CimInstance Win32_SystemDriver | Where-Object {
    ($_.Name -match '(?i)^ACE-|ace-game|EasyAntiCheat|AntiCheat') -and $_.State -ne 'Stopped'
})
if ($antiModules.Count -ne 0 -or $antiServices.Count -ne 0 -or $antiDrivers.Count -ne 0) {
    throw 'Anti-cheat runtime gate is not clean; refusing certification'
}

function Write-Control([System.Collections.IDictionary]$Envelope) {
    $directory = Split-Path -Parent $controlPath
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $temporary = $controlPath + '.tmp'
    $json = $Envelope | ConvertTo-Json -Depth 12
    [IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary -Destination $controlPath -Force
}

function Wait-Ack([string]$Path, [int]$TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if (Test-Path -LiteralPath $Path) { return }
        $fault = Get-ChildItem -LiteralPath (Split-Path -Parent $Path) -Filter '*-faulted.json' -ErrorAction SilentlyContinue |
            Sort-Object Name |
            Select-Object -Last 1
        if ($null -ne $fault) {
            throw "Probe faulted: $((Get-Content -LiteralPath $fault.FullName -Raw | ConvertFrom-Json).ack | ConvertTo-Json -Compress -Depth 12)"
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "Timed out waiting for $Path"
}

function Stop-Run([string]$Reason) {
    $stopId = 'stop-' + $identity.session_id
    Write-Control ([ordered]@{
        schema = 'depth-probe-control/v1'
        command_id = $stopId
        identity = $identity
        command = [ordered]@{
            type = 'stop_run'
            cutoff_qpc = [System.Diagnostics.Stopwatch]::GetTimestamp()
            reason = $Reason
        }
    })
}

$startId = 'start-' + $identity.session_id
Write-Control ([ordered]@{
    schema = 'depth-probe-control/v1'
    command_id = $startId
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
    Stop-Run "bounded DragonSword certification timed out after capturing $depthRows of $MaxFrames depth rows"
    Wait-Ack (Join-Path $ackDir '0004-flushed.json') 120
    throw "Expected $MaxFrames depth rows, got $depthRows"
}

Stop-Run 'bounded DragonSword depth/camera certification sample complete'

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
$lowComplexityDepth = 0
$minimumRawUniqueSampled = [int]::MaxValue
$minimumStoredUniqueSampled = [int]::MaxValue
Get-Content -LiteralPath $depthIndex | ForEach-Object {
    $row = $_ | ConvertFrom-Json
    $rawUniqueSampled = [int]$row.health.raw.unique_sampled
    $storedUniqueSampled = [int]$row.health.stored.unique_sampled
    $minimumRawUniqueSampled = [Math]::Min($minimumRawUniqueSampled, $rawUniqueSampled)
    $minimumStoredUniqueSampled = [Math]::Min($minimumStoredUniqueSampled, $storedUniqueSampled)
    if ($rawUniqueSampled -lt $MinRawUniqueSampled -or
        $storedUniqueSampled -lt $MinStoredUniqueSampled) {
        $lowComplexityDepth++
    }
    if ($row.health.raw.uniform -or $row.health.stored.uniform -or
        $row.projection.proj_scope -ne 'observed_this_frame' -or
        -not $row.pass.selection.operator_exact_pin -or
        -not $cameraPresents.Contains([uint64]$row.present_seq)) {
        $invalidDepth++
    }
}
if ($invalidDepth -ne 0) { throw "$invalidDepth depth rows failed depth/camera invariants" }
if ($lowComplexityDepth -ne 0) {
    throw "$lowComplexityDepth depth rows failed the main-scene complexity gate: " +
        "minimum raw/stored unique_sampled=$minimumRawUniqueSampled/$minimumStoredUniqueSampled, " +
        "required >=$MinRawUniqueSampled/>=$MinStoredUniqueSampled"
}

[pscustomobject]@{
    OutputDir = $OutputDir
    Generation = $Generation
    DepthRows = $summary.depth_row_count
    CameraRows = $summary.camera_pose_row_count
    PlayerRows = $summary.player_pose_row_count
    MinimumRawUniqueSampled = $minimumRawUniqueSampled
    MinimumStoredUniqueSampled = $minimumStoredUniqueSampled
    Errors = @($summary.errors).Count
    Status = 'research_only_depth_camera_ready_player_pose_blocked'
}

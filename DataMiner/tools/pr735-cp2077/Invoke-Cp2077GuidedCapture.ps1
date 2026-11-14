#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$RecorderPath,
    [string]$SessionDir,
    [uint32]$GameProcessId,
    [string]$LaunchId,
    [switch]$StartCapture,
    [ValidateRange(10,60)]
    [int]$PreparationSeconds = 10
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
# Presentation only. Rust supplies the preview AND the actual timed cues.
# No game input, independent capture timer or source-artifact repair.
if (-not [IO.Path]::IsPathRooted($RecorderPath) -or -not (Test-Path -LiteralPath $RecorderPath -PathType Leaf)) {
    throw 'Recorder must be an existing absolute path'
}
# A hidden PowerShell process may decode native stdout with the legacy console
# codepage. Read the recorder's UTF-8 JSON explicitly, just like actual cues.
$previewProcess = [Diagnostics.Process]::new()
$previewProcess.StartInfo.FileName = $RecorderPath
$previewProcess.StartInfo.UseShellExecute = $false
$previewProcess.StartInfo.CreateNoWindow = $true
$previewProcess.StartInfo.RedirectStandardOutput = $true
$previewProcess.StartInfo.RedirectStandardError = $true
$previewProcess.StartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true)
$previewProcess.StartInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false, $true)
$previewProcess.StartInfo.ArgumentList.Add('--print-action-plan')
try {
    if (-not $previewProcess.Start()) { throw 'Recorder action preview did not start' }
    $previewOutputTask = $previewProcess.StandardOutput.ReadToEndAsync()
    $previewErrorTask = $previewProcess.StandardError.ReadToEndAsync()
    $previewProcess.WaitForExit()
    $previewText = $previewOutputTask.GetAwaiter().GetResult()
    $previewError = $previewErrorTask.GetAwaiter().GetResult()
    if ($previewProcess.ExitCode -ne 0) { throw "Recorder action preview failed: $previewError" }
} finally { $previewProcess.Dispose() }
$plan = ConvertFrom-Json -InputObject $previewText
if ($plan.schema -ne 'dm-cp2077-guided-preview/v2' -or $plan.speech_protocol -ne 'short_cue_sapi_completion/v1' -or $plan.action_plan_schema -ne 'dm-cp2077-raw-demo-action-plan/v2' -or
    $plan.input_policy -notin @('fixed_view_ws/v2', 'fixed_view_ws/v3') -or $plan.contract -ne 'issue735_local' -or $plan.duration_ms -ne 64000 -or
    $plan.steps.Count -ne 9 -or -not $plan.non_trainable -or $plan.recording_ready) { throw 'Unsupported guided action protocol' }
$cues = [ordered]@{}
$shortCues = [ordered]@{}
$previousOffset = -1
foreach ($step in $plan.steps) {
    if ($step.offset_ms -le $previousOffset -or [string]::IsNullOrWhiteSpace($step.instruction_zh) -or
        [string]::IsNullOrWhiteSpace($step.speech_zh) -or $step.speech_zh.Length -gt 6) { throw 'Invalid preview cue order/text' }
    $cues[[string]$step.offset_ms] = [string]$step.instruction_zh
    $shortCues[[string]$step.offset_ms] = [string]$step.speech_zh
    $previousOffset = $step.offset_ms
}
if (@($cues.Keys)[0] -ne '0' -or @($cues.Keys)[8] -ne '64000') { throw 'Incomplete preview timeline' }
Write-Output '完整时间表（录制前读完；聊天转述可能延迟，采集中以现场语音为准）：'
foreach ($step in $plan.steps) { Write-Output ('{0,5:N1} 秒：{1}' -f ($step.offset_ms / 1000.0),$step.instruction_zh) }
Write-Output '只用 W/S；按住直到松开提示，中途不要松手重按，也不要按 Shift。后退不转身，鼠标留待单独标定。'
if ($plan.input_policy -eq 'fixed_view_ws/v3') {
    Write-Output '人工数秒允许按下、松开各偏差 5 秒；每次连续按住仍需 12–24 秒，结束前全部松开。'
}
Write-Output '正式语音只说短口令：“前进”按住 W，“后退”按住 S，“松手”松开按键；“微调”才允许短按 W/S。'
$speech = $null
$stderr = $null
$spoken = $null
$stdout = $null
$process = $null
try {
    $speech = New-Object -ComObject SAPI.SpVoice
    $chinese = @($speech.GetVoices() | Where-Object { $_.GetDescription() -match 'Huihui|Chinese' })
    if ($chinese.Count -eq 0) { throw 'No installed Chinese speech voice' }
    $speech.Voice = $chinese[0]
    $speech.Rate = 2
    $speech.Volume = 100
    if (-not $StartCapture) {
        if ($cues.Count -ne 9 -or @($cues.Keys)[8] -ne '64000') { throw 'Invalid cue map' }
        foreach ($key in $cues.Keys) {
            if (-not $cues.Contains([string]$key) -or [string]::IsNullOrWhiteSpace($cues[[string]$key])) { throw 'Cue lookup failed' }
        }
        [void]$speech.Speak('这是声音测试，还没有开始录制。请放开双手，保持不动。如果这句话能听清，音量就可以。', 0)
        [pscustomobject]@{ presentation_only=$true; preview_only=$true; cue_count=$cues.Count; voice=$chinese[0].GetDescription(); game_started=$false; capture_started=$false } | ConvertTo-Json
        return
    }
    if ($LaunchId -notmatch '^[0-9a-f]{32}$' -or $GameProcessId -eq 0) { throw 'Invalid launched game identity' }
    if (-not [IO.Path]::IsPathRooted($RecorderPath) -or -not (Test-Path -LiteralPath $RecorderPath -PathType Leaf)) { throw 'Recorder must be an existing absolute path' }
    $session = [IO.Path]::GetFullPath($SessionDir)
    $privateRoot = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'DataMiner\depth-raw-demo\cp2077'))
    if (-not $session.StartsWith($privateRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Session outside private raw-demo root' }
    foreach ($stale in @('raw-depth-demo-arm-request.json','raw-depth-telemetry.json','operator-action-plan.json','pr727-settings-runtime-evidence.json')) {
        if (Test-Path -LiteralPath (Join-Path $session $stale)) { throw "Refusing stale capture file: $stale" }
    }
    if ((Get-Item -LiteralPath (Join-Path $session 'raw-depth-index.jsonl')).Length -ne 0) { throw 'Refusing a session that already captured depth' }
    $launch = Get-Content -LiteralPath (Join-Path $session 'launcher.log')
    if ($launch.Count -ne 3 -or $launch[0] -ne "launched_pid=$GameProcessId" -or $launch[2] -ne "launch_id=$LaunchId") { throw 'Game launch identity mismatch' }
    $logDirectory = $session
    foreach ($stale in @('coordinator.stderr.log','coordinator.stdout.log','spoken-cues.jsonl','coordinator-exit.json')) {
        if (Test-Path -LiteralPath (Join-Path $logDirectory $stale)) { throw "Refusing stale attempt log: $stale" }
    }
    $encoding = [Text.UTF8Encoding]::new($false)
    function New-LogWriter([string]$Name) {
        $stream = [IO.File]::Open((Join-Path $logDirectory $Name), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        $writer = [IO.StreamWriter]::new($stream, $encoding)
        $writer.AutoFlush = $true
        return $writer
    }
    $stderr = New-LogWriter 'coordinator.stderr.log'
    $spoken = New-LogWriter 'spoken-cues.jsonl'
    $stdout = New-LogWriter 'coordinator.stdout.log'
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $RecorderPath
    $process.StartInfo.WorkingDirectory = Split-Path -Parent $RecorderPath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.StandardErrorEncoding = $encoding
    $process.StartInfo.StandardOutputEncoding = $encoding
    foreach ($argument in @('--pid',[string]$GameProcessId,'--launch-id',$LaunchId,'--session-dir',$session,'--contract','issue735_local','--duration-seconds','64')) {
        $process.StartInfo.ArgumentList.Add($argument)
    }
    $count = 0
    try {
        [void]$speech.Speak("这是准备阶段，还没有开始录制。请回到游戏，放开双手，保持不动。准备时间 $PreparationSeconds 秒；准备结束后也继续静止，等待正式口令。",0)
        Write-Output "准备 $PreparationSeconds 秒，尚未录制。"
        Start-Sleep -Seconds $PreparationSeconds
        if (-not $process.Start()) { throw 'Recorder did not start' }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        while ($null -ne ($line = $process.StandardError.ReadLine())) {
            $stderr.WriteLine($line)
            $receivedQpc = [Diagnostics.Stopwatch]::GetTimestamp()
            if ($line.TrimStart([char]7) -match '^\[CP2077 action\s+(\d+)\.(\d{3})s\] ') {
                $offset = 1000 * [int]$Matches[1] + [int]$Matches[2]
                if (-not $cues.Contains([string]$offset) -or $count -ge $cues.Count -or @($cues.Keys)[$count] -ne [string]$offset) {
                    $spoken.WriteLine((@{ error='unexpected_cue'; received_qpc=$receivedQpc; line=$line } | ConvertTo-Json -Compress))
                    [void]$speech.Speak('提示异常，请松开所有键。',3)
                    throw 'Unexpected actual cue; presentation stopped'
                }
                $message = $cues[[string]$offset]
                # Surface the same actual cue for the Codex operator to relay in chat;
                # this is auxiliary display, never the authoritative action clock.
                Write-Output ("[动作提示 {0:N3}s] {1}" -f ($offset / 1000.0), $message)
                $requestQpc = [Diagnostics.Stopwatch]::GetTimestamp()
                $shortMessage = $shortCues[[string]$offset]
                $streamNumber = $speech.Speak($shortMessage,3)
                $returnedQpc = [Diagnostics.Stopwatch]::GetTimestamp()
                $spoken.WriteLine((@{
                    schema='local-cp2077-spoken-cue/v2'; event='requested'; process_launch_id=$LaunchId
                    source='rust_recorder_stderr'; offset_ms=$offset; text=$shortMessage; instruction_zh=$message
                    stream_number=$streamNumber; speech_return_qpc=$returnedQpc
                    received_qpc=$receivedQpc; speech_request_qpc=$requestQpc
                    qpc_frequency=[Diagnostics.Stopwatch]::Frequency
                    actual_audio_onset_measured=$false; authoritative_action_marker=$false
                } | ConvertTo-Json -Compress))
                # Bounded completion, never a new action timer. This is SAPI completion only;
                # Bluetooth/device/acoustic latency remains unmeasured. Raw Input retains QPC.
                $completed = $speech.WaitUntilDone(1500)
                $completedQpc = [Diagnostics.Stopwatch]::GetTimestamp()
                $elapsedMs = 1000.0 * ($completedQpc - $requestQpc) / [Diagnostics.Stopwatch]::Frequency
                $spoken.WriteLine((@{
                    schema='local-cp2077-spoken-cue/v2'; event='completion'; process_launch_id=$LaunchId
                    offset_ms=$offset; stream_number=$streamNumber; sapi_completed=$completed
                    speech_completion_observed_qpc=$completedQpc; request_to_completion_ms=$elapsedMs
                    qpc_frequency=[Diagnostics.Stopwatch]::Frequency
                    actual_audio_onset_measured=$false; authoritative_action_marker=$false
                } | ConvertTo-Json -Compress))
                if (-not $completed -or $elapsedMs -gt 1500) {
                    [void]$speech.Speak('',3)
                    throw 'Short action cue exceeded the 1500 ms SAPI completion budget'
                }
                $count += 1
            }
        }
        $process.WaitForExit()
        $stdout.Write($stdoutTask.GetAwaiter().GetResult())
        if ($count -ne $cues.Count) { [void]$speech.Speak('采集已停止，请松开所有键，等待检查。',0) }
        $result = @{ recorder_exit_code=$process.ExitCode; spoken_cue_count=$count; capture_passed=($process.ExitCode -eq 0 -and $count -eq $cues.Count); input_injected=$false }
        $resultWriter = New-LogWriter 'coordinator-exit.json'
        try { $resultWriter.WriteLine(($result | ConvertTo-Json)) } finally { $resultWriter.Dispose() }
        $result | ConvertTo-Json
        if ($process.ExitCode -eq 0 -and $count -ne $cues.Count) { throw 'Recorder ended without the complete cue sequence' }
        exit $process.ExitCode
    } catch {
        # Ask the probe to stop via its existing same-launch control file, never kill
        # the game. RGB retains its original bounded 64-second deadline and logs.
        $presentationError = $_.Exception.Message
        $stopPath = Join-Path $session 'raw-depth-demo-stop-request.json'
        try {
            if ((Test-Path -LiteralPath (Join-Path $session 'raw-depth-demo-arm-request.json')) -and -not (Test-Path -LiteralPath $stopPath)) {
                $stopBytes = $encoding.GetBytes((@{schema='dm-cp2077-raw-depth-demo-stop-request/v1'; process_launch_id=$LaunchId} | ConvertTo-Json -Compress))
                $temporaryStop = $stopPath + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
                $stopStream = [IO.File]::Open($temporaryStop,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
                try { $stopStream.Write($stopBytes,0,$stopBytes.Length); $stopStream.Flush($true) } finally { $stopStream.Dispose() }
                [IO.File]::Move($temporaryStop,$stopPath)
            }
        } catch { Write-Warning "Could not publish stop request: $($_.Exception.Message)" }
        # An auxiliary log failure must never prevent the preceding depth-stop request.
        try {
            $failedResultPath = Join-Path $session 'coordinator-exit.json'
            if (-not (Test-Path -LiteralPath $failedResultPath)) {
                $failedWriter = New-LogWriter 'coordinator-exit.json'
                try {
                    $failedWriter.WriteLine((@{ capture_passed=$false; presentation_error=$presentationError;
                        completed_sapi_cue_count=$count; input_injected=$false; recorder_exit_code=$null } | ConvertTo-Json))
                } finally { $failedWriter.Dispose() }
            }
        } catch { Write-Warning "Could not write failed presentation result: $($_.Exception.Message)" }
        try { [void]$speech.Speak('动作提示异常，请松开所有键。录制结果需要检查。',0) } catch { Write-Warning 'Speech failure while stopping' }
        throw
    }
} finally {
    foreach ($resource in @($stderr,$spoken,$stdout,$process)) {
        if ($null -ne $resource) { $resource.Dispose() }
    }
    if ($null -ne $speech) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($speech) }
}

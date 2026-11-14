# Dot-source from the authoring controller. This supervises only the diagnostic helper,
# never the game. Its existing Raw Input monitor stops on loss of the bound foreground.
$ErrorActionPreference='Stop'

function Start-AuthoringInputWitness([string]$Recorder,[uint32]$GameProcessId,[string]$LaunchId,[string]$Directory) {
    if($LaunchId -cnotmatch '^[0-9a-f]{32}$' -or $GameProcessId -eq 0){throw 'Invalid input witness launch identity'}
    if(-not [IO.Path]::IsPathFullyQualified($Directory) -or (Test-Path -LiteralPath $Directory)){throw 'Input witness requires a new absolute directory'}
    foreach($path in @($Recorder,$Directory)){if($path.Contains('"')){throw 'Invalid quoted input witness path'}}
    foreach($path in @("$Directory.stdout.log","$Directory.stderr.log")){if(Test-Path -LiteralPath $path){throw 'Input witness was already attempted'}}
    $arguments=@('--observe-authoring-input','--pid',"$GameProcessId",'--launch-id',$LaunchId,'--duration-seconds','60','--output-dir',('"'+$Directory+'"'))
    $child=Start-Process -FilePath $Recorder -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput "$Directory.stdout.log" -RedirectStandardError "$Directory.stderr.log"
    $witness=[pscustomobject]@{Process=$child;Directory=$Directory;LaunchId=$LaunchId;GameProcessId=$GameProcessId;Ready=$null;Finished=$false}
    try {
        $deadline=[Diagnostics.Stopwatch]::StartNew()
        $readyPath=Join-Path $Directory 'ready.json'
        while(-not(Test-Path -LiteralPath $readyPath)){
            $child.Refresh()
            if($child.HasExited){throw 'Input witness exited before readiness; preserve logs'}
            if($deadline.Elapsed.TotalSeconds -ge 6){throw 'Input witness readiness timed out'}
            Start-Sleep -Milliseconds 50
        }
        $ready=Get-Content -LiteralPath $readyPath -Raw|ConvertFrom-Json
        if($ready.schema -cne 'dm-cp2077-authoring-input-ready/v1' -or $ready.process_launch_id -cne $LaunchId -or $ready.process_id -ne $GameProcessId -or $ready.qpc_frequency -ne [Diagnostics.Stopwatch]::Frequency -or $ready.qpc_start -le 0 -or $ready.qpc_deadline -le $ready.qpc_start -or -not $ready.non_trainable){throw 'Input witness readiness identity/clock mismatch'}
        $witness.Ready=$ready
        return $witness
    } catch {
        try { Stop-AuthoringInputWitness $witness | Out-Null } catch { Write-Warning $_.Exception.Message }
        throw
    }
}

function Assert-AuthoringInputWitness($Witness) {
    $Witness.Process.Refresh()
    if($Witness.Process.HasExited -or (Test-Path -LiteralPath (Join-Path $Witness.Directory 'result.json'))){throw 'Input witness stopped during the movement window'}
}

function Assert-AuthoringInputCoverage($Report,$Receipt) {
    if($Report.process_launch_id -cne $Receipt.request.process_launch_id -or $Report.qpc_frequency -le 0 -or $Report.qpc_frequency -ne $Receipt.qpc_frequency -or $Report.events.qpc_start -gt $Receipt.window_open_observed_qpc -or $Report.events.qpc_end -lt $Receipt.window_deadline_qpc -or $Receipt.window_open_observed_qpc -ge $Receipt.window_deadline_qpc){throw 'Input witness does not cover the same-launch native movement window'}
}

function Stop-AuthoringInputWitness($Witness) {
    if($Witness.Finished){throw 'Input witness has already been finalized'}
    try {
        $Witness.Process.Refresh()
        $stop=Join-Path $Witness.Directory 'stop-request.json'
        if(-not $Witness.Process.HasExited -and (Test-Path -LiteralPath $Witness.Directory) -and -not(Test-Path -LiteralPath $stop)){
            $prepared=Join-Path $Witness.Directory '.stop-request.prepared.json'
            $stream=[IO.File]::Open($prepared,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
            try {
                $bytes=[Text.Encoding]::UTF8.GetBytes((@{schema='dm-cp2077-authoring-input-stop/v1';process_launch_id=$Witness.LaunchId}|ConvertTo-Json -Compress))
                $stream.Write($bytes,0,$bytes.Length);$stream.Flush($true)
            } finally {$stream.Dispose()}
            [IO.File]::Move($prepared,$stop)
        }
        if(-not $Witness.Process.WaitForExit(6000)){
            # The Rust helper has its own 60-second cap as a second line of defence.
            $Witness.Process.Kill()
            $Witness.Process.WaitForExit(1000)|Out-Null
            throw 'Input witness did not finalize; killed only this helper, preserve partial evidence'
        }
        if($Witness.Process.ExitCode -ne 0){throw "Input witness failed ($($Witness.Process.ExitCode)); preserve partial evidence"}
        $report=Get-Content -LiteralPath (Join-Path $Witness.Directory 'result.json') -Raw|ConvertFrom-Json
        if($report.schema -cne 'dm-cp2077-authoring-input-witness/v1' -or $report.process_launch_id -cne $Witness.LaunchId -or $report.process_id -ne $Witness.GameProcessId -or -not $report.completed -or $report.error -or -not $report.non_trainable -or $report.recording_ready -or $report.operator_actions_verified -or $report.events.faulted -or $report.events.queue_overflows -ne 0 -or $report.events.boundary_reorder_drops -ne 0){throw 'Input witness report failed validation'}
        $events=Join-Path $Witness.Directory 'events/00000.events.jsonl.sealed'
        if([IO.Path]::GetFullPath([string]$report.events.path) -cne [IO.Path]::GetFullPath($events) -or $report.events.sha256 -cne ('sha256:'+(Get-FileHash -LiteralPath $events -Algorithm SHA256).Hash.ToLowerInvariant())){throw 'Input witness event identity/hash mismatch'}
        return $report
    } finally {
        $Witness.Finished=$true
        $Witness.Process.Dispose()
    }
}

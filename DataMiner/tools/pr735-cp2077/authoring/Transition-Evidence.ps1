# QPC evidence only; settings are not proof of an in-game projection or menu state.
function New-AuthoringTrace([string]$Path,[string]$LaunchId) {
 $stream=[IO.File]::Open($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
 $writer=[IO.StreamWriter]::new($stream,[Text.UTF8Encoding]::new($false));$writer.AutoFlush=$true
 return [pscustomobject]@{Writer=$writer;LaunchId=$LaunchId;Count=0}
}
function Write-AuthoringTrace($Trace,[string]$Event,$Details) {
 if($Trace.Count -ge 2000){throw 'Authoring controller evidence capacity exceeded'}
 $Trace.Writer.WriteLine(([ordered]@{schema='dm-cp2077-authoring-controller-event/v1';process_launch_id=$Trace.LaunchId;qpc=[Diagnostics.Stopwatch]::GetTimestamp();qpc_frequency=[Diagnostics.Stopwatch]::Frequency;event=$Event;details=$Details;non_trainable=$true}|ConvertTo-Json -Depth 15 -Compress))
 $Trace.Count++
}
function Publish-AuthoringGuidance([string]$Session,[string]$LaunchId,[int]$CompletedOrdinal,[Nullable[int]]$NextFov) {
 if($LaunchId -cnotmatch '^[0-9a-f]{32}$' -or $CompletedOrdinal -lt 1 -or $CompletedOrdinal -gt 8 -or ($null -ne $NextFov -and $NextFov -notin @(75,90))){throw 'Invalid authoring guidance'}
 foreach($n in @('raw-depth-demo-stop-request.json','raw-depth-telemetry.json')){if(Test-Path -LiteralPath (Join-Path $Session $n)){throw 'Cannot guide an ended session'}}
 $receipt=Get-Content -Raw -LiteralPath (Join-Path $Session ('camera-authoring-receipt-{0:00}.json' -f $CompletedOrdinal))|ConvertFrom-Json
 if($receipt.schema -cne 'dm-cp2077-camera-authoring-window-receipt/v2' -or $receipt.request.process_launch_id -cne $LaunchId -or $receipt.request.ordinal -ne $CompletedOrdinal -or $receipt.qpc_frequency -ne [Diagnostics.Stopwatch]::Frequency -or [Diagnostics.Stopwatch]::GetTimestamp() -le $receipt.window_deadline_qpc){throw 'Cannot guide before same-launch native window completion'}
 $dest=Join-Path $Session ('camera-authoring-guide-{0:00}.json' -f $CompletedOrdinal)
 if(Test-Path -LiteralPath $dest){throw 'Guidance was already attempted'}
 $tmp="$dest.prepared"
 $stream=[IO.File]::Open($tmp,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
 try {
  $bytes=[Text.UTF8Encoding]::new($false).GetBytes((@{schema='dm-cp2077-authoring-guidance/v1';process_launch_id=$LaunchId;completed_ordinal=$CompletedOrdinal;next_fov=$NextFov}|ConvertTo-Json -Compress))
  $stream.Write($bytes);$stream.Flush($true)
 } finally {$stream.Dispose()}
 [IO.File]::Move($tmp,$dest)
}

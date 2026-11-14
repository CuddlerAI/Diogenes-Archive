param([Parameter(Mandatory=$true)][string]$RunDirectory,[Parameter(Mandatory=$true)][ValidateSet('authoring-a','authoring-b')][string]$Stage,[Parameter(Mandatory=$true)][ValidateRange(1,3)][int]$Ordinal,[ValidateRange(10,10)][int]$ForegroundObservationSeconds=10,[switch]$FovOnly)
. (Join-Path (Join-Path $RunDirectory $Stage) 'common.ps1')
$launch=Get-Content -Raw -LiteralPath (Join-Path $Run 'launch.json')|ConvertFrom-Json
$Session=[string]$launch.session
. (Join-Path $PSScriptRoot 'Input-Witness.ps1')
. (Join-Path $PSScriptRoot 'Transition-Evidence.ps1')
$phaseTrace=$null
$inputWitness=$null
$inputReport=$null
trap {
 $failure=$_.Exception.Message
 if($phaseTrace){try{Write-AuthoringTrace $phaseTrace 'failure' @{error=$failure}}catch{Write-Warning $_.Exception.Message}finally{$phaseTrace.Writer.Dispose()}}
 if($inputWitness -and -not $inputWitness.Finished){try{Stop-AuthoringInputWitness $inputWitness|Out-Null}catch{Write-Warning $_.Exception.Message}}
 $stop=Join-Path $Session 'raw-depth-demo-stop-request.json'
 if(-not(Test-Path -LiteralPath $stop) -and -not(Test-Path -LiteralPath (Join-Path $Session 'raw-depth-telemetry.json'))){
  $prepared=Join-Path $Session '.authoring-controller-failed.stop.prepared'
  Write-NewJson $prepared ([ordered]@{schema='dm-cp2077-raw-depth-demo-stop-request/v1';process_launch_id=$launch.process_launch_id})
  [IO.File]::Move($prepared,$stop)
 }
 Write-Output "AUTHORING_FAILED: $failure; preserve this session and never re-arm it; stop file is published only when the probe has not already finalized"
 break
}
$phase=@('fov75-three-axis','fov90-still','fov75-return')[$Ordinal-1]
$expectedFov=@(75,90,75)[$Ordinal-1]
$duration=@(36000,4000,4000)[$Ordinal-1]
if($FovOnly){$duration=4000;$phase=@('fov75-still','fov90-still','fov75-return')[$Ordinal-1]}
$name='phase-{0:00}' -f $Ordinal
$phaseTrace=New-AuthoringTrace (Join-Path $Run "$name.transitions.jsonl") ([string]$launch.process_launch_id)
Write-AuthoringTrace $phaseTrace 'preparing' @{ordinal=$Ordinal;phase=$phase;duration_ms=$duration;fov_only=[bool]$FovOnly}
foreach($n in @('raw-depth-demo-stop-request.json','raw-depth-telemetry.json','render-camera-observation.json')){
 if(Test-Path -LiteralPath (Join-Path $Session $n)){throw 'Session already stopped or faulted; do not reuse'}
}
function Assert-LiveAuthoringGame {
 $current=Get-Process -Id ([uint32]$launch.process_id) -ErrorAction SilentlyContinue
 if(-not $current){throw 'CP2077_AUTHORING_GAME_EXITED: game process ended; preserve this session, no restart or re-arm'}
 if($current.StartTime.ToUniversalTime().Ticks -ne ([DateTime]$launch.process_start_utc).ToUniversalTime().Ticks){throw 'Game process was replaced'}
 return $current
}
$game=Assert-LiveAuthoringGame
$gate=Get-Content -Raw -LiteralPath (Join-Path $Run 'runtime-authoring.json')|ConvertFrom-Json
if($gate.status -ne 'pass' -or @($gate.blocking).Count){throw 'Runtime check failed'}
$module=@($game.Modules|Where-Object ModuleName -eq 'dm_depth_probe.dll')
if($module.Count -ne 1 -or (Get-FileHash -LiteralPath $module[0].FileName -Algorithm SHA256).Hash.ToLowerInvariant() -cne $ProbeSha){throw 'Resident DLL mismatch'}
function Publish-Control([string]$Leaf,$Value){
 $dest=Join-Path $Session $Leaf
 if(Test-Path -LiteralPath $dest){throw "Existing control: $Leaf"}
 $prepared=Join-Path $Session ('.'+$Leaf+'.prepared')
 Write-NewJson $prepared $Value
 [IO.File]::Move($prepared,$dest)
}
function Read-Setting($Settings,[string]$Group,[string]$Name){
 $g=@($Settings.data|Where-Object group_name -eq $Group)
 if($g.Count -ne 1){throw "Settings group mismatch $Group"}
 $v=@($g[0].options|Where-Object name -eq $Name)
 if($v.Count -ne 1){throw "Settings option mismatch $Name"}
 return $v[0].value
}
function Assert-CurrentPhaseSettings([string]$Checkpoint) {
 $observedQpc=[Diagnostics.Stopwatch]::GetTimestamp()
 $currentBytes=[IO.File]::ReadAllBytes($settingsPath)
 $currentSettings=[Text.Encoding]::UTF8.GetString($currentBytes)|ConvertFrom-Json
 $observed=[ordered]@{
  checkpoint=$Checkpoint;qpc=$observedQpc;qpc_frequency=[Diagnostics.Stopwatch]::Frequency
  expected_fov=$expectedFov;observed_fov=(Read-Setting $currentSettings '/graphics/basic' 'FieldOfView')
  resolution=(Read-Setting $currentSettings '/video/display' 'Resolution')
  fps_cap=(Read-Setting $currentSettings '/video/display' 'MaximumFPS_Value')
  fps_cap_enabled=(Read-Setting $currentSettings '/video/display' 'MaximumFPS_OnOff')
  settings_sha256=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($currentBytes)).ToLowerInvariant()
  in_game_fov_verified=$false;non_trainable=$true
 }
 Write-AuthoringTrace $phaseTrace 'settings_observed' $observed
 if($observed.observed_fov -ne $expectedFov -or $observed.resolution -cne '1920x1080' -or $observed.fps_cap -ne 32 -or $observed.fps_cap_enabled -ne $true){
  Write-NewJson "$name.settings-changed.json" $observed
  throw "CP2077_AUTHORING_SETTINGS_CHANGED: $Checkpoint expected FOV $expectedFov, observed $($observed.observed_fov); preserve session, do not retry"
 }
 if($Checkpoint -in @('before-request','window-complete')){Write-NewJson "$name.settings-$Checkpoint.json" $observed}
}
$settingsPath=Join-Path $env:LOCALAPPDATA 'CD Projekt Red/Cyberpunk 2077/UserSettings.json'
$bytes=[IO.File]::ReadAllBytes($settingsPath)
$settings=[Text.Encoding]::UTF8.GetString($bytes)|ConvertFrom-Json
if((Read-Setting $settings '/graphics/basic' 'FieldOfView') -ne $expectedFov){throw "Expected persisted FOV $expectedFov; no window requested"}
if((Read-Setting $settings '/video/display' 'Resolution') -ne '1920x1080' -or (Read-Setting $settings '/video/display' 'MaximumFPS_Value') -ne 32 -or -not (Read-Setting $settings '/video/display' 'MaximumFPS_OnOff')){throw 'Resolution/FPS settings changed'}
$settingsCopy=Join-Path $Run "$name.UserSettings.json"
if(Test-Path -LiteralPath $settingsCopy){throw 'This phase was already attempted'}
$stream=[IO.File]::Open($settingsCopy,[IO.FileMode]::CreateNew)
try{$stream.Write($bytes,0,$bytes.Length);$stream.Flush($true)}finally{$stream.Dispose()}
# Give the operator time to return to the game; never activate a window or synthesize input.
Start-Sleep -Seconds 5
Invoke-Logged "$name.foreground" (Join-Path $Tools 'dm-cp2077-raw-demo-record.exe') @('--observe-window','--pid',"$($game.Id)",'--duration-seconds',"$ForegroundObservationSeconds",'--output',(Join-Path $Run "$name.foreground.json"))
$fg=Get-Content -Raw -LiteralPath (Join-Path $Run "$name.foreground.json")|ConvertFrom-Json
Write-AuthoringTrace $phaseTrace 'foreground_check_complete' @{report="$name.foreground.json";observation_error=$fg.observation_error;sample_count=@($fg.samples).Count}
if($fg.observation_error -or @($fg.samples).Count -lt 2 -or @($fg.samples|Where-Object{-not $_.target_is_foreground -or -not $_.target_revalidated}).Count){throw 'Foreground check failed; no authoring window sent'}
if($Ordinal -eq 1 -and -not $FovOnly){
 $inputWitness=Start-AuthoringInputWitness (Join-Path $Tools 'dm-cp2077-raw-demo-record.exe') $game.Id ([string]$launch.process_launch_id) (Join-Path $Run 'input-witness')
}
# Foreground observation is not proof that the FOV read before its 15-second
# preparation still applies. Never publish a window against that stale snapshot.
Assert-CurrentPhaseSettings 'before-request'
[void](Assert-LiveAuthoringGame)
$arm=Join-Path $Session 'raw-depth-demo-arm-request.json'
if($Ordinal -eq 1){
 if(Test-Path -LiteralPath $arm){throw 'Never re-arm an attempted session'}
 Publish-Control 'raw-depth-demo-arm-request.json' ([ordered]@{schema='dm-cp2077-raw-depth-demo-arm-request/v1';process_launch_id=$launch.process_launch_id;capture_contract='issue735_local';duration_ms=64000;target_depth_hz=16;manual_stop=$true})
}else{
 if(-not(Test-Path -LiteralPath $arm)){throw 'Same-launch initial arm is missing'}
}
$requestName='camera-authoring-window-{0:00}.json' -f $Ordinal
Publish-Control $requestName ([ordered]@{schema='dm-cp2077-camera-authoring-window/v2';source_policy='revalidate-at-drained-boundary';process_launch_id=$launch.process_launch_id;ordinal=$Ordinal;phase=$phase;duration_ms=$duration})
Write-AuthoringTrace $phaseTrace 'window_request_published' @{ordinal=$Ordinal;request=$requestName}
$receiptPath=Join-Path $Session ('camera-authoring-receipt-{0:00}.json' -f $Ordinal)
$waitUntil=[DateTime]::UtcNow.AddSeconds(10)
while(-not(Test-Path -LiteralPath $receiptPath)){
 [void](Assert-LiveAuthoringGame)
 if($inputWitness){Assert-AuthoringInputWitness $inputWitness}
 Assert-CurrentPhaseSettings 'waiting-receipt'
 if([DateTime]::UtcNow -gt $waitUntil){throw 'Native authoring receipt missing; preserve session'}
 if(Test-Path -LiteralPath (Join-Path $Session 'raw-depth-telemetry.json')){throw 'Probe stopped before acknowledging authoring window'}
 Start-Sleep -Milliseconds 100
}
$receipt=Get-Content -Raw -LiteralPath $receiptPath|ConvertFrom-Json
[void](Assert-LiveAuthoringGame)
Assert-CurrentPhaseSettings 'receipt-observed'
if($receipt.schema -cne 'dm-cp2077-camera-authoring-window-receipt/v2' -or $receipt.request.source_policy -cne 'revalidate-at-drained-boundary' -or -not $receipt.previous_work_drained -or $receipt.writer_barrier_ordinal -ne $Ordinal -or -not $receipt.source_binding.runtime_candidate_id -or $receipt.source_binding.prepared_present -le 0 -or $receipt.source_binding.prepared_qpc -gt $receipt.window_open_observed_qpc -or $receipt.window_open_observed_qpc -ge $receipt.window_deadline_qpc){throw 'Native source preparation/phase receipt mismatch'}

if($receipt.request.process_launch_id -cne $launch.process_launch_id -or $receipt.request.ordinal -ne $Ordinal -or $receipt.request_sha256 -cne ('sha256:'+(Get-FileHash -LiteralPath (Join-Path $Session $requestName) -Algorithm SHA256).Hash.ToLowerInvariant())){throw 'Native authoring receipt identity/hash mismatch'}
Write-Output "NATIVE_WINDOW_STARTED $phase duration_ms=$duration"
Write-AuthoringTrace $phaseTrace 'native_receipt_observed' $receipt
$end=[long]$receipt.window_deadline_qpc
$nextSettingsCheck=0L
while([Diagnostics.Stopwatch]::GetTimestamp() -le ($end+2*[long]$receipt.qpc_frequency)){
 [void](Assert-LiveAuthoringGame)
 if($inputWitness){Assert-AuthoringInputWitness $inputWitness}
 $settingsNow=[Diagnostics.Stopwatch]::GetTimestamp()
 if($settingsNow -ge $nextSettingsCheck){Assert-CurrentPhaseSettings 'window-active';$nextSettingsCheck=$settingsNow+[long]([Diagnostics.Stopwatch]::Frequency/4)}
 if(Test-Path -LiteralPath (Join-Path $Session 'raw-depth-telemetry.json')){throw 'Probe stopped during authoring window; preserve session'}
 Start-Sleep -Milliseconds 100
}
Assert-CurrentPhaseSettings 'window-complete'
[void](Assert-LiveAuthoringGame)
if($inputWitness){
 $inputReport=Stop-AuthoringInputWitness $inputWitness
 Assert-AuthoringInputCoverage $inputReport $receipt
}
Write-NewJson "$name.controller.json" ([ordered]@{phase=$phase;ordinal=$Ordinal;settings_snapshot=$settingsCopy;settings_sha256=(Get-FileHash -LiteralPath $settingsCopy -Algorithm SHA256).Hash.ToLowerInvariant();expected_fov=$expectedFov;native_receipt=$receipt;controller_observed_end_qpc=[Diagnostics.Stopwatch]::GetTimestamp();operator_actions_verified=$false;non_trainable=$true;input_events_recorded=($null -ne $inputReport);input_witness=$inputReport;input_witness_report_sha256=$(if($inputReport){'sha256:'+(Get-FileHash -LiteralPath (Join-Path $Run 'input-witness/result.json') -Algorithm SHA256).Hash.ToLowerInvariant()}else{$null});production_binding_authorized=$false})
Write-Output "WINDOW_COMPLETE $phase; source/motion validity still requires offline analysis"
Write-AuthoringTrace $phaseTrace 'controller_complete' @{ordinal=$Ordinal;native_deadline_qpc=$receipt.window_deadline_qpc}
$phaseTrace.Writer.Dispose()

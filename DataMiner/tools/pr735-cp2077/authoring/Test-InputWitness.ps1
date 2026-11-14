param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'Input-Witness.ps1')
if(Test-Path -LiteralPath $OutputDirectory){throw 'Test output must be new'}
[IO.Directory]::CreateDirectory($OutputDirectory)|Out-Null
$launch='0123456789abcdef0123456789abcdef'
$checks=[Collections.Generic.List[string]]::new()
function Check([bool]$Ok,[string]$Name){if(-not $Ok){throw "FAILED: $Name"};$checks.Add($Name)}
function New-FakeWitness([string]$Name,[string]$Mode='ok') {
    $dir=Join-Path $OutputDirectory $Name
    [IO.Directory]::CreateDirectory((Join-Path $dir 'events'))|Out-Null
    $events=Join-Path $dir 'events/00000.events.jsonl.sealed'
    [IO.File]::WriteAllText($events,'synthetic fixture, not captured input')
    $report=@{
        schema='dm-cp2077-authoring-input-witness/v1';process_launch_id=$launch;process_id=123;
        completed=$true;error=$null;non_trainable=$true;recording_ready=$false;operator_actions_verified=$false;
        events=@{path=$events;sha256=('sha256:'+(Get-FileHash -LiteralPath $events).Hash.ToLowerInvariant());faulted=$false;queue_overflows=0;boundary_reorder_drops=0}
    }
    switch($Mode){
        'foreign' {$report.process_launch_id='ffffffffffffffffffffffffffffffff'}
        'fault' {$report.events.faulted=$true}
        'overflow' {$report.events.queue_overflows=1}
        'hash' {$report.events.sha256='sha256:'+('0'*64)}
        'unreviewed-claim' {$report.operator_actions_verified=$true}
        'partial' {$report.completed=$false;$report.error='lost foreground'}
    }
    [IO.File]::WriteAllText((Join-Path $dir 'result.json'),($report|ConvertTo-Json -Depth 5))
    $process=[pscustomobject]@{HasExited=$false;ExitCode=0;Killed=$false;Disposed=$false;Mode=$Mode}
    if($Mode -eq 'exit'){$process.ExitCode=7}
    $process|Add-Member ScriptMethod Refresh {}
    $process|Add-Member ScriptMethod WaitForExit {param($Milliseconds) return $this.Mode -ne 'hung'}
    $process|Add-Member ScriptMethod Kill {$this.Killed=$true}
    $process|Add-Member ScriptMethod Dispose {$this.Disposed=$true}
    [pscustomobject]@{Process=$process;Directory=$dir;LaunchId=$launch;GameProcessId=123;Ready=$null;Finished=$false}
}
$w=New-FakeWitness 'good'
$r=Stop-AuthoringInputWitness $w
Check ($r.completed -and $w.Finished -and $w.Process.Disposed) 'successful stop returns sealed evidence and disposes helper'
$stop=Get-Content -LiteralPath (Join-Path $w.Directory 'stop-request.json') -Raw|ConvertFrom-Json
Check ($stop.schema -ceq 'dm-cp2077-authoring-input-stop/v1' -and $stop.process_launch_id -ceq $launch) 'atomic stop carries the exact launch identity'
Check (-not(Test-Path -LiteralPath (Join-Path $w.Directory '.stop-request.prepared.json'))) 'prepared stop is atomically renamed'
$failed=$false
try{Stop-AuthoringInputWitness $w|Out-Null}catch{$failed=$true}
Check $failed 'a finalized witness is never stopped or reused again'
foreach($mode in @('foreign','fault','overflow','hash','unreviewed-claim','partial','exit','hung')){
    $w=New-FakeWitness $mode $mode
    $before=(Get-FileHash -LiteralPath (Join-Path $w.Directory 'events/00000.events.jsonl.sealed')).Hash
    $failed=$false
    try{Stop-AuthoringInputWitness $w|Out-Null}catch{$failed=$true}
    Check ($failed -and $w.Finished -and $w.Process.Disposed) "$mode is rejected and the helper handle is disposed"
    Check ($before -ceq (Get-FileHash -LiteralPath (Join-Path $w.Directory 'events/00000.events.jsonl.sealed')).Hash) "$mode retains original witness bytes"
    Check ($w.Process.Killed -eq ($mode -eq 'hung')) "$mode only terminates the supervised helper on timeout"
}
$w=New-FakeWitness 'early-exit'
$w.Process.HasExited=$true
$failed=$false
try{Assert-AuthoringInputWitness $w}catch{$failed=$true}
Check $failed 'a helper exit during the phase prevents continuation'
$coverage=[pscustomobject]@{process_launch_id=$launch;qpc_frequency=1000;events=@{qpc_start=100;qpc_end=600}}
$receipt=[pscustomobject]@{request=@{process_launch_id=$launch};qpc_frequency=1000;window_open_observed_qpc=200;window_deadline_qpc=500}
Assert-AuthoringInputCoverage $coverage $receipt
Check $true 'witness covers the full native movement window'
foreach($mode in @('late-start','early-end','clock','launch')){
    $changed=$coverage|ConvertTo-Json|ConvertFrom-Json
    switch($mode){
        'late-start' {$changed.events.qpc_start=201}
        'early-end' {$changed.events.qpc_end=499}
        'clock' {$changed.qpc_frequency=999}
        'launch' {$changed.process_launch_id='foreign'}
    }
    $failed=$false
    try{Assert-AuthoringInputCoverage $changed $receipt}catch{$failed=$true}
    Check $failed "$mode cannot masquerade as complete movement coverage"
}
[ordered]@{schema='local-cp2077-input-witness-tests/v1';passed=$checks.Count;checks=$checks;native_game_run=$false;hardware_verified=$false;non_trainable=$true}|ConvertTo-Json -Depth 5

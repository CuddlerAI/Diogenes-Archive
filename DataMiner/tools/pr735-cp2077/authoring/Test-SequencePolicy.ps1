$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'Sequence-Policy.ps1')
$checks=[System.Collections.Generic.List[string]]::new()
function Check([bool]$Condition,[string]$Name){if(-not $Condition){throw "FAILED $Name"};$checks.Add($Name)}
$policy=Get-Cp2077SequencePolicy
Check ($policy.TotalSeconds -lt $policy.NativeArmSafetyCeilingSeconds) 'controller budget is below unchanged native safety ceiling'
Check ((Get-Cp2077LatestPhaseStart 2) -eq 100) 'FOV 90 deadline reserves both later windows and stop'
Check ((Get-Cp2077LatestPhaseStart 3) -eq 140) 'FOV 75 return deadline reserves final window and stop'
$stable=@{Hash=$null;Since=$null}
Check (-not(Test-Cp2077FovSettled $stable 0 75 'a' 90)) 'wrong FOV never starts a phase'
Check (-not(Test-Cp2077FovSettled $stable 1 90 'b' 90)) 'first matching file does not trigger immediately'
Check (-not(Test-Cp2077FovSettled $stable 10.9 90 'b' 90)) 'settling requires ten full seconds'
Check (Test-Cp2077FovSettled $stable 11 90 'b' 90) 'stable matching setting opens controller gate'
Check (-not(Test-Cp2077FovSettled $stable 11.1 $null '' 90)) 'unreadable rewrite resets settled state'
Check (-not(Test-Cp2077FovSettled $stable 12 90 'b' 90)) 'same bytes after unreadable interval require a fresh wait'
Check (-not(Test-Cp2077FovSettled $stable 20 90 'c' 90)) 'new content resets stable interval'
Check (Test-Cp2077FovSettled $stable 30 90 'c' 90) 'new stable content can finish later'
Check (-not(Test-Cp2077FovSettled $stable 31 75 'd' 90)) 'changing back before expected phase cancels readiness'

function Run-Scenario([string]$Mode){
    $state=@{Now=0.0;Captures=[System.Collections.Generic.List[int]]::new();Fovs=[System.Collections.Generic.List[int]]::new();Stops=0;Completed=$false}
    $caught=$null
    try {
        Invoke-Cp2077SequenceCore @{
            Now={ $state.Now }
            Record={param($Name,$Details) if($Name -eq 'sequence_complete'){$state.Completed=$true}}
            WaitFov={param($Fov,$Deadline)
                $state.Fovs.Add($Fov)
                if($Mode -eq 'late-fov'){$state.Now=$Deadline;throw 'operator setting was not ready in remaining budget'}
                if($Mode -eq 'native-stopped'){throw 'native session already stopped'}
                $state.Now+=$(if($Mode -eq 'slow-menu'){28}else{20})
            }
            Capture={param($Ordinal,$Deadline)
                $state.Captures.Add($Ordinal)
                if($Mode -eq 'phase-failed' -and $Ordinal -eq 2){throw 'simulated failed source receipt'}
                if($Mode -eq 'phase-overrun' -and $Ordinal -eq 1){$state.Now=$Deadline+0.01;return}
                $state.Now+=@(53,21,21)[$Ordinal-1]
            }
            Stop={
                $state.Stops++
                if($Mode -eq 'stop-failed'){throw 'simulated report finalization failure'}
                $state.Now+=4
            }
        }
    } catch { $caught=$_.Exception.Message }
    return [pscustomobject]@{State=$state;Error=$caught}
}
$happy=Run-Scenario 'happy'
Check (-not $happy.Error -and $happy.State.Completed) 'normal continuous sequence completes without intermediate chat'
Check (($happy.State.Captures -join ',') -ceq '1,2,3') 'each ordinal executes exactly once'
Check (($happy.State.Fovs -join ',') -ceq '90,75') 'FOV transitions occur in order'
Check ($happy.State.Now -eq 139 -and $happy.State.Stops -eq 1) 'representative full sequence stops at 139 seconds'
$slow=Run-Scenario 'slow-menu'
Check (-not $slow.Error -and $slow.State.Completed -and $slow.State.Now -eq 155) 'slow menu transitions within remaining budget complete without re-arm'
foreach($mode in @('late-fov','native-stopped','phase-failed','phase-overrun','stop-failed')){
    $failed=Run-Scenario $mode
    Check ([bool]$failed.Error -and -not $failed.State.Completed) "$mode remains failed"
    Check ($failed.State.Stops -eq 1) "$mode attempts stop exactly once"
    Check (@($failed.State.Captures|Group-Object|Where-Object Count -gt 1).Count -eq 0) "$mode never retries an ordinal or re-arms"
    if($mode -ne 'stop-failed'){Check (-not $failed.State.Captures.Contains(3)) "$mode cannot continue to phase 3"}
}
$parseErrors=@()
foreach($file in @('Sequence-Policy.ps1','Invoke-AuthoringSequence.ps1','Test-SequencePolicy.ps1')){
    $tokens=$null;$errors=$null
    [void][Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot $file),[ref]$tokens,[ref]$errors)
    $parseErrors+=@($errors)
}
Check ($parseErrors.Count -eq 0) 'all controller scripts parse under PowerShell 7'
[ordered]@{schema='local-cp2077-sequence-policy-tests/v1';passed=$checks.Count;checks=$checks;native_game_run=$false;hardware_verified=$false;non_trainable=$true} | ConvertTo-Json -Depth 6

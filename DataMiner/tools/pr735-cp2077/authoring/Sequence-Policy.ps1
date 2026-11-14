$ErrorActionPreference='Stop'

function Get-Cp2077SequencePolicy {
    [pscustomobject]@{
        Schema='local-cp2077-authoring-sequence-policy/v1'
        TotalSeconds=200
        PhaseSeconds=@(70,40,40)
        StopReserveSeconds=20
        SettingsStableSeconds=10
        NativeArmSafetyCeilingSeconds=240
    }
}

function Get-Cp2077LatestPhaseStart([int]$Ordinal) {
    if($Ordinal -lt 1 -or $Ordinal -gt 3){throw 'Invalid sequence ordinal'}
    $policy=Get-Cp2077SequencePolicy
    $remaining=0
    for($i=$Ordinal-1;$i -lt 3;$i++){$remaining+=$policy.PhaseSeconds[$i]}
    return $policy.TotalSeconds-$policy.StopReserveSeconds-$remaining
}

function Test-Cp2077FovSettled($State,[double]$Now,[Nullable[double]]$Fov,[string]$ContentHash,[int]$ExpectedFov) {
    $policy=Get-Cp2077SequencePolicy
    if($null -eq $Fov -or $Fov -ne $ExpectedFov -or -not $ContentHash){
        $State.Since=$null;$State.Hash=$null
        return $false
    }
    if($State.Hash -cne $ContentHash -or $null -eq $State.Since){
        $State.Hash=$ContentHash;$State.Since=$Now
        return $false
    }
    if($Now -lt $State.Since){throw 'Monotonic clock went backwards'}
    return ($Now-$State.Since) -ge $policy.SettingsStableSeconds
}

function Invoke-Cp2077SequenceCore([hashtable]$Hooks) {
    $policy=Get-Cp2077SequencePolicy
    foreach($required in @('Now','Record','WaitFov','Capture','Stop')){
        if($Hooks[$required] -isnot [scriptblock]){throw "Missing sequence hook $required"}
    }
    $primaryError=$null
    $stopError=$null
    try {
        foreach($ordinal in 1..3){
            $latest=Get-Cp2077LatestPhaseStart $ordinal
            if($ordinal -gt 1){
                $expectedFov=@(75,90,75)[$ordinal-1]
                & $Hooks.Record 'waiting_for_fov' @{ordinal=$ordinal;expected_fov=$expectedFov;latest_start_seconds=$latest}
                & $Hooks.WaitFov $expectedFov $latest
            }
            $now=& $Hooks.Now
            if($now -ge $latest){throw "CP2077_SEQUENCE_TIME_BUDGET: phase $ordinal cannot fit with remaining phases and stop reserve"}
            $finishBy=$now+$policy.PhaseSeconds[$ordinal-1]
            & $Hooks.Record 'starting_phase' @{ordinal=$ordinal;finish_by_seconds=$finishBy}
            & $Hooks.Capture $ordinal $finishBy
            if((& $Hooks.Now) -gt $finishBy){throw "CP2077_SEQUENCE_TIME_BUDGET: phase $ordinal exceeded its bound"}
            & $Hooks.Record 'phase_complete' @{ordinal=$ordinal}
        }
    } catch {
        $primaryError=$_.Exception.Message
    } finally {
        # Always request a bounded stop; no path calls arm twice or retries a failed phase.
        try { & $Hooks.Stop } catch { $stopError=$_.Exception.Message }
    }
    if($primaryError -or $stopError){throw "Sequence failure: phase=[$primaryError]; stop=[$stopError]"}
    & $Hooks.Record 'sequence_complete' @{non_trainable=$true;physical_evidence_verified=$false}
}

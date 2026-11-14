"""File-only controller regression. Mocks all game/process/foreground tools; launches only pwsh."""
from pathlib import Path
import argparse
import hashlib
import json
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
COMMON = r'''
$ErrorActionPreference='Stop'
$Run=$PSScriptRoot
$FixtureRoot=Split-Path -Parent $Run
$Session=Join-Path $FixtureRoot 'session'
$Tools=Join-Path $FixtureRoot 'tools'
$ProbeSha=(Get-FileHash -LiteralPath (Join-Path $Tools 'dm_depth_probe.dll')).Hash.ToLowerInvariant()
$env:LOCALAPPDATA=Join-Path $FixtureRoot 'local-data'
$FixtureSettings=Join-Path $env:LOCALAPPDATA 'CD Projekt Red/Cyberpunk 2077/UserSettings.json'
$FixtureMode=Get-Content -LiteralPath (Join-Path $FixtureRoot 'mode.txt') -Raw
function Change-FixtureFov {
 $j=Get-Content -LiteralPath $FixtureSettings -Raw|ConvertFrom-Json
 ($j.data|Where-Object group_name -eq '/graphics/basic').options[0].value=75
 [IO.File]::WriteAllText($FixtureSettings,($j|ConvertTo-Json -Depth 20),[Text.UTF8Encoding]::new($false))
}
function Get-Process {
 param([int]$Id)
 if(Test-Path -LiteralPath (Join-Path $FixtureRoot 'game-exited')){return $null}
 [pscustomobject]@{Id=42;StartTime=[DateTime]'2026-09-16T00:00:00Z';Modules=@([pscustomobject]@{ModuleName='dm_depth_probe.dll';FileName=(Join-Path $Tools 'dm_depth_probe.dll')})}
}
function Start-Sleep {
 param([int]$Seconds,[int]$Milliseconds)
 if($Milliseconds -gt 0 -and $FixtureMode -eq 'during-window'){
  Change-FixtureFov
  [Threading.Thread]::Sleep(10)
 }
 if($Milliseconds -gt 0 -and $FixtureMode -eq 'source-stopped'){
  [IO.File]::WriteAllText((Join-Path $Session 'raw-depth-telemetry.json'),'{}')
 }
 if($Milliseconds -gt 0 -and $FixtureMode -eq 'exit-during-window'){
  [IO.File]::WriteAllText((Join-Path $FixtureRoot 'game-exited'),'fixture')
 }
}
function Write-NewJson($Name,$Value) {
 $p=if([IO.Path]::IsPathRooted($Name)){$Name}else{Join-Path $Run $Name}
 $b=[Text.UTF8Encoding]::new($false).GetBytes(($Value|ConvertTo-Json -Depth 30))
 $s=[IO.File]::Open($p,[IO.FileMode]::CreateNew)
 try{$s.Write($b);$s.Flush($true)}finally{$s.Dispose()}
 if([IO.Path]::GetFileName($p) -match '^\.camera-authoring-window-(\d+)\.json.prepared$'){
  if($FixtureMode -eq 'source-stopped'){return}
  if($FixtureMode -eq 'exit-before-receipt'){
   [IO.File]::WriteAllText((Join-Path $FixtureRoot 'game-exited'),'fixture')
   return
  }
  $now=[Diagnostics.Stopwatch]::GetTimestamp();$freq=[Diagnostics.Stopwatch]::Frequency
  $receipt=[ordered]@{
   schema='dm-cp2077-camera-authoring-window-receipt/v2';request=$Value
   request_sha256=('sha256:'+(Get-FileHash -LiteralPath $p).Hash.ToLowerInvariant())
   previous_work_drained=$true;writer_barrier_ordinal=$Value.ordinal;qpc_frequency=$freq
   source_binding=@{runtime_candidate_id='synthetic-source';prepared_present=1;prepared_qpc=$now-6*$freq}
   window_open_observed_qpc=$now-5*$freq;window_deadline_qpc=$now-4*$freq
  }
  if($FixtureMode -in @('during-window','exit-during-window')){$receipt.window_deadline_qpc=$now+[long]($freq/5)}
  [IO.File]::WriteAllText((Join-Path $Session ('camera-authoring-receipt-{0:00}.json' -f $Value.ordinal)),($receipt|ConvertTo-Json -Depth 20),[Text.UTF8Encoding]::new($false))
  if($FixtureMode -eq 'after-request'){Change-FixtureFov}
 }
}
function Invoke-Logged($Name,$Exe,[string[]]$Arguments) {
 if($Arguments[0] -cne '--observe-window'){throw 'Unexpected executable action in offline stub'}
 $fg=@{observation_error=$null;samples=@(@{target_is_foreground=$true;target_revalidated=$true},@{target_is_foreground=$true;target_revalidated=$true})}
 [IO.File]::WriteAllText((Join-Path $Run ($Name+'.json')),($fg|ConvertTo-Json -Depth 5),[Text.UTF8Encoding]::new($false))
 if($FixtureMode -eq 'during-foreground'){Change-FixtureFov}
 if($FixtureMode -eq 'exit-before-arm'){
  [IO.File]::WriteAllText((Join-Path $FixtureRoot 'game-exited'),'fixture')
 }
}
'''


def save(path, value):
    path.write_text(json.dumps(value, indent=2), encoding='utf-8')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(output, pwsh):
    output.mkdir(parents=True, exist_ok=False)
    results = []
    for mode, ok, request, ordinal in [('during-foreground', False, False, 2), ('stable', True, True, 2),
        ('after-request', False, True, 2), ('during-window', False, True, 2),
        ('source-stopped', False, True, 2), ('fov-only', True, True, 1),
        ('exit-before-arm', False, False, 1), ('exit-before-receipt', False, True, 1),
        ('exit-during-window', False, True, 1)]:
        root=output/mode;root.mkdir()
        stage=root/'authoring-a';stage.mkdir()
        session=root/'session';session.mkdir()
        tools=root/'tools';tools.mkdir()
        (tools/'dm_depth_probe.dll').write_bytes(b'plain text fixture; not executable')
        settings=root/'local-data/CD Projekt Red/Cyberpunk 2077/UserSettings.json'
        settings.parent.mkdir(parents=True)
        save(settings,dict(data=[dict(group_name='/graphics/basic',options=[dict(name='FieldOfView',value=75 if ordinal==1 else 90)]),
            dict(group_name='/video/display',options=[dict(name='Resolution',value='1920x1080'),dict(name='MaximumFPS_Value',value=32),dict(name='MaximumFPS_OnOff',value=True)])]))
        (root/'mode.txt').write_text(mode)
        (stage/'common.ps1').write_text(COMMON,encoding='utf-8')
        save(stage/'launch.json',dict(process_id=42,process_start_utc='2026-09-16T00:00:00Z',process_launch_id='a'*32,session=str(session)))
        save(stage/'runtime-authoring.json',dict(status='pass',blocking=[]))
        arm=session/'raw-depth-demo-arm-request.json'
        if ordinal==2:arm.write_text('existing initial arm; immutable')
        arm_before=sha(arm) if ordinal==2 else None
        argv=[pwsh,'-NoProfile','-File',str(HERE/'Capture-AuthoringWindow.ps1'),'-RunDirectory',str(root),'-Stage','authoring-a','-Ordinal',str(ordinal)]
        if ordinal==1:argv.append('-FovOnly')
        p=subprocess.run(argv,capture_output=True,encoding='utf-8',errors='replace',timeout=25)
        (root/'stdout.log').write_text(p.stdout,encoding='utf-8');(root/'stderr.log').write_text(p.stderr,encoding='utf-8')
        name=f'phase-{ordinal:02d}'
        emitted=(session/f'camera-authoring-window-{ordinal:02d}.json').exists()
        assert (p.returncode==0)==ok and emitted==request,(mode,p.returncode,p.stdout,p.stderr)
        if ordinal==2:assert sha(arm)==arm_before
        events=[json.loads(line) for line in (stage/f'{name}.transitions.jsonl').read_text().splitlines()]
        assert events and all(e['process_launch_id']=='a'*32 and e['qpc']>0 for e in events)
        if not ok:
            assert not (stage/f'{name}.controller.json').exists()
            assert events[-1]['event']=='failure'
            if mode!='source-stopped':assert (session/'raw-depth-demo-stop-request.json').exists()
        else:assert events[-1]['event']=='controller_complete'
        if mode=='fov-only':
            assert json.loads((session/'camera-authoring-window-01.json').read_text())['duration_ms']==4000
            assert not (stage/'input-witness').exists()
        if mode.startswith('exit-'):
            assert 'CP2077_AUTHORING_GAME_EXITED' in p.stdout+p.stderr
            assert arm.exists()==request
        results.append(dict(case=mode,exit_code=p.returncode,window_published=emitted,expected_outcome=True,game_launched=False))
    retired=output/'retired-sequence';retired.mkdir()
    p=subprocess.run([pwsh,'-NoProfile','-File',str(HERE/'Invoke-AuthoringSequence.ps1'),
        '-RunDirectory',str(retired),'-Stage','authoring-a','-FovOnly'],
        capture_output=True,encoding='utf-8',errors='replace',timeout=10)
    assert p.returncode!=0 and 'CP2077_AUTHORING_SEQUENCE_DISABLED' in p.stdout+p.stderr
    assert not list(retired.iterdir()), 'Retired sequence must reject before touching a bundle'
    results.append(dict(case='retired-sequence',exit_code=p.returncode,window_published=False,
        expected_outcome=True,game_launched=False))
    save(output/'result.json',dict(synthetic=True,non_trainable=True,results=results))
    print(json.dumps(results))


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--pwsh',default=shutil.which('pwsh'))
    args=parser.parse_args()
    if not args.pwsh:parser.error('PowerShell 7 is required')
    run(args.output.resolve(),args.pwsh)

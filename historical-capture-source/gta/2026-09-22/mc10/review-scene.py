"""Read-only review of a finalized 60s four-lane internal scene; never a gate."""
from __future__ import annotations
import argparse
from collections import Counter
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import types

# All writes belong to the new batch, including avoiding imported-code pycache.
sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
CAPTURE = BASE.parents[1]
MEDIA_HELPER = CAPTURE / 'out/mc2-20260908/build-review-media.py'
sys.path.insert(0, str(CAPTURE))
from gta_capture.evidence import validate_alignment, _timestamp
from gta_capture.gate import _verified_replay_binding
from gta_capture.media import _verify_muxed_output
from gta_capture.qc_bridge import _current_run_binding
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route
from gta_capture.tape import load_tape


def record(path):
    path=Path(path).resolve();digest=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024*1024),b''):digest.update(chunk)
    return {'path':str(path),'bytes':path.stat().st_size,'sha256':digest.hexdigest()}


def need(condition, message):
    if not condition:raise ValueError(message)


def load(path, max_bytes=32*1024*1024):
    path=Path(path)
    need(path.stat().st_size<=max_bytes, f'JSON input exceeds bounded size: {path.name}')
    return json.loads(path.read_text(encoding='utf-8'))


def samepath(a,b):return os.path.normcase(str(Path(a).resolve()))==os.path.normcase(str(Path(b).resolve()))


def safe_paths(run_dir, out, *, base=BASE):
    root=Path(run_dir).resolve();target=Path(out).resolve();base=Path(base).resolve()
    need(root.is_dir(), 'run_dir must be an existing directory')
    need(target!=base and target.is_relative_to(base), 'out must be a new directory inside out/mc10-20260908')
    need(not target.exists(), 'out already exists; use a new review directory')
    need(not target.is_relative_to(root) and not root.is_relative_to(target), 'out must not overlap the input run')
    need((root/'run-manifest.json').is_file(), 'wait for real finalize: run-manifest.json is absent')
    return root,target


def finalized_manifest(root):
    manifest=load(root/'run-manifest.json')
    need(manifest.get('schema')=='dm-gta-run-manifest/1' and bool(manifest.get('completed_at'))
         and manifest.get('dry_run') is False and manifest.get('mode')=='real-staged-operator-gated',
         'only a completed real staged manifest is accepted')
    need(manifest.get('delivery_eligible') is False, 'this helper is restricted to non-delivery internal demos')
    need(len(manifest.get('delivery_files',[]))==12 and len(manifest.get('metadata_files',[]))==8,
         'finalized manifest must seal all twelve core files, including eight metadata files')
    return manifest


def load_media_helper():
    data=MEDIA_HELPER.read_bytes();module=types.ModuleType('existing_review_media')
    module.__file__=str(MEDIA_HELPER)
    exec(compile(data,str(MEDIA_HELPER),'exec'),module.__dict__)
    return module,{'path':str(MEDIA_HELPER),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}


def probe_contract(value):
    _verify_muxed_output(value,n_frames=1800,fps=30,expected_duration_s=60)
    video=next(s for s in value['streams'] if s.get('codec_type')=='video')
    audio=next(s for s in value['streams'] if s.get('codec_type')=='audio')
    need((video.get('width'),video.get('height'))==(1920,1080), 'video must decode at 1920x1080')
    need(video.get('codec_name')=='h264' and video.get('pix_fmt')=='yuv420p', 'video codec/pixel format differs from existing output contract')
    need(audio.get('codec_name')=='aac', 'audio codec must be AAC')
    return {'decoded_frames':int(video['nb_read_frames']),'width':1920,'height':1080,
            'fps':video['avg_frame_rate'],'video_duration_s':float(video['duration']),
            'container_duration_s':float(value['format']['duration']),'codec':'h264','pixel_format':'yuv420p',
            'audio_codec':'aac','audio_sample_rate':48000,'audio_channels':2}


def decode_video(tool, path):
    command=[str(tool),'-v','error','-threads','1','-count_frames','-show_streams','-show_format','-of','json',str(path)]
    result=subprocess.run(command,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                          text=True,encoding='utf-8',errors='replace',timeout=300)
    need(result.returncode==0, f'ffprobe decode failed: {result.stderr[-2000:]}')
    value=json.loads(result.stdout)
    return probe_contract(value),value,command


def vector_close(a,b,tolerance=5.1e-7):
    return (isinstance(a,list) and len(a)==len(b) and
            all(isinstance(x,(float,int)) and not isinstance(x,bool) and math.isfinite(x)
                and abs(x-y)<=tolerance for x,y in zip(a,b)))


def metadata_check(rows, events, epoch, *, lane):
    need(isinstance(rows,list) and len(rows)==1800, f'lane {lane} metadata must contain 1800 frames')
    for frame,(row,event) in enumerate(zip(rows,events)):
        camera=event['final_camera'];p=camera['position'];q=camera['quaternion'];qn=math.sqrt(sum(x*x for x in q))
        need(row.get('frame')==frame and row.get('fps')==30 and row.get('time')==_timestamp(epoch,event['source_t_ns']), f'lane {lane} metadata clock/index {frame}')
        need(vector_close(row.get('camera_position'),[p[0],p[2],p[1]]) and
             vector_close(row.get('camera_rotation_quaternion'),[-q[0]/qn,-q[2]/qn,-q[1]/qn,q[3]/qn]), f'lane {lane} world-pose metadata mismatch {frame}')
        need(all(abs(row['camera_intrinsics'][k]-camera['intrinsics'][k])<=5.1e-7 for k in ('fx','fy','cx','cy')), f'lane {lane} native-control K mismatch {frame}')
        if lane==1:
            p=event['player_state']['position'];q=event['player_state']['quaternion'];qn=math.sqrt(sum(x*x for x in q))
            need(vector_close(row.get('player_position'),[p[0],p[2],p[1]]) and
                 vector_close(row.get('player_rotation_quaternion'),[-q[0]/qn,-q[2]/qn,-q[1]/qn,q[3]/qn]), f'lane 1 recording-side player metadata mismatch {frame}')


def review(root,out,manifest):
    out.mkdir(parents=True,exist_ok=False)
    inputs={};report={'schema':'dm-gta-mc10-scene-review/1','run_dir':str(root),'run_id':manifest['run_id'],
        'integrity_status':'RUNNING','formal_quality_gate':False,'human_review_approval':False,
        'subject_visibility_verified':False,'visual_review_status':'NOT_REVIEWED',
        'calibration_approval':False,'editor_equivalence_verified':False,'delivery_eligible':False,
        'non_trainable':True,'granted_rules':[],'checks':{},'errors':[],'videos':{},'contact_sheets':[],
        'limits':['Only finalized real inputs are read. Existing QC FAIL/SKIP are retained independently of integrity status.',
                  'Contact sheets contain 64 actual selected source frames; no automatic claim of subject visibility or human approval.',
                  'Position/quaternion checks compare metadata with original native events; K remains native FOV-control readback, not calibrated intrinsics.',
                  'Recording-side player telemetry is not independent replay-origin/bone-animation verification.',
                  'No pipe, UI, finalize, encode, source mutation or formal gate writer is called.']}
    def seal(path,expected_sha=None,expected_bytes=None):
        item=record(path)
        need(expected_sha is None or item['sha256']==expected_sha,f'hash mismatch: {Path(path).name}')
        need(expected_bytes is None or item['bytes']==expected_bytes,f'byte count mismatch: {Path(path).name}')
        prior=inputs.get(item['path']);need(prior is None or prior==item,f'input changed: {Path(path).name}')
        inputs[item['path']]=item;return item
    def checkpoint(name):print(name,flush=True);report['active_check']=name
    rows=None
    try:
        checkpoint('sealed manifest, source route and twelve core files')
        seal(root/'run-manifest.json')
        binding=_current_run_binding(root,manifest)
        for group in ('inputs','delivery_files','qc_sidecars'):
            for item in binding[group].values():inputs[item['path']]=item
        dependency=load(root/'config/dependency-manifest.json');tape=load_tape(root/'config/camera-tape.json',expected_duration_s=60)
        need(tape['n_frames']==1800 and tape['fps_num']==30 and tape['fps_den']==1, 'requires exactly four 1800-frame 30fps lanes')
        need(all(tape['lanes'][str(l)]['resolution']=={'width':1920,'height':1080} for l in range(1,5)), 'tape dimensions must be 1920x1080')
        policy=dependency['policy'];need(policy['purpose']=='internal_nontrainable_poc' and policy['non_trainable'] is True and policy['upload_enabled'] is False,'internal policy boundary differs')
        master=manifest['master'];new_sha=master['replay_sha256']
        _validate_scenario_tape_route(Scenario.load(root/'config/scenario.json'),tape,expected_replay_sha256=new_sha)
        need(master['mock'] is False and master['recording_start_verified'] is True and master['player_observation_count']==1800,'master was not genuinely recorded')
        for key in ('replay_path','source_clip_path'):seal(master[key],new_sha)
        seal(master['player_telemetry_path'],master['player_telemetry_sha256'])
        seal(root/'evidence/native-record-result.json');recorded=load(root/'evidence/native-record-result.json')
        # hello/probe are PRE-record snapshots and can legitimately name the previous master.
        need(all(master.get(k)==v for k,v in recorded['master'].items() if k!='editor_project_binding'),'finalized master differs from the new record result.master')
        recording_pid=recorded['hello']['process_id'];prior_controller_generation=recorded['hello']['controller_generation']
        need(isinstance(recording_pid,int) and recording_pid>0 and isinstance(prior_controller_generation,int) and prior_controller_generation>0,'record pre-snapshot has no valid process/controller generation')
        report['record_snapshot_semantics']='Only result.master identifies the new recording; prior hello/probe master fields may identify an older clip.'
        event_paths={};profile_pin=dependency['components']['replus_render_ini'];seal(profile_pin['path'],profile_pin['sha256'])
        render_text=Path(profile_pin['path']).read_text(encoding='utf-8-sig')
        roots=[line.split('=',1)[1].strip() for line in render_text.splitlines() if line.strip().startswith('RenderOutputFolder=')]
        need(len(roots)==1,'installed sealed Render.ini has no unique output root');output_root=roots[0]
        live_master_generation=None
        for lane in range(1,5):
            arm_path=root/f'evidence/native-lane-{lane}-arm.json';result_path=root/f'evidence/native-lane-{lane}-result.json'
            seal(arm_path);seal(result_path);arm=load(arm_path);result=load(result_path);artifact=manifest['lane_artifacts'][str(lane)]
            need(result['ok'] is True and arm['ok'] is True and result['lane']==artifact,'arm/collect/final manifest lane mismatch')
            need(arm['lane']['editor_session_id']==artifact['editor_session_id'] and arm['lane']['armed'] is True,'arm/collect session mismatch')
            if live_master_generation is None:
                live_master_generation=arm['hello']['master_record_generation']
                need(isinstance(live_master_generation,int) and live_master_generation>0,'ARM has no valid new-master record generation')
            for phase in (arm,result):
                for label in ('hello','probe'):
                    state=phase[label]
                    need(state['process_id']==state['master_process_id']==recording_pid and state['master_record_generation']==live_master_generation
                         and state['master_replay_sha256']==state['master_source_sha256']==new_sha and state['record_binding_ready'] is True
                         and state['online_session'] is False and state['story_mode'] is True,'new master live PID/generation/SHA binding failed')
                need(phase['hello']['controller_generation']==phase['probe']['controller_generation'],'hello/probe generation mismatch')
            need(result['hello']['controller_generation']>=arm['hello']['controller_generation']>=prior_controller_generation,'controller generation moved backward')
            event_path=root/f'evidence/frame-events-lane{lane}.jsonl'
            need(artifact['lane']==lane and samepath(artifact['frame_events_path'],event_path) and artifact['frame_count']==1800 and artifact['mock'] is False,'lane event seal points outside its finalized lane')
            need(event_path.stat().st_size<=8*1024*1024,'event file exceeds bounded 1800-frame envelope')
            seal(event_path,artifact['frame_events_sha256']);event_paths[lane]=str(event_path)
        report['checks']['manifest_master_and_sources']='PASS'
        checkpoint('all 7200 raw PNG hashes, native fence, master telemetry and actual clock')
        alignment,rows=validate_alignment(event_paths,tape,allow_mock=False,expected_replay_sha256=new_sha,
            expected_render_profile_sha256=profile_pin['sha256'],expected_output_root=output_root)
        report['alignment']=alignment.to_dict();need(alignment.ok,'; '.join(alignment.problems))
        need(alignment.lane_event_sha256==manifest['alignment']['lane_event_sha256'],'current events differ from finalized alignment')
        need(_verified_replay_binding(master['editor_project_binding'],new_sha,rows),'actual Editor project/replay sidecar binding failed')
        seal(master['editor_project_binding']['path'],master['editor_project_binding']['sha256'])
        for lane,events in rows.items():
            artifact=manifest['lane_artifacts'][str(lane)]
            need(all(e['editor_binding']['session_id']==artifact['editor_session_id'] and samepath(e['output_path'],artifact['frame_pattern']%i) for i,e in enumerate(events)),f'lane {lane} event/output pattern differs from collect')
        report['raw_pngs']={'count':sum(len(v) for v in rows.values()),'bytes':sum(e['output_bytes'] for v in rows.values() for e in v),'all_hashes_checked':True,'index_sources':{str(k):inputs[str(Path(v).resolve())] for k,v in event_paths.items()}}
        report['checks']['raw_fence_time_master']='PASS'
        checkpoint('metadata world coordinates and retained formal reports')
        for lane,events in rows.items():
            path=root/f'capture/action_camera_{lane}.json';metadata_check(load(path),events,master['source_epoch'],lane=lane)
            system=load(root/f'capture/systeminfo_{lane}.json')
            need(system['width']==1920 and system['height']==1080 and system['gameProcessName']==dependency['game'].get('process_name','GTA5.exe'),'systeminfo does not describe the actual output')
        report['checks']['metadata_roundtrip']='PASS'
        audio=manifest['audio'];seal(audio['audio_path'],audio['audio_sha256']);seal(audio['audio_report_path'],audio['audio_report_sha256'])
        gate=load(root/'evidence/gta-pr405-gate.json');seal(root/'evidence/gta-pr405-gate.json')
        core=lambda obj:{k:v for k,v in obj.items() if k!='run_binding'}
        need(core(gate)==core(manifest['gta_pr405_gate']),'formal gate changed from finalized manifest copy')
        if gate.get('run_binding') is not None:need(gate['run_binding']==binding,'bound gate source files changed')
        report['retained_gate']={'ok':gate['ok'],'passed_rules':gate['passed_rules'],'missing_rules':gate['missing_rules']}
        qc_path=root/'evidence/engine-qc.json'
        if qc_path.is_file():
            qc=load(qc_path);seal(qc_path)
            report['retained_qc']={'verdict':qc['verdict'],'counts':qc['counts'],'failures':[r for r in qc['results'] if r.get('status')=='FAIL']}
            bridge=qc.get('gta_capture_bridge')
            if bridge:
                for key in ('run_id','run_manifest','inputs','delivery_files','qc_sidecars'):need(bridge.get(key)==binding[key],f'QC bridge {key} input changed')
        else:report['retained_qc']={'status':'NOT_PRESENT','formal_review_not_inferred':True}
        report['retained_preflight']=manifest.get('preflight')
        report['checks']['sealed_reports']='PASS'
        checkpoint('actual sequential video decode counts')
        tool=dependency['components']['ffprobe'];seal(tool['path'],tool['sha256'])
        need(Path(tool['path']).name.casefold() in ('ffprobe','ffprobe.exe'),'pinned video probe must be ffprobe')
        for lane in range(1,5):
            print(f'Decode lane {lane}/4, one decoder thread',flush=True)
            path=root/f'capture/video_{lane}.mp4';summary,probe,command=decode_video(tool['path'],path)
            probe_path=out/f'video-{lane}-ffprobe.json';probe_path.write_text(json.dumps(probe,indent=2),encoding='utf-8')
            report['videos'][str(lane)]={'source':binding['delivery_files'][path.name],**summary,'probe':record(probe_path),'command':command}
        report['checks']['actual_video_decodes']='PASS'
    except PermissionError as exc:
        report['errors'].append({'check':report.get('active_check'),'kind':'permission_block','message':str(exc)})
    except Exception as exc:
        report['errors'].append({'check':report.get('active_check'),'kind':type(exc).__name__,'message':str(exc)})
    if rows is not None and report['checks'].get('raw_fence_time_master')=='PASS':
        try:
            checkpoint('existing helper: four 16-frame contact sheets')
            media,source=load_media_helper();indices=media.sample_indices(1800)
            need(len(indices)==16 and len(set(indices))==16,'existing helper must select 16 unique actual frames')
            contacts=out/'contact-sheets';contacts.mkdir()
            selected=[]
            for lane,events in rows.items():
                for frame in indices:
                    e=events[frame];need(media.sealed_png(e)==(1920,1080),'selected raw PNG has wrong dimensions')
                    selected.append({'lane':lane,'frame':frame,'source_t_ns':e['source_t_ns'],'path':e['output_path'],'bytes':e['output_bytes'],'sha256':e['output_sha256']})
                report['contact_sheets'].extend(media.make_stills(contacts,{lane:events},indices))
            report['contact_sheet_source']=source;report['selected_frames']=selected
            need(record(MEDIA_HELPER)==source,'existing contact-sheet source changed during review')
            report['checks']['64_actual_contact_frames']='PASS'
        except Exception as exc:report['errors'].append({'check':'contact_sheets','kind':type(exc).__name__,'message':str(exc)})
    try:
        for path,item in inputs.items():need(record(path)==item,f'input changed during review: {Path(path).name}')
        report['checks']['inputs_unchanged']='PASS'
    except Exception as exc:report['errors'].append({'check':'input_stability','kind':type(exc).__name__,'message':str(exc)})
    report['integrity_status']=('BLOCKED' if any(e['kind']=='permission_block' for e in report['errors']) else 'FAIL') if report['errors'] else 'PASS'
    report['inputs']=list(inputs.values());report['source']=record(__file__)
    report['verification_sources']=[record(CAPTURE/'gta_capture'/name) for name in ('evidence.py','gate.py','media.py','qc_bridge.py','staged.py')]
    report.pop('active_check',None)
    path=out/'scene-review.json'
    with path.open('x',encoding='utf-8') as f:json.dump(report,f,ensure_ascii=False,indent=2,allow_nan=False)
    print(json.dumps({'status':report['integrity_status'],'report':str(path),'sha256':record(path)['sha256'],'contact_sheets':len(report['contact_sheets']),'errors':report['errors']},ensure_ascii=False),flush=True)
    return 0 if not report['errors'] else 2


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--run-dir',required=True,type=Path);p.add_argument('--out',required=True,type=Path)
    args=p.parse_args();root,out=safe_paths(args.run_dir,args.out)
    manifest=finalized_manifest(root)
    return review(root,out,manifest)


if __name__=='__main__':
    try:raise SystemExit(main())
    except (ValueError,OSError,KeyError) as exc:
        print(json.dumps({'status':'REJECTED','message':str(exc),'source_changed':False}),file=sys.stderr)
        raise SystemExit(2)

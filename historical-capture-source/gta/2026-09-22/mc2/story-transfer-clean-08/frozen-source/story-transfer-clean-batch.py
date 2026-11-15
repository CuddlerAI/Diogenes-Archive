"""Freeze eight real-master-bound clean Story requests; capture only explicitly.

generate / verify are offline. capture uses only existing hello, probe_state,
and story_projection_sample commands, stops on any failure, and never resets
transport state. Run capture through the generated frozen-source copy.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import shutil
import sys
import uuid

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
CODE_ROOT = HERE if (HERE/'gta_capture').is_dir() else Path(__file__).resolve().parents[2]
sys.path.insert(0,str(CODE_ROOT))
from gta_capture.errors import ContractError
from gta_capture.jsonio import file_record, load_json, sha256_file, utc_now
from gta_capture.protocol import DEFAULT_PIPE, JsonLinePipeClient
from gta_capture.story_projection import capture_sample, validate_request
from gta_capture.tape import _quaternion_zxy

SCRIPT_NAME = 'story-transfer-clean-batch.py'
SOURCE_NAMES = [SCRIPT_NAME]+['gta_capture/'+name for name in
    ('__init__.py','errors.py','jsonio.py','protocol.py','story_projection.py','tape.py')]
SCHEMA = 'dm-gta-story-transfer-clean-plan/1'
RECEIPT_SCHEMA = 'dm-gta-story-transfer-clean-capture/1'


def write_new(path,value):
    path=Path(path); path.parent.mkdir(parents=True,exist_ok=True)
    with path.open('x',encoding='utf-8',newline='\n') as stream:
        json.dump(value,stream,ensure_ascii=False,indent=2,allow_nan=False)
        stream.write('\n')


def check_file(ref,path=None):
    path=Path(ref['path']) if path is None else Path(path)
    if (not path.is_file() or path.stat().st_size!=ref['bytes'] or
        sha256_file(path)!=ref['sha256']):
        raise ContractError('file differs from frozen bytes/SHA: '+str(path))
    return path


def inside(root,name):
    result=(root/name).resolve()
    if not result.is_relative_to(root.resolve()):
        raise ContractError('path escapes this plan')
    return result


def real_master(run):
    run=Path(run).resolve(); path=run/'evidence'/'native-record-result.json'
    record=load_json(path); master=record.get('master',{}); hello=record.get('hello',{})
    if (record.get('ok') is not True or master.get('mock') is not False or
        master.get('recording_start_verified') is not True or
        master.get('player_observation_count') not in (150,300)):
        raise ContractError('a successfully sealed real 5s/10s master is required')
    paths={'record_result':path,'replay':run/'source'/'master-replay.clip',
           'telemetry':Path(master.get('player_telemetry_path','')),
           'rockstar_source_clip':Path(master.get('source_clip_path',''))}
    if Path(master.get('replay_path','')).resolve()!=paths['replay']:
        raise ContractError('recording points at a different run replay')
    refs={key:file_record(path) for key,path in paths.items()}
    if (refs['replay']['sha256']!=master.get('replay_sha256') or
        refs['rockstar_source_clip']['sha256']!=master.get('replay_sha256') or
        refs['telemetry']['sha256']!=master.get('player_telemetry_sha256')):
        raise ContractError('master/source clip/telemetry SHA binding differs')
    for key in ('process_id','controller_generation'):
        if not isinstance(hello.get(key),int) or isinstance(hello[key],bool) or hello[key]<1:
            raise ContractError('recording lacks process/generation identity')
    if hello.get('story_mode') is not True or hello.get('online_session') is not False:
        raise ContractError('recording did not originate in offline Story mode')
    return {'files':refs,'replay_sha256':master['replay_sha256'],
            'source_clip_identity':master['source_clip_identity'],
            'process_id':hello['process_id'],'controller_generation':hello['controller_generation'],
            'telemetry_frame_count':master['player_observation_count']}


def generate(args):
    binding=real_master(args.record_run_dir)
    scout_path=args.scout_request.resolve(); scout=validate_request(load_json(scout_path))
    if scout['marker_mode']!='none' or abs(scout['camera_euler_deg'][2]+65)>1e-6:
        raise ContractError('use the actual marker-free city scout with yaw -65')
    plan_path=args.fov_plan.resolve(); plan=load_json(plan_path)
    if plan.get('sample_count')!=128 or len(plan.get('samples',[]))!=128:
        raise ContractError('the existing complete 128-request FOV plan is required')
    fovs={}; fov_refs={}
    for row in plan['samples']:
        request_path=check_file(row['request'],inside(plan_path.parent,row['request']['path']))
        request=validate_request(load_json(request_path))
        focal=request['nominal_focal_mm']; fov=request['fov_deg']
        if focal in fovs and fovs[focal]!=fov:
            raise ContractError('128 plan has more than one FOV for a focal label')
        fovs[focal]=fov; fov_refs.setdefault(str(focal),file_record(request_path))
    if set(fovs)!={18,24,35,50}:
        raise ContractError('all four nominal focal FOVs are required')
    out=args.out_dir.resolve(); out.mkdir(parents=True,exist_ok=False)
    sources=[]
    for name in SOURCE_NAMES:
        original=Path(__file__).resolve() if name==SCRIPT_NAME else CODE_ROOT/name
        target=out/'frozen-source'/name; target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(original,target)
        ref=file_record(target)
        if ref['sha256']!=sha256_file(original):
            raise ContractError('source changed while snapshotting')
        sources.append({'name':name,**ref})
    samples=[]
    camera=scout['camera_position']
    for focal in (18,24,35,50):
        for phase,pitch in enumerate((-5.0,5.0),1):
            euler=[pitch,0.0,-65.0]
            x,y,z,w=_quaternion_zxy(euler)
            forward=[2*(x*y-w*z),1-2*(x*x+z*z),2*(y*z+w*x)]
            request={'schema':'dm-gta-story-projection-request/1',
                'camera_position':camera,'camera_euler_deg':euler,
                'point_world':[camera[i]+20.0*forward[i] for i in range(3)],
                'fov_deg':fovs[focal],'nominal_focal_mm':focal,'marker_mode':'none'}
            validate_request(request)
            sample_id=f'f{focal:02d}-hold{phase}'
            path=out/'requests'/f'{sample_id}.json'; write_new(path,request)
            samples.append({'sample_id':sample_id,'lane':(18,24,35,50).index(focal)+1,
                'hold':phase,'nominal_focal_mm':focal,
                'request':{'path':str(path.relative_to(out)),
                    'bytes':path.stat().st_size,'sha256':sha256_file(path)},
                'output_parent':f'samples/{sample_id}'})
    manifest={'schema':SCHEMA,'created_utc':utc_now(),'planning_only':True,
        'sample_count':8,'master_binding':binding,'source_inventory':sources,
        'scout_request':file_record(scout_path),'source_fov_plan':file_record(plan_path),
        'source_fov_requests':fov_refs,'samples':samples,
        'camera_design':{'position':camera,'yaw_deg':-65,'pitch_deg':[-5,5],
            'point_geometry':'camera_position + 20 * quaternion-forward; no screen UV used',
            'marker_mode':'none','static_feature_selection':'pending real images; exclude actors, trees, clouds, shadows and moving objects'},
        'formal_calibration_approval':False,'editor_equivalence_verified':False,
        'capture_status':'NOT_CAPTURED'}
    write_new(out/'manifest.json',manifest)
    print(json.dumps({'manifest':str(out/'manifest.json'),
        'manifest_sha256':sha256_file(out/'manifest.json'),'bound_replay':binding['replay_sha256'],
        'frozen_capture_helper':str(out/'frozen-source'/SCRIPT_NAME),'samples':8,
        'game_pipe_opened':False}))


def load_plan(path):
    path=Path(path).resolve(); root=path.parent; value=load_json(path)
    if (value.get('schema')!=SCHEMA or value.get('sample_count')!=8 or
        len(value.get('samples',[]))!=8 or len({r['sample_id'] for r in value['samples']})!=8):
        raise ContractError('expected exactly eight unique clean samples')
    if {r['name'] for r in value['source_inventory']}!=set(SOURCE_NAMES):
        raise ContractError('frozen source closure differs')
    for ref in value['source_inventory']:
        check_file(ref,inside(root,'frozen-source/'+ref['name']))
    for ref in value['master_binding']['files'].values():
        check_file(ref)
    for row in value['samples']:
        req=check_file(row['request'],inside(root,row['request']['path']))
        if validate_request(load_json(req))['marker_mode']!='none':
            raise ContractError('a clean request contains a marker')
        inside(root,row['output_parent'])
    return path,value


def check_runtime(state,binding):
    expected={'story_mode':True,'online_session':False,'record_binding_ready':True,
        'process_id':binding['process_id'],'master_process_id':binding['process_id'],
        'controller_generation':binding['controller_generation'],
        'master_record_generation':binding['controller_generation'],
        'master_replay_sha256':binding['replay_sha256'],
        'master_source_sha256':binding['replay_sha256']}
    if any(state.get(k)!=v for k,v in expected.items()):
        raise ContractError('runtime lost offline Story/master/process/generation binding')


def runtime_snapshot(pipe,binding):
    client=JsonLinePipeClient(pipe)
    try:
        before=utc_now(); hello=client.request('hello'); check_runtime(hello,binding)
        probe=client.request('probe_state'); check_runtime(probe,binding)
        return {'started_utc':before,'completed_utc':utc_now(),'hello':hello,'probe':probe}
    finally:
        client.close()


def verify_attempt(directory,row,manifest_sha,binding):
    receipt=load_json(directory/'capture-receipt.json')
    if (receipt.get('schema')!=RECEIPT_SCHEMA or receipt.get('sample_id')!=row['sample_id'] or
        receipt.get('manifest_sha256')!=manifest_sha or receipt.get('master_replay_sha256')!=binding['replay_sha256']):
        raise ContractError('completed clean attempt belongs to another plan/master')
    names={'request.json','point.png','observation.json','runtime-before.json',
           'master-runtime-before.json','master-runtime-after.json'}
    if set(receipt.get('files',{}))!=names:
        raise ContractError('clean attempt receipt file closure differs')
    for name,ref in receipt['files'].items():
        check_file(ref,directory/name)
    if receipt['files']['request.json']['sha256']!=row['request']['sha256']:
        raise ContractError('captured clean request differs from frozen request')
    check_runtime(load_json(directory/'runtime-before.json'),binding)
    for name in ('master-runtime-before.json','master-runtime-after.json'):
        state=load_json(directory/name)
        for part in ('hello','probe'):
            check_runtime(state[part],binding)
    observed=load_json(directory/'observation.json')
    if (observed.get('mock') is not False or observed.get('marker_mode')!='none' or
        observed.get('request_sha256')!=row['request']['sha256'] or
        observed.get('output_sha256')!=receipt['files']['point.png']['sha256'] or
        observed.get('output_bytes')!=receipt['files']['point.png']['bytes'] or
        observed.get('editor_equivalence_verified') is not False):
        raise ContractError('clean observation differs from native request/PNG binding')
    return {'sample_id':row['sample_id'],'directory':str(directory),
            'receipt':file_record(directory/'capture-receipt.json')}


def attempts_for(root,row):
    parent=inside(root,row['output_parent'])
    return sorted(p for p in parent.glob('attempt-*') if p.is_dir()) if parent.exists() else []


def completed(root,row,manifest_sha,binding):
    attempts=attempts_for(root,row)
    successful=[p for p in attempts if (p/'capture-receipt.json').is_file()]
    if len(successful)>1:
        raise ContractError('multiple successful clean attempts require explicit review')
    return attempts,verify_attempt(successful[0],row,manifest_sha,binding) if successful else None


def process(args):
    manifest_path,manifest=load_plan(args.manifest); root=manifest_path.parent
    manifest_sha=sha256_file(manifest_path); binding=manifest['master_binding']
    status=[]
    for row in manifest['samples']:
        attempts,success=completed(root,row,manifest_sha,binding)
        status.append((row,attempts,success))
    if args.command=='verify':
        print(json.dumps({'planned':8,'completed_rehashed':sum(s is not None for _,_,s in status),
            'failed_or_incomplete_attempts':sum(sum(not (p/'capture-receipt.json').exists() for p in a) for _,a,_ in status),
            'master_replay_sha256':binding['replay_sha256'],'source_files_rehashed':len(SOURCE_NAMES),
            'game_pipe_opened':False,'editor_equivalence_verified':False}))
        return
    if Path(__file__).resolve()!=root/'frozen-source'/SCRIPT_NAME:
        raise ContractError('capture must use this plan frozen-source helper')
    if args.retry_failed and not args.resume:
        raise ContractError('--retry-failed requires explicit --resume')
    selected=status[:args.limit] if args.limit is not None else status
    for _,attempts,success in selected:
        if attempts and not args.resume:
            raise ContractError('existing attempts require explicit --resume')
        if attempts and success is None and not args.retry_failed:
            raise ContractError('failed attempt preserved; explicit --retry-failed is required')
    logpath=root/'logs'/('capture-'+datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'-'+uuid.uuid4().hex[:8]+'.jsonl')
    logpath.parent.mkdir(exist_ok=True)
    with logpath.open('x',encoding='utf-8') as log:
        def emit(item):
            line=json.dumps({'at':utc_now(),**item},ensure_ascii=False)
            log.write(line+'\n'); log.flush(); print(line,flush=True)
        for row,attempts,success in selected:
            if success:
                emit({'event':'skip_rehashed_success',**success}); continue
            number=max((int(p.name.split('-')[1]) for p in attempts),default=0)+1
            output=inside(root,row['output_parent'])/f'attempt-{number:03d}'
            request=inside(root,row['request']['path'])
            emit({'event':'capture_start','sample_id':row['sample_id'],'output':str(output)})
            try:
                load_plan(manifest_path)
                if sha256_file(manifest_path)!=manifest_sha:
                    raise ContractError('frozen plan changed during capture')
                before=runtime_snapshot(args.pipe,binding)
                capture_sample(request,output,pipe=args.pipe)
                write_new(output/'master-runtime-before.json',before)
                check_runtime(load_json(output/'runtime-before.json'),binding)
                after=runtime_snapshot(args.pipe,binding)
                write_new(output/'master-runtime-after.json',after)
                load_plan(manifest_path)
                if sha256_file(manifest_path)!=manifest_sha:
                    raise ContractError('frozen plan changed during capture')
                receipt={'schema':RECEIPT_SCHEMA,'created_utc':utc_now(),
                    'sample_id':row['sample_id'],'manifest_sha256':manifest_sha,
                    'master_replay_sha256':binding['replay_sha256'],
                    'helper':file_record(__file__),
                    'files':{name:file_record(output/name) for name in
                        ('request.json','point.png','observation.json','runtime-before.json',
                         'master-runtime-before.json','master-runtime-after.json')},
                    'approval_status':'draft','editor_equivalence_verified':False,
                    'formal_calibration_approval':False,'static_feature_registration':'not_performed'}
                write_new(output/'capture-receipt.json',receipt)
                success=verify_attempt(output,row,manifest_sha,binding)
                emit({'event':'capture_saved',**success})
            except Exception as exc:
                failure={'event':'capture_failed','sample_id':row['sample_id'],
                    'error_type':type(exc).__name__,'error':str(exc),
                    'transport_reset_or_clear_requested':False,'automatic_retry':False}
                output.mkdir(parents=True,exist_ok=True)
                write_new(output/'failure.json',failure); emit(failure)
                raise
    all_done=[(row,completed(root,row,manifest_sha,binding)[1]) for row in manifest['samples']]
    if all(success is not None for _,success in all_done):
        scenes={'schema':'dm-gta-story-editor-transfer-scenes/1',
            'source_manifest':file_record(manifest_path),'scene_selection_status':'real_images_captured_static_feature_review_pending',
            'lanes':{str(lane):{'nominal_focal_mm':focal,'holds':[]} for lane,focal in enumerate((18,24,35,50),1)}}
        for row,success in all_done:
            scenes['lanes'][str(row['lane'])]['holds'].append({'name':'tilt-'+str(row['hold']),
                'clean_sample_dir':success['directory'],'capture_receipt':success['receipt']})
        target=root/'completed-clean-scenes.json'
        if target.exists():
            if load_json(target)!=scenes:
                raise ContractError('existing completed scene index differs')
        else:
            write_new(target,scenes)
        print(json.dumps({'completed':8,'scenes':str(target),'editor_equivalence_verified':False}))


def main():
    parser=argparse.ArgumentParser(description=__doc__); sub=parser.add_subparsers(dest='command',required=True)
    p=sub.add_parser('generate')
    p.add_argument('--record-run-dir',type=Path,required=True)
    p.add_argument('--scout-request',type=Path,required=True)
    p.add_argument('--fov-plan',type=Path,required=True)
    p.add_argument('--out-dir',type=Path,required=True)
    for name in ('verify','capture'):
        p=sub.add_parser(name); p.add_argument('--manifest',type=Path,required=True)
        if name=='capture':
            p.add_argument('--pipe',default=DEFAULT_PIPE)
            p.add_argument('--resume',action='store_true'); p.add_argument('--retry-failed',action='store_true')
            p.add_argument('--limit',type=int,choices=range(1,9))
    args=parser.parse_args()
    try:
        generate(args) if args.command=='generate' else process(args)
    except Exception as exc:
        parser.exit(2,type(exc).__name__+': '+str(exc)+'\n')


if __name__=='__main__':
    main()

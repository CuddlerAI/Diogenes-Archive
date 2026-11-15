"""Offline draft/prepare helper for a real short Story -> Editor comparison.

Never connects to GTA, edits a source capture, or grants calibration approval.
Record first with the existing CLI, capture eight clean Story observations in
that same live master process, then prepare the complete four-lane short tape.
"""
from __future__ import annotations

import argparse
import copy
import json
import math
from pathlib import Path
import shutil
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from gta_capture.errors import ContractError
from gta_capture.jsonio import file_record, load_json, sha256_file
from gta_capture.native_tape import write_native_tape
from gta_capture.recorded_route import build_recorded_subject_route, validate_recorded_subject_route
from gta_capture.scenario import Scenario, validate_scenario
from gta_capture.story_projection import validate_request
from gta_capture.tape import (FOCALS, GTA_COORDINATE_CONVENTION, TAPE_SCHEMA,
                              _quaternion_zxy, _source_t_ns, validate_tape, write_tape)


def write_new(path, value):
    path = Path(path)
    with path.open('x', encoding='utf-8', newline='\n') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write('\n')


def command(*args):
    return {'cwd':str(ROOT),'argv':[sys.executable,'-m','gta_capture',*map(str,args)],
            'execution_status':'not_executed_by_this_helper'}


def draft(args):
    original = Scenario.load(args.base_scenario)
    scenario = copy.deepcopy(original.raw)
    scenario['scenario_id'] = args.scenario_id
    scenario['replay']['duration_s'] = float(args.duration)
    if args.warmup_s is not None:
        scenario['replay']['warmup_s'] = args.warmup_s
    if args.location is not None:
        scenario['location'] = args.location
    validate_scenario(scenario)
    output = args.out_dir.resolve()
    output.mkdir(parents=True,exist_ok=False)
    scenario_path = output/'scenario.json'
    write_new(scenario_path,scenario)
    scenes = {'schema':'dm-gta-story-editor-transfer-scenes/1',
              'scene_selection_status':'pending_real_clean_image_review',
              'instructions':'Choose static building features at multiple depths; exclude the actor, people, vegetation, shadows and moving effects. Fill only with real marker_mode=none sample directories captured after this master recording.',
              'lanes':{str(lane):{'nominal_focal_mm':focal,
                  'holds':[{'name':'tilt-a','clean_sample_dir':None},
                           {'name':'tilt-b','clean_sample_dir':None}]}
                       for lane,focal in enumerate(FOCALS,1)}}
    write_new(output/'clean-scenes.template.json',scenes)
    write_new(output/'record-plan.json',{
        'schema':'dm-gta-story-editor-transfer-record-plan/1','planning_only':True,
        'source_base_scenario':file_record(original.path),'scenario':file_record(scenario_path),
        'requested_frames':args.duration*30,'record_run_dir':str(output/'run'),
        'record_command':command('native-record-smoke','--scenario',scenario_path,
                                 '--run-dir',output/'run'),
        'real_recording_completed':False,'replay_sha256':None,
        'limits':['Five-second Rockstar sealing remains to be proven by a real attempt; 10 seconds is the known fallback.',
                  'The positive native task parameters remain commands, not physical speed or a stationary actor claim.',
                  'Do not restart GTA or replace the live master between recording, eight clean Story samples and all four Editor lanes.']})
    print(json.dumps({'draft':str(output),'requested_frames':args.duration*30,
                      'real_recording_completed':False}))


def vec(value,n,label):
    if (not isinstance(value,list) or len(value)!=n or
        any(not isinstance(x,(int,float)) or isinstance(x,bool) or not math.isfinite(x) for x in value)):
        raise ContractError(label+' must be finite')
    return list(map(float,value))


def read_clean(directory, *, focal, recorded_hello, replay_sha, route_positions):
    directory = Path(directory).resolve()
    paths = {name:directory/name for name in
             ('request.json','observation.json','runtime-before.json','point.png')}
    if not all(path.is_file() for path in paths.values()):
        raise ContractError('real clean Story sample is incomplete: '+str(directory))
    refs = {name:file_record(path) for name,path in paths.items()}
    request = validate_request(load_json(paths['request.json']))
    observed = load_json(paths['observation.json'])
    runtime = load_json(paths['runtime-before.json'])
    if (request['marker_mode']!='none' or request['nominal_focal_mm']!=focal or
        observed.get('schema')!='dm-gta-story-calibration-frame/1' or observed.get('mock') is not False or
        observed.get('approval_status')!='draft' or observed.get('editor_equivalence_verified') is not False or
        observed.get('marker_mode')!='none' or observed.get('nominal_focal_mm')!=focal or
        observed.get('request_sha256')!=refs['request.json']['sha256'] or
        observed.get('output_sha256')!=refs['point.png']['sha256'] or
        observed.get('output_bytes')!=refs['point.png']['bytes'] or
        Path(observed.get('output_path','')).resolve()!=paths['point.png']):
        raise ContractError('clean Story observation/request/PNG binding differs')
    # Request observations are not invented projections. No screen_normalized
    # value is consumed here, and no Story schema is upcast to an Editor schema.
    for key in ('process_id','controller_generation'):
        expected = recorded_hello.get(key)
        if (not isinstance(expected,int) or isinstance(expected,bool) or expected<1 or
            runtime.get(key)!=expected):
            raise ContractError('clean Story sample differs from recording process/generation')
    if (runtime.get('story_mode') is not True or runtime.get('online_session') is not False or
        runtime.get('record_binding_ready') is not True or
        runtime.get('master_replay_sha256')!=replay_sha or
        runtime.get('master_source_sha256')!=replay_sha or
        runtime.get('master_process_id')!=runtime['process_id'] or
        runtime.get('master_record_generation')!=runtime['controller_generation']):
        raise ContractError('clean Story sample lacks the current real master binding')
    cameras = []
    for name in ('camera_before','camera_after'):
        camera = observed.get(name,{})
        p = vec(camera.get('position'),3,name+'.position')
        e = vec(camera.get('euler_deg'),3,name+'.euler_deg')
        q = vec(camera.get('quaternion'),4,name+'.quaternion')
        fov = camera.get('fov_deg')
        if not isinstance(fov,(int,float)) or isinstance(fov,bool) or not math.isfinite(fov) or not 5<=fov<=120:
            raise ContractError('clean Story camera FOV is invalid')
        expected_q = _quaternion_zxy(e)
        if min(math.dist(q,expected_q),math.dist(q,[-x for x in expected_q]))>1e-7:
            raise ContractError('clean camera quaternion differs from observed order-2 Euler')
        cameras.append({'position':p,'euler_deg':e,'quaternion':q,'fov_deg':float(fov)})
    before,after = cameras
    if (math.dist(before['position'],after['position'])>0.001 or
        min(math.dist(before['quaternion'],after['quaternion']),
            math.dist(before['quaternion'],[-x for x in after['quaternion']]))>0.0000873 or
        abs(before['fov_deg']-after['fov_deg'])>0.001):
        raise ContractError('clean Story camera changed across readback')
    requested_q=_quaternion_zxy(request['camera_euler_deg'])
    if (math.dist(before['position'],request['camera_position'])>0.001 or
        min(math.dist(before['quaternion'],requested_q),
            math.dist(before['quaternion'],[-x for x in requested_q]))>0.0000873 or
        abs(before['fov_deg']-request['fov_deg'])>0.001):
        raise ContractError('clean Story camera differs from its requested pose/FOV')
    near = max(math.dist(before['position'],p) for p in route_positions)
    if near>=30:
        raise ContractError('camera is not within 30 world units of every recorded actor position')
    width,height = observed.get('width'),observed.get('height')
    if (width,height)!=(1920,1080):
        raise ContractError('short transfer uses actual 1920x1080 clean samples')
    header=paths['point.png'].read_bytes()[:24]
    if (header[:8]!=b'\x89PNG\r\n\x1a\n' or header[12:16]!=b'IHDR' or
        int.from_bytes(header[16:20],'big')!=width or int.from_bytes(header[20:24],'big')!=height):
        raise ContractError('clean PNG header differs from observation')
    return {'files':refs,'camera':before,'maximum_recorded_actor_distance_world_units':near,
            'nominal_focal_mm':focal,'resolution':{'width':width,'height':height}}


def prepare(args):
    scenario = Scenario.load(args.scenario)
    if scenario.duration_s not in (5.0,10.0):
        raise ContractError('this bounded transfer draft supports only complete 5s or 10s masters')
    run = args.record_run_dir.resolve()
    record_path = run/'evidence'/'native-record-result.json'
    record = load_json(record_path)
    route = build_recorded_subject_route(record_path,scenario.path,window_frames=1,passes=0)
    replay_sha = route['observation']['files']['replay']['sha256']
    validate_recorded_subject_route(route,scenario=scenario,expected_replay_sha256=replay_sha)
    replay_path = Path(route['observation']['files']['replay']['path'])
    if replay_path.resolve()!=run/'source'/'master-replay.clip':
        raise ContractError('recorded replay is not this run master-replay.clip')
    manifest_path = args.manifest.resolve()
    if not manifest_path.is_file():
        raise ContractError('an actual dependency manifest path is required')
    scenes = load_json(args.scenes)
    if (scenes.get('schema')!='dm-gta-story-editor-transfer-scenes/1' or
        set(scenes.get('lanes',{}))!={'1','2','3','4'}):
        raise ContractError('scene configuration requires all four lanes')
    positions = [p['position'] for p in route['waypoints']]
    observed_holds = {}
    for lane,focal in enumerate(FOCALS,1):
        spec = scenes['lanes'][str(lane)]
        holds = spec.get('holds')
        if spec.get('nominal_focal_mm')!=focal or not isinstance(holds,list) or len(holds)!=2:
            raise ContractError('each focal lane needs two real clean Story camera holds')
        observed_holds[str(lane)] = []
        for hold in holds:
            directory=hold.get('clean_sample_dir')
            if not isinstance(directory,str) or not directory:
                raise ContractError('fill clean_sample_dir from an actual marker-free Story sample')
            p=Path(directory)
            if not p.is_absolute():
                p=args.scenes.resolve().parent/p
            observed_holds[str(lane)].append(read_clean(p,focal=focal,
                recorded_hello=record['hello'],replay_sha=replay_sha,route_positions=positions))
        first,second=observed_holds[str(lane)]
        if abs(first['camera']['euler_deg'][0]-second['camera']['euler_deg'][0])<5:
            raise ContractError('two clean holds must differ in actual camera pitch by at least 5 degrees')
        if abs(first['camera']['fov_deg']-second['camera']['fov_deg'])>0.001:
            raise ContractError('both holds in a focal lane must use the same actual FOV')
    # No output is created before all real-recording and clean-image checks pass.
    output=args.out_dir.resolve()
    output.mkdir(parents=True,exist_ok=False)
    route=build_recorded_subject_route(record_path,scenario.path,window_frames=1,passes=0,
        artifact_dir=output/'frozen-master')
    n=len(route['waypoints']); half=n//2
    selected=[]; lanes={}; frozen_holds={}
    for lane,focal in enumerate(FOCALS,1):
        samples=[]; frozen_holds[str(lane)]=[]
        for phase,hold in enumerate(observed_holds[str(lane)]):
            target=output/'frozen-story'/f'lane{lane}-hold{phase+1}'
            target.mkdir(parents=True)
            frozen={}
            for name,ref in hold['files'].items():
                shutil.copyfile(ref['path'],target/name)
                frozen[name]=file_record(target/name)
                if any(frozen[name][k]!=ref[k] for k in ('sha256','bytes')):
                    raise ContractError('clean Story source changed during snapshot')
            hold={**hold,'files':frozen}
            camera=hold['camera']; width=hold['resolution']['width']; height=hold['resolution']['height']
            fpx=height/(2*math.tan(math.radians(camera['fov_deg'])/2))
            start=phase*half; stop=(phase+1)*half
            for frame in range(start,stop):
                samples.append({'frame':frame,'source_t_ns':_source_t_ns(frame),
                    'position':camera['position'],'euler_deg':camera['euler_deg'],
                    'quaternion':camera['quaternion'],
                    'intrinsics':{'fx':fpx,'fy':fpx,'cx':width/2,'cy':height/2}})
            hold.update({'start_frame':start,'end_frame_exclusive':stop,
                         'intrinsics_semantics':'FOV control encoding only; not measured/calibrated K'})
            frozen_holds[str(lane)].append(hold)
            selected.append({'lane':lane,'hold':phase+1,'planned_output_frame':start+round(0.6*half),
                'story_png':frozen['point.png'],'editor_png':None,'editor_frame_event':None,
                'registration_status':'pending_real_Editor_export_and_static_feature_measurement'})
        lanes[str(lane)]={'focal_mm':focal,'resolution':{'width':1920,'height':1080},'samples':samples}
    tape={'schema':TAPE_SCHEMA,'clip_id':'story-editor-transfer-'+scenario.scenario_id,
          'fps_num':30,'fps_den':1,'duration_s':scenario.duration_s,'n_frames':n,
          'timestamp_semantics':'source_replay_timeline',
          'filmback_id':'story-editor-transfer-control-fov-unapproved',
          'coordinate_convention':GTA_COORDINATE_CONVENTION,
          'source':{'kind':'story_editor_transfer_diagnostic','anchor':route['waypoints'][0]['position'],
                    'native_command':route['native_command'],'subject_route':route,
                    'story_holds':frozen_holds,'replay_sha256':replay_sha,
                    'scene_configuration':file_record(args.scenes),
                    'delivery_eligible':False,'editor_equivalence_verified':False,
                    'intrinsics_calibrated':False,'entity_animation_state_verified':False},
          'lanes':lanes}
    validate_tape(tape,expected_duration_s=scenario.duration_s)
    tape_path=output/'camera-tape.json'; write_tape(tape_path,tape)
    write_native_tape(output/'camera-tape.bin',tape,json_path=tape_path)
    write_new(output/'comparison-index-plan.json',{'schema':'dm-gta-story-editor-transfer-index-plan/1',
        'planning_only':True,'source_replay_sha256':replay_sha,'complete_frames_per_lane':n,
        'selected_stills':selected,'static_scene_geometry_measured':False,
        'requirement':'Read real sealed full-lane events and hash the selected raw PNGs; never manufacture an event or truncate the lane.',
        'formal_calibration_approval':False,'editor_equivalence_verified':False})
    write_new(output/'prepare-receipt.json',{'schema':'dm-gta-story-editor-transfer-prepare/1',
        'planning_only':True,'master_recording_real_verified':True,'source_replay_sha256':replay_sha,
        'recording_process_id':record['hello']['process_id'],
        'recording_controller_generation':record['hello']['controller_generation'],
        'camera_tape':file_record(tape_path),'native_tape':file_record(output/'camera-tape.bin'),
        'dependency_manifest':file_record(manifest_path),'frames_per_lane':n,
        'native_export_commands':[command('native-export-smoke','--lane',lane,'--tape',tape_path,
            '--replay',replay_path,'--manifest',manifest_path,'--run-dir',run,'--arm-only') for lane in range(1,5)],
        'pipe_called':False,'editor_exports_completed':False,'formal_calibration_approval':False,
        'limits':['Process identity is checked from preserved observations; the existing CLI must recheck the live binding when arming.',
                  'Same pixel view of static multi-depth features still requires real registration and review; exclude all dynamic content.',
                  'A two-hold diagnostic tape is not an MC motion-QC passing delivery and must not use MC finalize as an approval shortcut.']})
    print(json.dumps({'prepared':str(output),'frames_per_lane':n,'replay_sha256':replay_sha,
                      'editor_exports_completed':False,'formal_calibration_approval':False}))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    sub=parser.add_subparsers(dest='action',required=True)
    p=sub.add_parser('draft-record',help='Only write a scenario, empty scene template and unexecuted record command')
    p.add_argument('--base-scenario',type=Path,required=True)
    p.add_argument('--scenario-id',required=True)
    p.add_argument('--out-dir',type=Path,required=True)
    p.add_argument('--duration',type=int,choices=(5,10),default=5)
    p.add_argument('--warmup-s',type=float)
    p.add_argument('--location',type=float,nargs=3)
    p.set_defaults(func=draft)
    p=sub.add_parser('prepare',help='Require a real short master and eight same-process clean Story observations')
    p.add_argument('--scenario',type=Path,required=True)
    p.add_argument('--record-run-dir',type=Path,required=True)
    p.add_argument('--scenes',type=Path,required=True)
    p.add_argument('--manifest',type=Path,required=True)
    p.add_argument('--out-dir',type=Path,required=True)
    p.set_defaults(func=prepare)
    args=parser.parse_args()
    try:
        args.func(args)
    except (ContractError,OSError,ValueError,KeyError,TypeError) as exc:
        parser.exit(2,str(exc)+'\n')


if __name__=='__main__':
    main()

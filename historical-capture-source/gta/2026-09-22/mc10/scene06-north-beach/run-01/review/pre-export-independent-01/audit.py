"""Read-only source/geometry audit. Writes only new audit outputs beside this file."""
from pathlib import Path
from datetime import datetime, timezone
from collections import Counter
import sys, json, math, itertools

HERE = Path(__file__).resolve().parent
RUN = HERE.parents[1]
PKG = RUN.parents[3]
sys.path.insert(0, str(PKG))
from gta_capture.jsonio import load_json, file_record, sha256_file
from gta_capture.scenario import Scenario
from gta_capture.recorded_route import validate_recorded_subject_route
from gta_capture.staged import _validate_scenario_tape_route
from gta_capture.tape import validate_tape
from gta_capture.moving_plan import _dependencies
_dependencies()
import numpy as np
from engine_qc.clip_checker import check_clip

assert not (HERE/'audit.json').exists(), 'choose a new audit output; never overwrite'
checks = []
inputs = {}
def check(name, passed, **detail):
    checks.append(dict(id=name, status='PASS' if bool(passed) else 'FAIL', **detail))
def seal(path):
    path = Path(path).resolve()
    rec = file_record(path)
    inputs[str(path)] = rec
    return rec
def read(path):
    seal(path)
    return load_json(path)

P = RUN/'config/recorded-plan'
B = RUN/'config/recorded-plan-base-01'
PROPOSAL = RUN/'review/body-framing-proposal-01'
scenario = Scenario.load(RUN/'config/scenario.json')
seal(RUN/'config/scenario.json')
record = read(RUN/'evidence/native-record-result.json')
master = record['master']
replay = seal(RUN/'source/master-replay.clip')
telemetry = seal(RUN/'source/master-player-telemetry.jsonl')
rows = [json.loads(s) for s in Path(telemetry['path']).read_text(encoding='utf-8').splitlines()]
plan = read(P/'plan.json')
parent = read(B/'plan.json')
prov = read(P/'provenance.json')
tape = read(RUN/'config/camera-tape.json')
separate_route = read(RUN/'config/subject-route.json')
manifest = read(RUN/'config/dependency-manifest.json')
manifest_copy = read(RUN/'config/manifest-installed.json')
expected_master = '720573e9561a7ce292d7c736fcfb2d0b211352aed5150dba5382a09e6f8d1a52'
expected_telemetry = 'fee151262a944209cb1ac105b2fa60d6f7106f81257df2ced60d8d7cfb037a20'
check('real_master_sha_and_rows', replay['sha256'] == master['replay_sha256'] == expected_master
      and telemetry['sha256'] == master['player_telemetry_sha256'] == expected_telemetry
      and len(rows) == master['player_observation_count'] == 1800
      and record['ok'] is True and master['mock'] is False)
check('scenario_binding', record['scenario']['scenario_sha256'] == sha256_file(RUN/'config/scenario.json'))
check('manifest_copies_equal', manifest == manifest_copy)
check('filmback_is_still_pending', tape['filmback_id'] == manifest['calibration']['filmback_id']
      == 'pending-customer-confirmation' and manifest['calibration']['metric_scale_approved'] is False)
route = plan['subject_motion']['recorded_route']
diagnostics = validate_recorded_subject_route(route, scenario=scenario, expected_replay_sha256=expected_master)
check('strict_recorded_route_recompute', True, diagnostics=diagnostics)
validate_tape(tape, expected_duration_s=60)
_validate_scenario_tape_route(scenario, tape, expected_replay_sha256=expected_master)
check('strict_tape_and_scenario_route', True)
check('separate_route_matches_tape', separate_route == tape['source']['subject_route'])
for key, rec in route['observation']['files'].items():
    actual = seal(rec['path'])
    check('route_file_'+key, all(actual[k] == rec[k] for k in ('bytes','sha256')))
for rec in tape['source']['files']:
    actual = seal(P/rec['name'])
    check('tape_file_'+rec['name'], actual['sha256'] == rec['sha256'])
for path in P.iterdir():
    if path.is_file():
        actual = seal(path)
        original = seal(PROPOSAL/'recorded-plan'/path.name)
        check('selected_proposal_'+path.name, actual['sha256'] == original['sha256'])
check('unchanged_subject_and_segments', all(plan[k] == parent[k] for k in
      ('cameras','anchors','cam4','subject_motion')))
check('timeline_shape', plan['duration_s'] == 60 and plan['fps'] == 30
      and tape['n_frames'] == 1800 and tape['fps_num'] == 30 and tape['fps_den'] == 1
      and all(a['t'] == i/30 for i,a in enumerate(plan['anchors'])))

world = np.array([r['player_state']['position'] for r in rows], dtype=float)
fit = np.array([w['position'] for w in route['waypoints']], dtype=float)
obs_ns = np.array([r['observation_t_ns'] for r in rows], dtype=np.int64)
source_ns = np.array([r['source_t_ns'] for r in rows], dtype=np.int64)
dt = np.diff(obs_ns)/1e9
dist = np.linalg.norm(np.diff(world,axis=0),axis=1)
speed = dist/dt
residual = np.linalg.norm(world-fit,axis=1)
anchor = np.array(tape['source']['anchor'])
anchors = np.array([a['p'] for a in plan['anchors']])
entity = (fit-fit[0])[:,[0,2,1]] + np.array([anchors[0,0],0,anchors[0,2]])
check('dynamic_world_z_preserved', np.allclose(anchors[:,1]-anchors[0,1], fit[:,2]-fit[0,2],rtol=0,atol=1e-8))
check('placement_actual_first_not_scout_z', np.max(np.abs(anchor-world[0])) < 1e-8
      and tape['source']['placement']['heading_deg'] == 0 and tape['source']['placement']['ground_y'] == 0)
h=math.radians(scenario.raw['player']['heading_deg'])
direction=np.array([-math.sin(h),math.cos(h)])
delta=world[:,:2]-world[0,:2]
progress=delta@direction
lateral=np.abs(delta@np.array([-direction[1],direction[0]]))
stalls=[]
start=None
slow=speed < 0.1
for i,flag in enumerate(list(slow)+[False]):
    if flag and start is None: start=i
    if not flag and start is not None:
        duration=(obs_ns[i]-obs_ns[start])/1e9
        if duration>=0.5:
            stalls.append(dict(start_frame=int(start),end_frame=int(i),start_s=float(source_ns[start]/1e9),
                end_s=float(source_ns[i]/1e9),observed_duration_s=float(duration),path_distance_m=float(dist[start:i].sum())))
        start=None
route_review=dict(diagnostics=diagnostics, first_observed=world[0].tolist(),last_observed=world[-1].tolist(),
    observed_entity_z_range=[float(world[:,2].min()),float(world[:,2].max())],
    smoothed_entity_z_range=[float(fit[:,2].min()),float(fit[:,2].max())],
    z_change_m=float(world[-1,2]-world[0,2]),max_residual_frame=int(residual.argmax()),
    residual_at_worst=world[residual.argmax()].tolist(),scenario_to_first_distance_m=float(np.linalg.norm(world[0]-scenario.raw['location'])),
    along_heading_end_m=float(progress[-1]),max_cross_track_m=float(lateral.max()),
    low_speed_intervals=stalls,low_speed_diagnostic_definition='observed segment speed <0.1 GTA units/s for >=0.5s; diagnostic only, not a changed acceptance gate',
    observation_lateness_ns_max=int((obs_ns-source_ns).max()),native_speed_parameter=0.2,
    native_speed_parameter_is_physical_mps=False,feet_ground_or_collision_observed=False)

declared_joint=read(PROPOSAL/'joint-body-margin.json')
corners=np.array(list(itertools.product((-.35,.35),(-1.2,1.85),(-.35,.35))))
lanes={}
all_translations=[]
for lane in ('1','2','3','4'):
    action=read(P/f'action_camera_{lane}.json')
    original=read(B/f'action_camera_{lane}.json')
    system=read(P/f'systeminfo_{lane}.json')
    camera=next(c for c in plan['cameras'] if str(c['id'])==lane)
    sample=tape['lanes'][lane]['samples']
    check('lane_count_'+lane,len(action)==len(original)==len(sample)==1800)
    changed=sorted({k for a,b in zip(action,original) for k in set(a)|set(b) if a.get(k)!=b.get(k)})
    allowed={'camera_rotation_quaternion'}|({'camera_rotation_oula','mouse_dx','mouse_dy'} if lane=='1' else set())
    check('aim_only_changes_'+lane,set(changed)<=allowed and (lane!='4' or not changed),changed_fields=changed)
    positions=np.array([a['camera_position'] for a in action])
    q=np.array([a['camera_rotation_quaternion'] for a in action]);q/=np.linalg.norm(q,axis=1)[:,None]
    tq=np.array([s['quaternion'] for s in sample]);tq/=np.linalg.norm(tq,axis=1)[:,None]
    tp=np.array([s['position'] for s in sample])
    expectedp=(positions-np.array([anchors[0,0],0,anchors[0,2]]))[:,[0,2,1]]+anchor
    expectedq=q[:,[0,2,1,3]]*np.array([-1,-1,-1,1])
    qerr=np.minimum(np.linalg.norm(expectedq-tq,axis=1),np.linalg.norm(expectedq+tq,axis=1))
    check('all_frame_placement_'+lane,np.max(np.abs(expectedp-tp))<1e-8 and qerr.max()<3e-11,
          position_max_abs_error_m=float(np.max(np.abs(expectedp-tp))),quaternion_chord_max_error=float(qerr.max()))
    check('frame_clock_and_no_input_in_tape_'+lane,
          all(s['frame']==i and s['source_t_ns']==round(i*1e9/30) and set(s)=={'frame','source_t_ns','position','quaternion','euler_deg','intrinsics'} for i,s in enumerate(sample)))
    intr=np.array([[a['camera_intrinsics'][k] for k in ('fx','fy','cx','cy')] for a in action])
    expectedK=np.array([camera['focal_mm']*45,camera['focal_mm']*45,960,540])
    check('K_preserved_'+lane,np.array_equal(intr,np.tile(expectedK,(1800,1))) and
          all(a['camera_intrinsics']==s['intrinsics']==b['camera_intrinsics'] for a,s,b in zip(action,sample,original)),
          nominal_focal_mm=camera['focal_mm'],K=expectedK.tolist(),approved_calibration=False)
    # Independent inverse quaternion rotation of each corner (not helper's matrix-dot implementation).
    d=entity[:,None,:]+corners[None,:,:]-positions[:,None,:]
    v=-q[:,:3][:,None,:];w=q[:,3,None,None]
    cam=d+2*np.cross(v,np.cross(v,d)+w*d)
    u=intr[:,2,None]+intr[:,0,None]*cam[:,:,0]/cam[:,:,2]
    vv=intr[:,3,None]-intr[:,1,None]*cam[:,:,1]/cam[:,:,2]
    margins=np.minimum.reduce([u,1920-u,vv,1080-vv]).min(axis=1)
    inside=(cam[:,:,2].min(axis=1)>.05)&(margins>=0)
    oldmargin=np.array([x['min_edge_margin_px'] for x in declared_joint['lanes'][lane]['frame_margins']])
    check('independent_joint_box_'+lane,inside.all() and np.max(np.abs(margins-oldmargin))<1e-6,
          declared_margin_max_difference_px=float(np.max(np.abs(margins-oldmargin))))
    forward=np.tile([0.,0.,1.],(1800,1));v2=q[:,:3]
    forward=forward+2*np.cross(v2,np.cross(v2,forward)+q[:,3,None]*forward)
    aimerror=None
    if lane!='4':
        target=entity+np.array([0,[.325,.425,.575][int(lane)-1],0])
        aim=target-positions;aim/=np.linalg.norm(aim,axis=1)[:,None]
        aimerror=float(np.linalg.norm(aim-forward,axis=1).max())
        check('actual_time_varying_aim_'+lane,aimerror<1e-10,unit_direction_max_error=aimerror)
    translations=[s for s in camera['segments'] if s['cls'].startswith('translate_')]
    check('six_translations_'+lane,len(translations)==6 and {s['cls'] for s in translations}==
          {'translate_forward','translate_back','translate_left','translate_right','translate_up','translate_down'}
          and all(abs(s['t1']-s['t0']-3)<1e-8 for s in translations))
    all_translations.extend([dict(lane=lane,**s) for s in translations])
    worst=np.argsort(margins,kind='stable')[:8]
    lanes[lane]=dict(n_frames=1800,nominal_focal_mm=camera['focal_mm'],K=expectedK.tolist(),
        world_camera_min=tp.min(axis=0).tolist(),world_camera_max=tp.max(axis=0).tolist(),
        minimum_height_above_initial_entity_plane_m=float((tp[:,2]-anchor[2]).min()),
        height_above_dynamic_entity_range_m=[float((tp[:,2]-fit[:,2]).min()),float((tp[:,2]-fit[:,2]).max())],
        full_joint_box_inside_frames=int(inside.sum()),min_edge_margin_px=float(margins.min()),
        worst_frames=[dict(output_frame=int(i),min_edge_margin_px=float(margins[i]),bbox_px=[float(u[i].min()),float(vv[i].min()),float(u[i].max()),float(vv[i].max())]) for i in worst],
        aim_direction_max_error=aimerror,changed_fields=changed)
    if lane=='4':
        lanes[lane]['angle_from_vertical_down_max_deg']=float(np.degrees(np.arccos(np.clip(-forward[:,1],-1,1))).max())
    if lane=='1':
        check('player_position_actual_fit',np.allclose([a['player_position'] for a in action],entity,rtol=0,atol=1e-6))
        check('player_rotation_declared_placeholder',all(a['player_rotation_quaternion']==[0,0,0,1] and a['player_rotation_oula']==[0,0,0] for a in action)
              and 'placeholder' in prov['field_conventions']['player_rotation_quaternion'])

stored_qc=read(P/'forward-plan-qc.json')
freshqc=check_clip(P,'mc',include_video=False)
freshqc_dict=dict(verdict=freshqc.verdict,counts=freshqc.counts,results=[x.to_dict() for x in freshqc.results])
check('existing_thresholds_qc_recomputed',freshqc.verdict=='PASS' and freshqc.counts==stored_qc['counts']
      and freshqc_dict['results']==stored_qc['results'])
covis=read(P/'covis.json');horizontal=read(P/'covis-horizontal-proxy.json')
check('proxy_ground_claims_honest',covis['measured_in_gta'] is False and covis['plane_fit']['terrain_measured'] is False
      and horizontal['measured_in_gta'] is False and covis['horizontal_reference']['sha256']==sha256_file(P/'covis-horizontal-proxy.json'))
check('input_and_approval_scope', 'Synthetic' in plan['framing_derivation']['planning_mouse_semantics']
      and plan['framing_derivation']['calibration_approved'] is False and prov['non_trainable'] is True
      and prov['delivery_eligible'] is False and all('input' not in r and 'mouse_dx' not in r for r in rows))
different=[]
for name,rel in [('original_beach','out/mc2-20260908/run-mc2-beach-002'),
                 ('airport','out/mc10-20260908/scene01-airport/run-03'),
                 ('sandy_airfield','out/mc10-20260908/scene03-sandy-airfield/run-02')]:
    old=read(PKG/rel/'evidence/native-record-result.json')['master']
    oldclip=seal(old['replay_path'])
    oldtelemetry=seal(old['player_telemetry_path'])
    first=json.loads(Path(oldtelemetry['path']).read_text(encoding='utf-8').splitlines()[0])['player_state']['position']
    check('distinct_master_'+name,oldclip['sha256']==old['replay_sha256'] and oldclip['sha256']!=expected_master
          and old['source_clip_identity']!=master['source_clip_identity'])
    different.append(dict(scene=name,master=oldclip,identity=old['source_clip_identity'],
                          first_position=first,horizontal_start_distance_m=math.dist(first[:2],world[0,:2])))

preflight_path=RUN/'evidence/preflight.json'
preflight_summary=None
if preflight_path.exists():
    pf=load_json(preflight_path)
    preflight_summary=dict(file=file_record(preflight_path),created_at=pf.get('created_at'),ok=pf.get('ok'),
        counts=dict(Counter(c['status'] for c in pf['checks'])),
        nonpass=[c for c in pf['checks'] if c['status']!='PASS'],internal_artifact_materialization=pf.get('internal_artifact_materialization'))
    # This is an existing ROOT-written preflight observation, not a new preflight/game probe.

unchanged=all(file_record(Path(p))==r for p,r in inputs.items())
check('all_original_inputs_rehashed_unchanged',unchanged,input_file_count=len(inputs))
result=dict(schema='dm-gta-pre-export-independent-review/1',created_at=datetime.now(timezone.utc).isoformat(),
    verdict='PASS' if all(x['status']=='PASS' for x in checks) else 'FAIL',
    scope='Offline source integrity and planned geometry only; not pixels, physical input, collision, metric or calibration approval.',
    run=str(RUN),checks=checks,counts=dict(Counter(x['status'] for x in checks)),inputs=list(inputs.values()),
    clip_id=tape['clip_id'],master_identity=master['source_clip_identity'],recording_camera=master['recording_camera'],
    route=route_review,lanes=lanes,translations=all_translations,planning_qc=freshqc_dict,
    covisibility=dict(slope_proxy_ratio=covis['covis_ratio'],horizontal_proxy_ratio=horizontal['covis_ratio'],plane_fit=covis['plane_fit'],actual_gta_covisibility_verified=False),
    distinct_prior_masters=different,existing_preflight=preflight_summary,
    input_semantics=dict(telemetry_contains_physical_input=False,planning_mouse='explicit synthetic angular-control placeholders only',
        tape_contains_mouse_or_key_events=False,final_metadata_must_use_native_frame_events=True,
        real_input_acceptance_pass=False,player_rotation='planning identity placeholder; telemetry contains genuine recording-side rotations'),
    limitations=['Entity origins are recording-side estimates; no in-band replay origin proof.',
        'Body envelope entityZ[-1.2,+1.85] and halfwidth0.35 is hypothetical, not measured feet/bones/pixels.',
        'Cam4 frame263 has narrow model-only edge margin; actual PNG review required.',
        'Horizontal and fitted slope planes are synthetic proxies, not terrain or GTA occlusion evidence.',
        'Nominal 50/24/18/35mm and K consistency are not approved optical/metric calibration.',
        'No capture frames, physical input, audio rights, final synchronization or delivery quality approved here.',
        'Planning input PASS cannot replace native input evidence; old writer fixed-zero semantics remain unresolved.',
        'Pre-record hello/probe refer to prior Sandy master; new source uses record.master and telemetry identities.'],
    game_api_ui_calls_performed=False,existing_artifacts_modified=False,formal_approved=False,
    calibration_approved=False,delivery_eligible=False,non_trainable=True)
(HERE/'audit.json').write_text(json.dumps(result,ensure_ascii=False,indent=2,allow_nan=False)+'\n',encoding='utf-8')
print(json.dumps(dict(verdict=result['verdict'],checks=result['counts'],route=route_review,
    margins={k:v['min_edge_margin_px'] for k,v in lanes.items()},tape_sha256=sha256_file(RUN/'config/camera-tape.json'),
    audit=file_record(HERE/'audit.json'),preflight_counts=preflight_summary['counts'] if preflight_summary else None),ensure_ascii=False,indent=2))

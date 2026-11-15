"""One bounded read-only source/ARM audit; no pipe, UI, preflight writer or capture."""
from __future__ import annotations
from collections import Counter
from datetime import datetime,timezone
import hashlib
import json
import math
from pathlib import Path
import sys

sys.dont_write_bytecode=True
RUN=Path(__file__).resolve().parent.parent
CAPTURE=RUN.parents[3]
BATCH=RUN.parents[1]
sys.path.insert(0,str(CAPTURE))
from gta_capture.tape import load_tape
from gta_capture.native_tape import HEADER,LANE_HEADER,SAMPLE,inspect_native_tape
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route


def main():
    out=RUN/'review/frozen-arm-integrity-01'
    if out.exists():raise ValueError('new review directory required; no overwrite')
    inputs={};checks=[];warnings=[]
    def record(path):
        path=Path(path).resolve();digest=hashlib.sha256()
        with path.open('rb') as f:
            for chunk in iter(lambda:f.read(1024*1024),b''):digest.update(chunk)
        item={'path':str(path),'bytes':path.stat().st_size,'sha256':digest.hexdigest()}
        previous=inputs.get(str(path))
        if previous is not None and previous!=item:raise ValueError('input changed during audit: '+path.name)
        inputs[str(path)]=item;return item
    def load(path):
        record(path);return json.loads(Path(path).read_text(encoding='utf8'))
    def check(name,ok,detail=None):
        checks.append({'id':name,'status':'PASS' if ok else 'FAIL','detail':detail})
    def sealed(path,digest):
        item=record(path);check('seal.'+Path(path).name,item['sha256']==digest);return item

    scenario=load(RUN/'config/scenario.json');tape=load(RUN/'config/camera-tape.json')
    route=load(RUN/'config/subject-route.json');manifest=load(RUN/'config/dependency-manifest.json')
    recorded=load(RUN/'evidence/native-record-result.json');arm=load(RUN/'evidence/native-lane-1-arm.json')
    plan_review=load(RUN/'review/plan-placement-review.json')
    master=recorded['master'];sha=master['replay_sha256'];anchor=tape['source']['anchor']
    check('record_real',recorded['ok'] is True and master['mock'] is False and master['recording_start_verified'] is True)
    check('scenario_applied_hash',recorded['scenario']['applied'] is True and recorded['scenario']['scenario_sha256']==record(RUN/'config/scenario.json')['sha256'])
    check('record_source_epoch',master['source_epoch']==scenario['source_epoch'])
    check('record_1800_observations',master['player_observation_count']==1800)
    for field in ('replay_path','source_clip_path'):sealed(master[field],sha)
    sealed(master['player_telemetry_path'],master['player_telemetry_sha256'])
    telemetry=[json.loads(s) for s in Path(master['player_telemetry_path']).read_text(encoding='utf8').splitlines() if s.strip()]
    check('telemetry_full_window',len(telemetry)==1800 and all(r['frame']==i and r['source_t_ns']==round(i*1e9/30)
         and r['replay_sha256']==sha and r['source_clip_identity']==master['source_clip_identity'] for i,r in enumerate(telemetry)))
    camera=master['recording_camera']
    check('recording_third_person_guard',camera['requested_view_mode']==camera['last_observed_view_mode']==1
          and camera['follow_ped_camera_active'] is True and camera['game_tick_observation_count']>=1800)
    check('recording_visibility_not_granted',camera['pixel_visibility_verified'] is False)
    first=telemetry[0]['player_state']['position'];last=telemetry[-1]['player_state']['position']
    start_delta=math.dist(anchor,scenario['location'])
    check('observed_placement_anchor',math.dist(anchor,first)<1e-8 and start_delta<=.25,
          {'requested_to_observed_gta_units':start_delta,'observed_xyz':first,'scenario_requested_xyz':scenario['location']})
    check('unrounded_world_origin_retained',math.dist(master['record_start_position'],first)<1e-6 and math.dist(first,[0,0,0])>1)
    check('route_exact_mirror',route==tape['source']['subject_route'])
    try:
        load_tape(RUN/'config/camera-tape.json',expected_duration_s=60)
        _validate_scenario_tape_route(Scenario.load(RUN/'config/scenario.json'),tape,expected_replay_sha256=sha)
        check('existing_complete_route_tape_validator',True)
    except Exception as exc:check('existing_complete_route_tape_validator',False,str(exc))
    for item in tape['source']['files']:sealed(Path(tape['source']['path'])/item['name'],item['sha256'])
    for item in route['observation']['files'].values():
        current=sealed(item['path'],item['sha256']);check('route_bound_size.'+Path(item['path']).name,current['bytes']==item['bytes'])
    check('route_observation_scope',route['observation']['player_replay_origin_verified'] is False
          and tape['source']['observation_semantics']=='recording_side_estimate')
    check('command_is_not_measured_speed',tape['source']['native_command']['speed_semantics']=='native_task_parameter_not_physical_mps'
          and tape['source']['native_command']['speed_mps']==scenario['player']['speed_mps'])
    check('plan_review_binding',plan_review['replay_sha256']==sha and plan_review['telemetry_sha256']==master['player_telemetry_sha256']
          and plan_review['camera_tape_sha256']==record(RUN/'config/camera-tape.json')['sha256'])
    summary={}
    for lane in ('1','2','3','4'):
        data=tape['lanes'][lane];samples=data['samples']
        check('timeline.'+lane,len(samples)==1800 and all(s['frame']==i and s['source_t_ns']==round(i*1e9/30) for i,s in enumerate(samples)))
        check('resolution.'+lane,data['resolution']=={'width':1920,'height':1080})
        summary[lane]={'focal_mm':data['focal_mm'],'n_frames':len(samples),'first_source_t_ns':samples[0]['source_t_ns'],
                      'last_source_t_ns':samples[-1]['source_t_ns'],'first_position_xyz':samples[0]['position'],
                      'pitch_range_deg':[min(s['euler_deg'][0] for s in samples),max(s['euler_deg'][0] for s in samples)]}
    check('four_focal_groups',sorted(v['focal_mm'] for v in summary.values())==[18,24,35,50])
    check('cam4_topdown',tape['source']['cam4_mode']=='topdown' and all(abs(s['euler_deg'][0]+85)<1e-7 for s in tape['lanes']['4']['samples']))
    binary=RUN/'config/camera-tape.bin';record(binary);envelope=inspect_native_tape(binary);blob=binary.read_bytes()
    check('binary_json_digest',envelope['json_sha256']==record(RUN/'config/camera-tape.json')['sha256'])
    check('binary_envelope',envelope['lane_count']==4 and envelope['frame_count']==1800 and envelope['fps_num']==30
          and envelope['fps_den']==1 and envelope['lane_stride']==LANE_HEADER.size and envelope['sample_stride']==SAMPLE.size)
    for index,lane in enumerate(('1','2','3','4')):
        data=tape['lanes'][lane];lh=LANE_HEADER.unpack_from(blob,HEADER.size+index*LANE_HEADER.size)
        expected_offset=HEADER.size+4*LANE_HEADER.size+index*1800*SAMPLE.size
        valid=lh==(int(lane),data['focal_mm'],1920,1080,expected_offset)
        for i,s in enumerate(data['samples']):
            row=SAMPLE.unpack_from(blob,expected_offset+i*SAMPLE.size)
            expected=(s['frame'],0,s['source_t_ns'],*s['position'],*s['quaternion'],*s['euler_deg'],
                      *(s['intrinsics'][k] for k in ('fx','fy','cx','cy')))
            valid=valid and row==expected
        check('binary_every_sample_equals_json.'+lane,valid)
    pid=recorded['hello']['process_id'];generation=recorded['hello']['controller_generation']
    for label in ('hello','probe'):
        v=arm[label]
        check('arm_live_new_master.'+label,v['process_id']==v['master_process_id']==pid
              and v['controller_generation']==generation and v['master_record_generation']==1
              and v['master_replay_sha256']==v['master_source_sha256']==sha and v['record_binding_ready'] is True
              and v['story_mode'] is True and v['online_session'] is False)
    check('arm_lane1',arm['ok'] is True and arm['lane']['armed'] is True and arm['lane']['lane']==1 and bool(arm['lane']['editor_session_id']))
    check('arm_v18',arm['hello']['fx_capture_protocol']==18 and arm['hello']['pixel_pose_fence_ready'] is True)
    installed_base=BATCH/'replus-cursor-refresh-derived-02'
    installed=load(installed_base/'manifest-installed.json');deployment=load(installed_base/'deployment-receipt.json')
    preflight=load(installed_base/'preflight-installed.json');source_verified=load(installed_base/'source-verification.json')
    check('run_manifest_equals_installed_manifest',manifest==installed)
    check('deployment_manifest_seal',deployment['installed_manifest']==record(installed_base/'manifest-installed.json'))
    component_status={}
    for name,item in manifest['components'].items():
        try:
            actual=record(item['path']);check('installed_component.'+name,actual['sha256']==item['sha256'])
            component_status[name]={'status':'PASS' if actual['sha256']==item['sha256'] else 'FAIL','file':actual}
        except PermissionError:
            component_status[name]={'status':'NOT_RECHECKED_PERMISSION','path':item['path'],'expected_sha256':item['sha256']}
            warnings.append('Current file read denied for '+name+'; prior installed preflight retained, not independently rechecked.')
    receipt=load(manifest['components']['capture_build_receipt']['path'])
    check('reviewed_patch_bound',receipt['source_provenance']['exact_patch']['sha256']==manifest['source_pins']['replus_patch_sha256']
          and source_verified['status']=='PASS' and source_verified['evidence']['reviewed_replus_patch_sha256']==manifest['source_pins']['replus_patch_sha256'])
    sealed(receipt['source_provenance']['exact_patch']['path'],manifest['source_pins']['replus_patch_sha256'])
    check('installed_replus_matches_deployment',deployment['installed_component']['sha256']==manifest['components']['replus_asi']['sha256'])
    retained=[v for v in preflight['checks'] if v['status']!='PASS']
    check('prior_preflight_only_calibration_gaps',{v['id'] for v in retained}=={'calibration.binding','calibration.projection_evidence','calibration.projection_contract'}
          and all(v['status']=='FAIL' for v in retained))
    policy=manifest['policy']
    check('internal_policy_unchanged',policy['purpose']=='internal_nontrainable_poc' and policy['story_mode_only'] is True
          and policy['online_forbidden'] is True and policy['non_trainable'] is True and policy['upload_enabled'] is False)
    covis=load(RUN/'config/recorded-plan/covis.json');provenance=load(RUN/'config/recorded-plan/provenance.json')
    check('planning_proxy_not_actual_geometry',covis['measured_in_gta'] is False and provenance['scene']['synthetic'] is True
          and tape['source']['collision_clearance_verified'] is False and tape['source']['delivery_eligible'] is False)
    # Frozen source inputs only: the in-progress partial event file is intentionally not read.
    for item in list(inputs.values()):record(item['path'])
    check('frozen_inputs_unchanged_during_audit',True)
    failures=[v for v in checks if v['status']=='FAIL']
    risks=[
        'Lane1 ARM proves intent and live master binding, not exported pixels. Require successful collect for all four distinct sessions, 4x1800 original PNG hashes/fences/times and same sealed master.',
        'Capture/review must independently check actual subject visibility and cursor/HUD absence. Cam4 planning body edge margin is only 16.13px; its entity-Z proxy does not measure feet or occlusion.',
        'Finalize still requires the real continuous non-silent 60s audio, exact 4x1800 video decode at 1920x1080/30fps, metadata roundtrip, real Editor project binding and unchanged configs/components.',
        'Use only the existing explicit internal calibration-gap materialization path; retain its three calibration FAILs and eventual real engine QC FAIL/SKIP. No approval or threshold change follows from this audit.',
        'The native-task source may retain the known input_stream QC failure; do not synthesize mouse input to turn it into a PASS.',
        'A final index requires ten distinct selected final runs with completed scene-review PASS and real preview receipts. This unfinished airport attempt is not yet eligible or counted complete.',
        'Pre-record hello/probe are earlier observations and need not have this new master binding; result.master and the later ARM are authoritative for this new recording.',
        'ARM stores no binary-tape digest itself. This audit proves on-disk JSON/bin agreement; actual per-frame native camera events must establish execution agreement after collection.'
    ]
    report={'schema':'dm-gta-mc10-frozen-arm-audit/1','created_at':datetime.now(timezone.utc).isoformat(),'run_dir':str(RUN),
        'status':'PASS' if not failures else 'FAIL','scope':'bounded frozen inputs plus lane1 ARM; not full scene review or quality gate',
        'may_continue_rendering_from_audited_inputs':not failures,'counts':dict(Counter(c['status'] for c in checks)),
        'checks':checks,'failures':failures,'warnings':warnings,'source_pid':pid,'controller_generation':generation,
        'master_replay_sha256':sha,'master_record_generation_from_arm':arm['hello']['master_record_generation'],
        'lane1_editor_session_id':arm['lane']['editor_session_id'],'lanes':summary,'binary':envelope,
        'observed_motion':{'recorded_displacement_gta_units':math.dist(first,last),**route['diagnostics']},
        'native_speed_parameter':scenario['player']['speed_mps'],'native_speed_is_physical_mps':False,
        'installed_components':component_status,'retained_prior_preflight':retained,'finalize_and_index_risks':risks,
        'formal_quality_approval':False,'human_review_approval':False,'calibration_approval':False,
        'delivery_eligible':False,'scene_complete':False,'raw_pngs_reviewed':0,'inputs':list(inputs.values()),'generator':record(__file__)}
    out.mkdir(exist_ok=False)
    with (out/'audit.json').open('x',encoding='utf8') as f:json.dump(report,f,ensure_ascii=False,indent=2,allow_nan=False)
    lines=['# 机场 run-03 冻结来源与 ARM 独立核对','',f"来源核对：{report['status']}；{report['counts']}。可基于本次已核对输入继续渲染：{not failures}。",'',
           f'母片 `{sha}`；同进程 {pid}，controller generation {generation}，ARM master generation 1。',
           '四路各 1800 帧，1920×1080、30 fps、60 秒；焦距 50/24/18/35 mm，二进制全部样本与 JSON 一致性见 JSON 检查。','',
           '本审计未读取进行中的 partial 帧文件，未核查任何实际 PNG，未运行 finalize、正式 QC、游戏或 pipe。', '', '后续限制：','']
    lines.extend('- '+v for v in risks)
    if warnings:lines+=['','本次独立读取限制：','']+['- '+v for v in warnings]
    if failures:lines+=['','失败检查：','']+['- '+json.dumps(v,ensure_ascii=False) for v in failures]
    with (out/'README.md').open('x',encoding='utf8') as f:f.write('\n'.join(lines)+'\n')
    print(json.dumps({'status':report['status'],'counts':report['counts'],'warnings':warnings,'output':str(out/'audit.json')},ensure_ascii=False))


if __name__=='__main__':main()

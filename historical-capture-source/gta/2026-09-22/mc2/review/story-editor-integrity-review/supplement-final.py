"""Bind completed project sidecar and final idle transport without mutating them."""
import hashlib
import json
import os
from pathlib import Path

HERE=Path(__file__).resolve().parent
BASE=HERE.parents[1]
RUN=BASE/'story-editor-transfer-draft-01/run'
records=[]

def record(path):
    path=Path(path).resolve();data=path.read_bytes()
    value={'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
    records.append(value)
    return value,data

def read(path):
    binding,data=record(path)
    return binding,json.loads(data)

all4_binding,all4=read(HERE/'all4.json')
assert all4['status']=='PASS' and all4['all_four_collected'] is True and not all4['failures']
project_binding,project=read(RUN/'evidence/editor-replay-binding.json')
runtime_binding,runtime=read(RUN/'evidence/editor-runtime-identity.json')
collect_binding,collect=read(RUN/'evidence/native-lane-4-result.json')
manifest_binding,manifest=read(RUN/'config/dependency-manifest.json')
assert project['schema']=='dm-gta-editor-replay-binding/1' and project['machine_verified'] is True
assert project['verification_method']=='replus-runtime-montage-v1'
master_sha=all4['full_alignment_readonly_diagnostic']['replay_sha256']
assert all(project[key]==master_sha for key in ('replay_sha256','source_clip_sha256','project_clip_reference_sha256'))
assert project['editor_project_fingerprint']==runtime_binding['sha256']
assert len(project['project_artifacts'])==1
for expected in [*project['project_artifacts'],project['source_clip'],project['source_original_clip']]:
    actual,_=record(expected['path'])
    assert actual['bytes']==expected['bytes'] and actual['sha256']==expected['sha256']
assert sorted(s['lane'] for s in project['lane_sessions'])==[1,2,3,4]
for session in project['lane_sessions']:
    actual=next(s for s in all4['selected_stills'] if s['lane']==session['lane'])['actual_event']['editor_binding']
    assert all(session[key]==actual[key] for key in ('session_id','project_name','clip_identity'))
    assert session['replay_sha256']==master_sha and session['editor_project_fingerprint']==runtime_binding['sha256']
assert collect['lane']['editor_project_binding'] is not None
final_binding,final=read(BASE/'story-after-transfer-editor-state.json')
assert len(final['samples'])==3
heartbeats=[]
for sample in final['samples']:
    state,transport=sample['editor'],sample['transport']
    assert state['publisher_pid']==collect['hello']['process_id']
    assert state['editor_active']==state['render_active']==state['edit_menu_active']==0
    assert transport['request_id']==transport['ack_id']==transport['observer_release_id']==transport['capture_ready_id']==738
    assert transport['generation']==transport['ready_generation']==600 and transport['status']==0
    heartbeats.append(state['heartbeat'])
assert all(a<b for a,b in zip(heartbeats,heartbeats[1:]))
ini_pin=manifest['components']['replus_render_ini']
ini_binding,ini_bytes=record(ini_pin['path'])
assert ini_binding['sha256']==ini_pin['sha256']
matches=[line.split('=',1)[1].strip() for line in ini_bytes.decode('utf-8-sig').splitlines() if line.strip().startswith('RenderOutputFolder=')]
assert len(matches)==1
for selected in all4['selected_stills']:
    root=selected['actual_event']['editor_binding']['normalized_output_root']
    assert os.path.normcase(str(Path(root).resolve()))==os.path.normcase(str(Path(matches[0]).resolve()))
report={'schema':'dm-gta-story-editor-final-sidecar-review/1','status':'PASS',
        'approval_status':'draft','approval_granted':False,'editor_equivalence_verified':False,
        'delivery_eligible':False,'metric_scale_approved':False,'non_trainable':True,
        'all4_integrity_report':all4_binding,'project_sidecar':project_binding,
        'project_sidecar_four_sessions_match_original_events':True,
        'project_runtime_artifact_hash_matches_fingerprint':True,
        'final_story_inactive_three_live_heartbeats':heartbeats,
        'final_transport_request_ack_release_ready':738,'final_transport_generation':600,
        'render_output_root_matches_installed_ini':True,
        'limitations':['Project identity integrity and idle transport are not optical calibration or Editor pixel equivalence.',
                       'No original run, result, event, image, receipt, gate or approval was changed.'],
        'inputs':records,'source':record(__file__)[0]}
target=HERE/'all4-sidecar-supplement.json'
with target.open('x',encoding='utf8') as f:json.dump(report,f,ensure_ascii=False,indent=2,allow_nan=False)
md=f'''# Story → Editor 四路采集完整性复核

**PASS：4 路各 150 帧，共 600 张真实 PNG，完整性检查零失败。**

全部 PNG 的字节数、SHA-256 和 PNG 结构通过；主报告绑定 693 个输入、13,626 项检查。四路均为同一录制进程与同一母片；实际 replay 时钟跨路最大差为 **0 ns**。原生 final-camera generation 全 600 帧连续，frame 45 / 120 共 8 张比较图逐一绑定原始 event 行、完整 event 文件、Editor PNG 及真实 Story 原图。

请求机位与真实 final-camera 读回最大差为 3.55×10⁻¹⁵ GTA units、7.72×10⁻⁷°（归一四元数）；FOV 控制 K 最大差为 5.23×10⁻¹² px。此项只验证控制一致性，**不声称 K 已实测标定**。

已封只读状态记录显示同一进程中的 Story inactive → Editor active → Story inactive，三阶段 heartbeat 均增长。最后三次 transport 的 request / ack / release / ready 均为 738，generation / ready generation 均为 600，状态 idle。

第 4 路 collect 新生成的非 null project binding 合法：四个 session、母片 hash、clip identity、项目名称与真实 frame events 一致，项目 runtime artifact 的 SHA 与 fingerprint 一致。报告没有要求该字段必须为 null；较早三路通过每帧实际 editor_binding 核对身份。

仍为 **draft / non-trainable**；本报告不执行像素配准，不批准 Editor 等价性、实米尺度或交付。录制侧人物遥测匹配不能替代 Editor replay 实体来源验证；音频、动态区域排除与光学匹配另行核验。未调用 UI / pipe / finalize，未修改 RUN 或任何原证据、门槛、审批。

主报告：`all4.json`，SHA-256 `{all4_binding['sha256']}`。
新增项目 sidecar 与最终 idle 状态的独立核查及输入哈希见 `all4-sidecar-supplement.json`。
'''
with (HERE/'all4-summary.md').open('x',encoding='utf8') as f:f.write(md)
print(json.dumps({'status':'PASS','report':str(target),'summary':str(HERE/'all4-summary.md'),'sha256':record(target)[0]['sha256']}))

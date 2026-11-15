"""One-shot plan writer only: no upload, deletion, copying or PNG content reads."""
from pathlib import Path
from datetime import datetime, timezone
from collections import defaultdict
import hashlib, json, re, stat

BASE=Path(__file__).resolve().parent
PKG=BASE.parents[1]
OLD=PKG/'out/mc2-20260908'
OUT=BASE/'review/completed-raw-retention-20260910-01'
assert not OUT.exists(), 'new output required'
RAW=Path('C:/capture/gta/replus-output')
SCENES=[('airport',BASE/'scene01-airport/run-03',[54,55,56,57]),
        ('sandy',BASE/'scene03-sandy-airfield/run-02',[64,65,66,67]),
        ('beach',OLD/'run-mc2-beach-002-reexport-02',[44,45,46,47])]
ALLOW={f'render_{n:04d}':(name,run,lane) for name,run,nums in SCENES for lane,n in enumerate(nums,1)}
inputs={}
def sha(p):
    h=hashlib.sha256()
    with Path(p).open('rb') as f:
        for b in iter(lambda:f.read(1024*1024),b''):h.update(b)
    return h.hexdigest()
def record(p):
    p=Path(p).resolve();r=dict(path=str(p),bytes=p.stat().st_size,sha256=sha(p));inputs[str(p)]=r;return r
def read(p):record(p);return json.loads(Path(p).read_text(encoding='utf-8'))
def stats(p):
    s=p.lstat()
    assert stat.S_ISREG(s.st_mode) and not (getattr(s,'st_file_attributes',0)&1024),str(p)
    return dict(bytes=s.st_size,mtime_ns=s.st_mtime_ns,ctime_ns=s.st_ctime_ns,
                device=s.st_dev,file_id=s.st_ino,nlink=s.st_nlink)
def write(p,obj):p.write_text(json.dumps(obj,ensure_ascii=False,indent=2,allow_nan=False)+'\n',encoding='utf-8')
keep=defaultdict(lambda:defaultdict(list))
def add(render,frame,reason):
    assert render in ALLOW and type(frame) is int and 0<=frame<1800
    if reason not in keep[render][frame]:keep[render][frame].append(reason)
for render in ALLOW:
    for f in list(range(0,1800,30))+[1799]:add(render,f,dict(kind='fixed_policy',rule='one_per_second_plus_last'))
    for f in list(range(259,267))+[857,858,1205,1206,1251,900]:
        add(render,f,dict(kind='explicit_planning_risk_reserve',note='prior discussed edge/body and cursor review checkpoints; not a new visual claim'))

roots=[BASE/'scene01-airport/run-03/review',BASE/'scene03-sandy-airfield/run-02/review',
       BASE/'review/scene01-airport-01',BASE/'review/scene01-airport-02',
       BASE/'review/scene01-airport-final-bundle-01',BASE/'review/scene01-airport-final-bundle-02',
       BASE/'review/scene01-preview-decoded-01',BASE/'review/scene03-sandy-airfield-01',
       BASE/'review/scene03-sandy-final-bundle-01',BASE/'review/scene03-preview-decoded-01',
       BASE/'preview/scene01-airport-01',BASE/'preview/scene03-sandy-airfield-01',
       OLD/'run-mc2-beach-002-reexport-02/preview']
roots += [p for p in (OLD/'review').glob('retry02*') if p.is_dir()]
sources=set(p for root in roots if root.exists() for p in root.rglob('*.json'))
sources.update((OLD/'review').glob('retry02*.json'))
PAT=re.compile(r'(render_(?:004[4-7]|005[4-7]|006[4-7]))[\\/]+frame_(\d{6})\.png',re.I)
def walk(o,field='$'):
    if isinstance(o,dict):
        for k,v in o.items():yield from walk(v,field+'.'+k)
    elif isinstance(o,list):
        for i,v in enumerate(o):yield from walk(v,field+f'[{i}]')
    elif isinstance(o,str):
        for m in PAT.finditer(o):yield m[1].lower(),int(m[2]),field
review_sources=[];skipped=[];examined=[]
for p in sorted(sources):
    if any(x in str(p) for x in ['body-framing-proposal','recorded-plan','pre-export-independent']):continue
    if 'known-template-scan' in p.name or 'known-cursor' in p.name or re.search(r'cursor-lane\d-all',p.name):
        skipped.append(dict(path=str(p),reason='Full-frame hash/template inventory does not mean every frame was visually reviewed.'))
        continue
    obj=json.loads(p.read_text(encoding='utf-8'));refs=list(walk(obj));examined.append(str(p))
    if not refs:continue
    unique={(r,f) for r,f,_ in refs}
    if any(sum(r==render for r,f in unique)>=1700 for render in ALLOW):
        skipped.append(dict(path=str(p),reason='Full-frame inventory excluded as keep-all rule.',unique_references=len(unique)))
        continue
    src=record(p);review_sources.append(src)
    for render,frame,field in refs:add(render,frame,dict(kind='named_existing_visual_or_targeted_diagnostic',source=src,field=field))

# Preserve the exact continuous window which was actually judged, not just periodic samples.
gait_path=OLD/'review/retry02-gait-19-21-diagnostic/diagnostic-receipt.json'
gait=read(gait_path);scope=gait['frame_scope']
assert scope==dict(lanes=[1,2],start_inclusive=570,end_exclusive=630,seconds_start=19,seconds_end_exclusive=21)
for lane in scope['lanes']:
    for frame in range(scope['start_inclusive'],scope['end_exclusive']):
        add(f'render_{43+lane:04d}',frame,dict(kind='actually_judged_contiguous_gait_window',source=inputs[str(gait_path.resolve())],start_inclusive=570,end_exclusive=630))

# Explicit non-path references from the frozen planning-risk and encoded-preview reports.
risk_path=BASE/'scene01-airport/run-03/review/lane4-framing-risk-01.json'
risk=read(risk_path);risk_frames={risk['worst']['output_frame']}
for key in ('worst_contiguous_12','worst_local_minima'):
    risk_frames.update(x['output_frame'] for x in risk[key])
risk_frames.update(risk.get('suggested_neighbor_frames',[]))
for f in risk_frames:add('render_0057',f,dict(kind='named_planning_risk_frame',source=inputs[str(risk_path.resolve())]))
joint_path=BASE/'scene03-sandy-airfield/run-02/review/body-framing-proposal-01/joint-body-margin.json'
joint=read(joint_path)
for lane,details in joint['lanes'].items():
    for item in details['worst_frames']:add(f'render_{63+int(lane):04d}',item['output_frame'],dict(kind='named_joint_body_risk_frame',source=inputs[str(joint_path.resolve())]))
preview_specs=[('airport',BASE/'review/scene01-preview-decoded-01/root-visual-observations-01.json','observations','encoded_frame_index_reported_by_root'),
               ('sandy',BASE/'review/scene03-preview-decoded-01/root-visual-observations-01.json','frames','source_frame')]
for name,p,items,key in preview_specs:
    obj=read(p)
    for frame in (x[key] for x in obj[items]):
        for render,(scene,_,_) in ALLOW.items():
            if scene==name:add(render,frame,dict(kind='original_counterpart_of_reviewed_encoded_preview',source=inputs[str(p.resolve())]))
bp=OLD/'review/retry02-final-mp4-frames/extraction-receipt.json';br=read(bp)
assert br['selection']=='Exact decoded frame indices 0,240,600,1080,1440,1797; 6 samples only'
for f in [0,240,600,1080,1440,1797]:
    for n in [44,45,46,47]:add(f'render_{n:04d}',f,dict(kind='original_counterpart_of_bounded_encoded_preview',source=inputs[str(bp.resolve())]))

mapping=BASE/'r2-staging-20260910-01/finished/SOURCE-RESTORE-MAPPING.jsonl'
mapping_record=record(mapping)
stage_rows=[json.loads(x) for x in mapping.read_text(encoding='utf-8').splitlines()]
stage_paths={Path(x['source_path']).resolve() for x in stage_rows}|{Path(x['staged_path']).resolve() for x in stage_rows}
assert not any(p.parent.name in ALLOW and p.suffix.lower()=='.png' for p in stage_paths)
renders=[];retained=[];discard=[];ancillary=[];core=[];scene_checks=[]
for name,run,nums in SCENES:
    master=read(run/'evidence/native-record-result.json')['master']
    rm=record(run/'run-manifest.json');qc=read(run/'evidence/engine-qc.json')
    assert qc['counts']=={'PASS':148,'FAIL':1,'SKIP':14,'UNVERIFIED':0} and qc['verdict']=='FAIL'
    for p in [run/'source/master-replay.clip',run/'source/master-player-telemetry.jsonl',run/'config/scenario.json',run/'config/camera-tape.json',run/'config/dependency-manifest.json']:
        assert p.is_file();core.append(dict(path=str(p),protection='whole_run_tree; never a PNG disposal candidate',stat=stats(p)))
    scene_checks.append(dict(scene=name,run_dir=str(run),master_replay_sha256=master['replay_sha256'],
        run_manifest=rm,final_qc=inputs[str((run/'evidence/engine-qc.json').resolve())],qc_counts=qc['counts'],formal_qc_verdict='FAIL'))
    for lane,n in enumerate(nums,1):
        render=f'render_{n:04d}';rd=RAW/render
        for parent in [RAW,*RAW.parents,rd]:assert not (getattr(parent.lstat(),'st_file_attributes',0)&1024),str(parent)
        ep=run/f'evidence/frame-events-lane{lane}.jsonl';er=record(ep)
        events=[json.loads(x) for x in ep.read_text(encoding='utf-8').splitlines()]
        collect=read(run/f'evidence/native-lane-{lane}-result.json')
        assert collect['ok'] is True and collect['lane']['frame_events_sha256']==er['sha256'] and len(events)==1800
        seen=set();lane_keep=[];lane_discard=[]
        for frame,event in enumerate(events):
            p=Path(event['output_path']);expected=rd/f'frame_{frame:06d}.png'
            assert p==expected and p.resolve()==expected and event['output_frame']==frame
            assert event['replay_sha256']==master['replay_sha256'] and event['editor_binding']['session_id']==collect['lane']['editor_session_id']
            assert event['editor_binding']['clip_identity']==master['source_clip_identity']
            assert re.fullmatch('[0-9a-f]{64}',event['output_sha256']) and p not in seen and p not in stage_paths
            seen.add(p);s=stats(p);assert s['bytes']==event['output_bytes'] and s['nlink']==1
            item=dict(render=render,scene=name,lane=lane,frame=frame,original_path=str(p),relative_path=p.name,
                bytes=event['output_bytes'],sha256=event['output_sha256'],
                sha256_method='sealed_native_event_digest; PNG content not rehashed in this plan',
                native_event=dict(file=er,output_frame=frame,replay_sha256=event['replay_sha256'],editor_session_id=event['editor_binding']['session_id'],output_sha256=event['output_sha256'],output_bytes=event['output_bytes']),
                current_stat=s,raw_content_rehashed_in_this_plan=False,remote_full_backup_verified=False,
                exact_reconstruction_available_after_discard=False)
            if frame in keep[render]:
                item.update(action='retain_in_place',reasons=keep[render][frame]);retained.append(item);lane_keep.append(item)
            else:
                item['action']='candidate_discard_not_executed';discard.append(item);lane_discard.append(item)
        actual=set(rd.iterdir());assert all(p.is_file() for p in actual)
        assert {p for p in actual if p.suffix.lower()=='.png'}==seen
        for p in sorted(actual-seen):ancillary.append(dict(path=str(p),render=render,stat=stats(p),action='retain_unconditionally',note='all non-PNG files, including audio and assemble, excluded from disposal'))
        renders.append(dict(render=render,scene=name,related_run_dir=str(run),raw_directory=str(rd),lane=lane,
            source_png_count=1800,source_png_bytes=sum(x['bytes'] for x in lane_keep+lane_discard),
            retained_png_count=len(lane_keep),retained_png_bytes=sum(x['bytes'] for x in lane_keep),
            candidate_discard_count=len(lane_discard),candidate_discard_bytes=sum(x['bytes'] for x in lane_discard),
            retained_frames=sorted(keep[render]),native_event=er,all_png_have_single_current_link=True,
            raw_content_rehashed_in_this_plan=False,remote_full_backup_verified=False))

assert len(retained)+len(discard)==21600
assert {x['original_path'] for x in retained}.isdisjoint(x['original_path'] for x in discard)
assert all(x['render'] in ALLOW for x in retained+discard)
assert all(all(f in keep[f'render_{43+lane:04d}'] for f in range(570,630)) for lane in (1,2))
assert all(stats(Path(x['original_path']))==x['current_stat'] for x in retained+discard)
assert all(Path(p).stat().st_size==r['bytes'] and sha(p)==r['sha256'] for p,r in inputs.items())
OUT.mkdir(parents=True,exist_ok=False)
for name,items in [('retained-files.jsonl',retained),('discard-candidates.jsonl',discard)]:
    with (OUT/name).open('w',encoding='utf-8',newline='\n') as f:
        for x in items:f.write(json.dumps(x,ensure_ascii=False,separators=(',',':'))+'\n')
protected=dict(schema='dm-gta-completed-raw-protected-scope/1',whole_run_trees=[str(run) for _,run,_ in SCENES],
    all_other_run_and_dependency_trees_protected=True,all_nonwhitelisted_render_directories_protected=True,
    active_render_0068_and_later_excluded=True,finished_source_restore_mapping=mapping_record,
    finished_stage_and_source_hardlinks_protected=True,core_file_stat_snapshots=core,raw_directory_ancillary_files=ancillary,
    rule='Only explicit discard-candidates.jsonl PNG paths may ever be considered. No directory deletion; all other content remains protected.')
write(OUT/'protected-scope.json',protected)
counts=dict(source_png_count=21600,source_png_bytes=sum(x['source_png_bytes'] for x in renders),
    retained_png_count=len(retained),retained_png_bytes=sum(x['bytes'] for x in retained),
    candidate_discard_count=len(discard),candidate_discard_bytes=sum(x['bytes'] for x in discard),
    protected_raw_ancillary_count=len(ancillary))
outputs=[dict(path=str(OUT/n),bytes=(OUT/n).stat().st_size,sha256=sha(OUT/n)) for n in ['retained-files.jsonl','discard-candidates.jsonl','protected-scope.json']]
plan=dict(schema='dm-gta-completed-raw-representative-retention-plan/1',created_at_utc=datetime.now(timezone.utc).isoformat(),
    status='PLAN_ONLY_WAITING_FINISHED_PACKAGE_VERIFIED',planning_authorization='ROOT adopted user-authorized rolling archival and explicitly requested this completed-RAW plan; no execution delegated to this writer.',
    execution_performed=False,execution_approval_granted_by_this_plan=False,scope_render_whitelist=list(ALLOW),
    scene_priority=['airport','sandy','beach'],fixed_policy=dict(every_30_frames=list(range(0,1800,30)),last_frame=1799,
        retain_in_place=True,new_raw_hardlinks_created=0,beach_judged_contiguous_window=dict(lanes=[1,2],frames_inclusive=[570,629],total_frames=120)),
    selection_method='Union of periodic/last/risk reserves, bounded reports explicitly naming raw PNGs, stated encoded-frame counterparts and the full judged beach gait window. Full-frame hash/template scans are not keep-all rules.',
    source_input_records=list(inputs.values()),review_sources=review_sources,review_files_examined=examined,
    not_used_as_keep_all=skipped,renders=renders,counts=counts,outputs=outputs,scene_checks=scene_checks,
    execution_boundary=[
        'Do not execute before finished package upload-report.status=verified with copy/check exit0 and source stability/count/bytes verified for the exact frozen finished mapping; archives/index verified alone is insufficient.',
        'Confirm all twelve selected lanes are completed, no writer/open capture owns these exact directories, and render0068+ plus all nonwhitelisted directories remain excluded.',
        'Before any disposal, rehash the plan/lists/native event files and required finished package receipts; compare original full scene-verification and finalization evidence without changing QC FAIL.',
        'Verify each retained PNG actual bytes and SHA against its native event before disposing its lane; preserve every retained path and all non-PNG files.',
        'Immediately before each candidate removal, recheck exact absolute literal path under its one whitelisted root, no reparse component, file device/id/nlink1/size/mtime/ctime unchanged, then read actual SHA and require equality with sealed native event; reject drift or missing files.',
        'Use a per-file nonrecursive operation and append exact path/bytes/SHA/identity to a new journal. Never erase/rewrite old source events, completed run trees, manifests, QC, previews or build dependency closure.',
        'Record actual released counts/bytes and irreversible raw loss honestly; remaining MP4 and master replay cannot reconstruct identical discarded PNGs. This is not a remote-full-RAW backup claim.'
    ],remote_complete_raw_backup_verified=False,finished_package_verified_by_this_plan=False,
    raw_content_rehashed_in_this_plan=False,exact_raw_reconstruction_promised=False,
    known_loss_if_executed='Discarded lossless native PNG pixels become unavailable for full original-image re-verification; SHA and lossy video cannot reconstruct them. Old passing integrity reports remain historical observations.',
    formal_delivery_eligible=False,no_upload=True,no_deletion=True,old_sources_and_events_modified=False)
write(OUT/'retention-plan.json',plan)
lines=['# 三组成片原帧：代表帧保留与处置候选','',
    '**仅计划，未删除、未上传、未授予正式验收。执行前必须完成 finished 整包 verified；索引 verified 不够。**','',
    '优先机场，其次 Sandy，最后原海滩。原海滩不计入额外十场；当前北滩活动 render0068 及之后目录完全排除。','',
    '| 场景 | RAW 目录 | 保留张数 | 候选处置张数 | 候选逻辑字节 |','|---|---|---:|---:|---:|']
for name,_,nums in SCENES:
    rr=[x for x in renders if x['scene']==name]
    lines.append(f"| {name} | {nums[0]:04d}–{nums[-1]:04d} | {sum(x['retained_png_count'] for x in rr)} | {sum(x['candidate_discard_count'] for x in rr)} | {sum(x['candidate_discard_bytes'] for x in rr):,} |")
lines+=['',f"总计保留 {len(retained)} PNG（{counts['retained_png_bytes']:,} 字节）；候选 {len(discard)} PNG（{counts['candidate_discard_bytes']:,} 字节）。这是逻辑大小，不是已经释放的磁盘空间。",'',
    '每路保留每秒一帧及1799，另取已有联系表、真实视觉/局部诊断明确引用的原图、规划边缘风险点和最终编码抽查帧的原始对应帧。海滩第1/2路570–629各60张、共120张实际判断过的连续窗口完整保留。全量哈希/模板扫描不是全量目视保留要求；这些原报告仍完整保护。','',
    '保留母片、遥测、全部run树、场景、plan/tape、dependency来源、全部事件与QC、MP4/quad/联系表、原始音频及RAW目录全部非PNG附属文件。候选仅指两个JSONL明确分区中的PNG，不递归处理目录。','',
    '本计划只复用封存native事件中的逐帧SHA，重新核对事件文件SHA、collect结果绑定、21600文件路径/大小/ID/nlink/mtime/ctime；未抢渲染IO重读全部71GB原图。正式执行前必须实际核对保留原图SHA，并对每个待处置文件再次核对真实SHA和快照身份；计划本身不授权绕过这些检查。','',
    '清理后成片仍可访问，但被处置的原始无损PNG无法由MP4、SHA或母片精确重建，依赖全部原图的完整验证不能再直接复跑。现有148 PASS / 1 FAIL / 14 SKIP及input/calibration/non-trainable限制原样保留。','',
    '精确文件及执行前条件见 [retention-plan.json](retention-plan.json)、[retained-files.jsonl](retained-files.jsonl)、[discard-candidates.jsonl](discard-candidates.jsonl)、[protected-scope.json](protected-scope.json)。','']
(OUT/'README-代表帧保留候选.md').write_text('\n'.join(lines),encoding='utf-8')
print(json.dumps(dict(out=str(OUT),counts=counts,per_scene=[dict(scene=name,retained=sum(x['retained_png_count'] for x in renders if x['scene']==name),candidates=sum(x['candidate_discard_count'] for x in renders if x['scene']==name),candidate_bytes=sum(x['candidate_discard_bytes'] for x in renders if x['scene']==name)) for name,_,_ in SCENES],plan_sha256=sha(OUT/'retention-plan.json')),ensure_ascii=False,indent=2))

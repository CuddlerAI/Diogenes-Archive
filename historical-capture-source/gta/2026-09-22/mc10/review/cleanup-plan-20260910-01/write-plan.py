"""Add a cleanup priority plan from stat-only inventory. No deletion or remote calls."""
from pathlib import Path
import json, os, hashlib, datetime
HERE=Path(__file__).resolve().parent
I=json.loads((HERE/'inventory.json').read_text('utf-8'))
RAW=Path('C:/capture/gta/replus-output')
R={int(Path(r['path']).name.split('_')[1]):r for r in I['raw_directories']}

def record(p):
    b=p.read_bytes(); return {'path':str(p),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()}

def group(label, nums, priority, reason):
    rows=[R[n] for n in nums]
    return {'name':label,'priority':priority,'classification':reason,
      'logical_bytes':sum(x['logical_bytes'] for x in rows),'png_bytes':sum(x['png_bytes'] for x in rows),
      'png_files':sum(x['png_files'] for x in rows),
      'directories':[{'path':x['path'],'logical_bytes':x['logical_bytes'],'files':x['files'],
       'png_files':x['png_files'],'png_bytes':x['png_bytes'],
       'event_references':x['event_references']} for x in rows],
      'release_condition':'after verified archival of exact originals and companion evidence; preserve local small manifests, receipts and reviews',
      'unique_local_original_evidence_assumed':True,'current_remote_verified':False}

groups=[
 group('POC061 历史已上传 RAW，先核当前远端',range(37,41),1,'historical finalized POC; existing upload receipts, current remote not checked'),
 group('港区两个已弃用尝试 RAW',range(58,62),2,'run-02 framing failure and run-02-reexport-01 cursor failure; no complete selected scene'),
 group('机场 run-02 指针失败 RAW',range(52,54),2,'superseded by successful run-03; preserves original cursor failure pixels'),
 group('海滩前序失败/取消 RAW',range(41,44),3,'mc2 earlier attempts; render41 first-person and render42/43 cursor/cancel evidence; partial event association incomplete'),
 group('最终海滩内部成片 RAW',range(44,48),4,'completed 4 x 1800 internal demo; source pixels remain unique evidence'),
 group('最终机场内部成片 RAW',range(54,58),4,'selected batch scene01, 4 x 1800; retain provenance/QC and final videos'),
 group('最终 Sandy 内部成片 RAW',range(64,68),4,'selected batch scene03, 4 x 1800; retain provenance/QC and final videos'),
 group('Story→Editor 实测对照 RAW',range(48,52),5,'600 real calibration-transfer diagnostic frames; not disposable synthetic output'),
 group('DrawMovie guard 12s 实测短片 RAW',range(62,64),5,'2 x 360 real runtime regression frames; not selected 60s content'),
 group('其他历史 POC RAW，状态未逐项判定',range(1,37),6,'historical task evidence; absent event association does not imply safe deletion'),
]

failure=[]
for rootrow in I['local_top_level_directories']:
    root=Path(rootrow['path'])
    if not str(root).startswith('C:\\capture\\gta\\runs') or rootrow['png_files']==0: continue
    for base,dirs,files in os.walk(root,followlinks=False):
        dirs[:]=[d for d in dirs if not (Path(base)/d).is_symlink()]
        if 'failure' not in Path(base).parts or not any(n.lower().endswith('.png') for n in files):continue
        rows=[]
        for name in files:
            p=Path(base)/name; st=p.stat(follow_symlinks=False)
            if getattr(st,'st_file_attributes',0)&0x400:continue
            rows.append((p,st.st_size))
        p=Path(base); prefix=p.name.split('-render_')[0]
        sidecars=[record(q) for q in p.parent.glob(prefix+'-*') if q.is_file() and q.suffix in {'.json','.jsonl'}]
        failure.append({'path':str(p),'files':len(rows),'logical_bytes':sum(s for q,s in rows),
          'png_files':sum(q.suffix.lower()=='.png' for q,s in rows),
          'png_bytes':sum(s for q,s in rows if q.suffix.lower()=='.png'),
          'associated_sidecars':sidecars,'classification':'moved-aside original failure pixels, not proven duplicate scratch',
          'original_event_path_may_point_to_reused_render0033':True,
          'release_condition':'archive under present retained directory path with sibling failure JSON/JSONL; do not flatten into reused render_0033'})

historical=[]
for p in sorted(Path('C:/capture/gta/upload-reports').glob('poc061-*/upload-report.json')):
    d=json.loads(p.read_text('utf-8-sig'))
    raw=next((r for r in R.values() if os.path.normcase(r['path'])==os.path.normcase(d.get('source',''))),None)
    historical.append({'report':record(p),'status':d.get('status'),'source':d.get('source'),
       'destination':d.get('destination'),'finished_at':d.get('finished_at'),
       'copy_exit_code':d.get('copy_exit_code'),'check_exit_code':d.get('check_exit_code'),
       'source_after':d.get('source_after'),'destination_after':d.get('destination_after'),
       'current_local_count_and_size_match': bool(raw and raw['files']==d['source_after']['count'] and raw['logical_bytes']==d['source_after']['bytes']) if raw else None,
       'current_remote_verification_performed':False})

cache=[]
for b in I['build_directories']:
    files=[]
    for f in b['intermediate_files']:
        # Prior inventory references are diagnostic descriptions, not build provenance pins.
        direct=[p for p in f['direct_receipt_references'] if 'storage-audit-' not in p and 'cleanup-plan-' not in p]
        files.append({**f,'build_or_other_direct_references':direct})
    cache.append({'path':b['path'],'whole_directory_bytes_not_safe_to_delete':b['logical_bytes'],
       'generated_intermediate_bytes':sum(f['bytes'] for f in files),
       'without_non_storage_audit_reference_bytes':sum(f['bytes'] for f in files if not f['build_or_other_direct_references']),
       'files':files})

doc={'schema':'dm-gta-local-cleanup-plan/1','created_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
 'inventory':record(HERE/'inventory.json'),'only_new_report_writes':True,'deletion_performed':False,
 'remote_verification_performed':False,'raw_pixels_read_or_hashed':False,'formal_approval_granted':False,
 'priority_groups':groups,'historical_moved_failure_originals':failure,
 'historical_moved_failure_originals_bytes':sum(f['logical_bytes'] for f in failure),
 'historical_upload_receipts':historical,'build_intermediates':cache,
 'build_intermediate_total_bytes':sum(c['generated_intermediate_bytes'] for c in cache),
 'build_intermediate_without_non_audit_refs_bytes':sum(c['without_non_storage_audit_reference_bytes'] for c in cache),
 'scratch':I['media_scratch'],
 'classification_limits':['sizes are exact observed logical bytes, not allocation/reclaim guarantee',
  'PNG compression savings were not measured; PNG and MP4 are already compressed',
  'no cloud existence/content check in this subtask; old verified report is historical only',
  'two analysis-runtime subdirectories denied stat enumeration; excluded from cleanup candidates',
  'scope tables overlap; do not sum raw + run tables + failure subtrees twice'],
 'archive_restore_contract':{
  'preferred':'copy files to new immutable R2 prefix, keep root-map and path/bytes/SHA manifest; no need to construct a 25GB local archive before upload',
  'alternative':'bounded per-render/per-attempt tar or ZIP64 archive with relative members and root-map; lossless compression only',
  'root_map_examples':{'capture/gta/replus-output':'C:/capture/gta/replus-output','capture/gta/runs':'C:/capture/gta/runs'},
  'must_preserve':['PNG exact bytes','audio and assemble files','frame events and arm/collect receipts','config/tapes/master replay/telemetry','run manifest/metadata/formal QC','code/source/build/SDK/parent receipt dependencies'],
  'verification_before_release':['local source stability and explicit path inventory','remote object count/bytes and content verification per upload workflow','source SHA mapping for every archived file; native PNG SHA reused only after archive bytes actually checked','restore a representative member from each package and check SHA; keep whole-package/object integrity evidence','write additive archive accession and release log outside immutable run files'],
  'restoration':'restore exact bytes to original retained absolute paths from root-map; do not rewrite sealed event/run paths or SHA; keep historical collision directories separate',
  'while_offline':'full RAW revalidation cannot run until originals restored; final MP4, metadata, reviews and QC remain available locally; no claim that cloud move changes existing formal gates'},
 'recommended_first_wave_bytes':groups[0]['logical_bytes']+groups[1]['logical_bytes']+groups[2]['logical_bytes'],
 'completed_three_internal_demo_raw_bytes':sum(g['logical_bytes'] for g in groups if g['priority']==4)}
with (HERE/'plan.json').open('x',encoding='utf-8') as f:json.dump(doc,f,ensure_ascii=False,indent=2);f.write('\n')
lines=['# 本地空间清理计划（只读盘点）','',
 f'C 盘本次可用 {I["volumes"][0]["available_bytes"]:,} 字节（约 {I["volumes"][0]["available_bytes"]/1e9:.2f} GB），没有新增可用挂载卷。67 个 RAW 渲染目录共 {I["totals"]["raw_logical_bytes"]:,} 字节 / {I["totals"]["raw_png_files"]:,} 张 PNG。本报告未删除、移动、压缩或上传文件，也未读取 PNG 内容。',
 '', '优先顺序如下。字节数是当前文件逻辑长度；不是承诺释放的磁盘分配量。所有 RAW 都按目前唯一保留的原始证据处理：废弃尝试也先归档、验证再释放；成片 MP4 不能替代 PNG。', '',
 '| 顺序 | 目录组 | 当前字节 | PNG 数 |','|---|---|---:|---:|']
for g in sorted(groups,key=lambda g:g['priority']):
 names=', '.join(Path(d['path']).name for d in g['directories'])
 lines.append(f'| {g["priority"]} | {g["name"]}：{names} | {g["logical_bytes"]:,} | {g["png_files"]:,} |')
lines += ['',f'最快候选是历史 POC061 已上传四路：本地数量与字节数仍吻合 2026-08-31 的 verified 回执，但本次未检查远端当前内容。先核验该远端，然后处理港区两次失败与机场旧 attempt，三组合计 {doc["recommended_first_wave_bytes"]:,} 字节（约 {doc["recommended_first_wave_bytes"]/1e9:.2f} GB）。远端地址与回执 SHA 见 plan.json；不把历史回执直接当作今天可删许可。',
 '',f'另外，C:/capture/gta/runs 下 failure/rejected-* 目录有 {doc["historical_moved_failure_originals_bytes"]:,} 字节旧失败原图。它们不是已证实重复的 scratch：例如原 render_0033 当前没有 PNG，而失败目录保留像素。归档须保留现有不同 failure 目录，不能合并成同一个 render_0033；部分旧事件仍引用被复用的旧路径，报告不改写历史证据。',
 '', '| 旧失败原件精确目录 | 字节 | PNG |','|---|---:|---:|']
for r in failure:lines.append(f'| {r["path"]} | {r["logical_bytes"]:,} | {r["png_files"]:,} |')
lines += ['',f'重复临时文件没有实质空间收益：唯一 media-scratch 是 review/media-scratch-smoke-02/failure-second-mux 下的合成测试，只有 {I["totals"]["scratch_logical_bytes"]:,} 字节。构建生成的 .obj/.pch/.idb/.ilk/.tlog 合计 {doc["build_intermediate_total_bytes"]:,} 字节；排除仅被空间审计引用的条目后，无其他直接来源引用的候选为 {doc["build_intermediate_without_non_audit_refs_bytes"]:,} 字节。精确文件在 plan.json；不要整目录删除 build：其中 ASI、CMakeCache、编译器配置、工程文件和日志仍是当前 v1/v2/v3 来源验证链的一部分。源码快照、SDK、父回执、旧 ASI 备份也保留。',
 '', '归档和恢复：', '',
 '1. 优先将每个 render 目录及对应 run 的 source/config/evidence、元数据、QC 和来源依赖复制到独立 R2 前缀；本地保留小型 manifest/审核/README 和最终视频。无需在仅 21GB 剩余盘上先构造整场 25GB 压缩包。',
 '2. 若需压缩，可按单路或单 attempt 做无损 tar/ZIP64，使用相对成员名加根路径映射。PNG、MP4 已压缩，未测额外节省率，不承诺压缩收益。禁止重编码、改图、覆盖已有证据。',
 '3. 上传流程核验源稳定性、远端数量/字节及内容，形成逐文件 SHA 清单和归档包 SHA。原生事件已声明 PNG SHA，可作期望值，但本盘点没有重算，不冒充本次内容复核。每包还原抽检及 SHA 对照后，由 ROOT 执行精确名单释放，写追加式归档/释放回执。',
 '4. 恢复时按 root-map 放回原保留绝对路径，逐字节 SHA 应与原事件一致；历史失败目录名保持独立。不改封存 JSON 以适配云路径。RAW 离线期间不能重跑需要原图的完整审核，必须先恢复；已有正式 FAIL 和 non-trainable 状态不因归档改变。',
 '', '本次仅统计本任务两个 out 批次、C:/capture/gta/runs 与 replus-output。没有扫描或清理其他用户文件。analysis-runtime 的 cv2 和 dist-info 两目录读取受限，未列为候选。inventory.json 中的不同层级统计存在包含关系，不应叠加。完整精确路径、字节和事件绑定见 plan.json / inventory.json。', '']
with (HERE/'SUMMARY.md').open('x',encoding='utf-8') as f:f.write('\n'.join(lines))
print(json.dumps({'plan':record(HERE/'plan.json'),'summary':record(HERE/'SUMMARY.md'),
 'first_wave_bytes':doc['recommended_first_wave_bytes'],
 'completed_raw_bytes':doc['completed_three_internal_demo_raw_bytes'],
 'historical_failure_bytes':doc['historical_moved_failure_originals_bytes'],
 'build_intermediate_bytes':doc['build_intermediate_total_bytes'],
 'build_non_audit_unreferenced_bytes':doc['build_intermediate_without_non_audit_refs_bytes'],
 'failure_directories':len(failure)},ensure_ascii=False,indent=2))

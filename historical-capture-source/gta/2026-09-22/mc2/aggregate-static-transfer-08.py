"""Rehash and summarize all eight real static measurements; no new image matching."""
import hashlib
import json
import math
from pathlib import Path

ROOT=Path(__file__).resolve().parent/'review'
OUT=ROOT/'story-editor-transfer-summary-01'
FOLDERS={1:'story-editor-transfer-lane1-02',2:'story-editor-transfer-lane2-01',
         3:'story-editor-transfer-lane3-01',4:'story-editor-transfer-lane4-01'}


def load(path):return json.loads(path.read_text(encoding='utf-8'))


def ref(path):
    path=Path(path).resolve()
    return {'path':str(path),'bytes':path.stat().st_size,
            'sha256':hashlib.sha256(path.read_bytes()).hexdigest()}


def check(record):
    if ref(record['path'])!=record:raise ValueError('source hash/bytes changed: '+record['path'])


def percentile(values,percent):
    order=sorted(values);index=(len(order)-1)*percent/100
    lower=math.floor(index);upper=math.ceil(index)
    return order[lower]+(order[upper]-order[lower])*(index-lower)


def inside(point,polygon):
    x,y=point;hit=False
    for p,q in zip(polygon,polygon[1:]+polygon[:1]):
        if ((p[1]>y)!=(q[1]>y)) and x<(q[0]-p[0])*(y-p[1])/(q[1]-p[1])+p[0]:hit=not hit
    return hit


pairs=[];methods=[];helper_shas=set();checked_sources={}
for lane,dirname in FOLDERS.items():
    directory=ROOT/dirname;binding_path=directory/'source-bindings.json';binding=load(binding_path)
    if binding['sealed_lane_event_count']!=150 or binding['formal_calibration_approval'] is not False:
        raise ValueError('source binding is not the real unapproved short lane')
    for name in ('frame_events','comparison_plan'):
        check(binding[name]);checked_sources[binding[name]['path']]=binding[name]
    for hold in (1,2):
        path=directory/f'hold{hold}-measurement'/'displacement.json';report=load(path)
        execution=load(directory/f'hold{hold}-execution.json')
        if execution['exit_code']!=0 or report['formal_calibration_approval'] is not False or report['editor_equivalence_verified'] is not False:
            raise ValueError('measurement must be real successful diagnostic and unapproved')
        for record in [report['helper'],report['regions'],*report['inputs'].values()]:
            check(record);checked_sources[record['path']]=record
        selected=next(b for b in binding['bindings'] if b['hold']==hold)
        for name in ('story','editor'):
            if selected[name]!=report['inputs'][name]:raise ValueError('measurement input differs from frame binding')
        methods.append(report['method']);helper_shas.add(report['helper']['sha256'])
        raw=[]
        for match in report['matches']:
            vector=[b-a for a,b in zip(match['story_px'],match['editor_px'])]
            distance=math.hypot(*vector)
            if any(abs(a-b)>1e-10 for a,b in zip(vector,match['raw_displacement_px'])) or abs(distance-match['raw_distance_px'])>1e-10:
                raise ValueError('reported residual differs from original coordinates')
            raw.append(distance)
        if not raw or len(raw)!=report['match_count']:raise ValueError('missing actual matches')
        recomputed={'raw_distance_rms_px':math.sqrt(sum(x*x for x in raw)/len(raw)),
                    'raw_distance_p50_px':percentile(raw,50),'raw_distance_p95_px':percentile(raw,95),
                    'raw_distance_max_px':max(raw)}
        if any(abs(value-report['statistics_all_mutual_matches'][key])>1e-10 for key,value in recomputed.items()):
            raise ValueError('summary statistics do not match all original-coordinate residuals')
        regions=load(Path(report['regions']['path']))
        counts=[{'label':r['label'],'match_count':sum(inside(m['story_px'],r['polygon']) for m in report['matches'])}
                for r in regions['story']['include']]
        pairs.append({'lane':lane,'nominal_focal_mm':(18,24,35,50)[lane-1],'hold':hold,
            'pitch_deg':(-5,5)[hold-1],'editor_output_frame':selected['output_frame'],
            'measurement':ref(path),'source_bindings':ref(binding_path),
            'execution':ref(directory/f'hold{hold}-execution.json'),'inputs':report['inputs'],
            'regions':report['regions'],'match_count':len(raw),'recomputed_raw_statistics':recomputed,
            'story_coverage':report['coverage']['story'],'editor_coverage':report['coverage']['editor'],
            'story_mask_fraction':report['features']['story']['mask_image_fraction'],
            'region_match_counts':counts,'largest_retained_match':max(report['matches'],key=lambda m:m['raw_distance_px']),
            'formal_calibration_approval':False,'editor_equivalence_verified':False})
if len(helper_shas)!=1 or any(m!=methods[0] for m in methods):
    raise ValueError('helper bytes or measurement method changed across the eight pairs')
if (methods[0]['ratio_threshold_both_directions']!=0.70 or methods[0]['ransac'] is not False or
    methods[0]['translation_applied'] is not False or methods[0]['resizing_applied'] is not False or
    methods[0]['warp_or_homography'] is not None or methods[0]['spatial_match_filter'] is not None):
    raise ValueError('unexpected geometry correction or threshold change')
integrity_path=ROOT/'story-editor-integrity-review'/'all4.json'
integrity=load(integrity_path)
if integrity.get('status')!='PASS' or integrity.get('completed_lanes')!=[1,2,3,4] or integrity.get('approval_granted') is not False:
    raise ValueError('independent integrity report must cover all four actual lanes without approval')
summary={'schema':'dm-gta-story-editor-static-transfer-summary/1','pair_count':8,
         'original_png_count':16,'complete_lane_event_count':600,'all_original_input_hashes_reverified':True,
         'statistics_recomputed_from_all_retained_raw_coordinates':True,
         'shared_helper_sha256':next(iter(helper_shas)),'shared_method':methods[0],
         'source_inventory':list(checked_sources.values()),'pairs':pairs,
         'independent_integrity_review':ref(integrity_path),
         'integrity_pass_is_not_projection_equivalence':True,
         'interpretation':'Local static-feature pixel consistency only; no full-image, metric-depth, animated-entity or formal calibration approval.',
         'formal_calibration_approval':False,'editor_equivalence_verified':False}
OUT.mkdir(parents=True,exist_ok=False)
with (OUT/'summary.json').open('x',encoding='utf-8',newline='\n') as stream:
    json.dump(summary,stream,indent=2,ensure_ascii=False,allow_nan=False);stream.write('\n')
lines=['# Story → Editor 八组局部静态像素对照（诊断）','',
       '四个焦段、每个两种倾角的真实原图已完成同一算法测量。表中保留全部双向匹配的原始像素差，未进行几何修正或按误差剔除点。正式批准仍为 false。','',
       '| 焦段标签 / 倾角 | 匹配数 | RMS px | p95 px | max px | 静态 mask / 匹配凸包占比 |',
       '|---|---:|---:|---:|---:|---:|']
for pair in pairs:
    s=pair['recomputed_raw_statistics']
    lines.append(f"| {pair['nominal_focal_mm']} mm / {pair['pitch_deg']:+}° | {pair['match_count']} | {s['raw_distance_rms_px']:.6f} | {s['raw_distance_p95_px']:.6f} | {s['raw_distance_max_px']:.6f} | {pair['story_mask_fraction']*100:.2f}% / {pair['story_coverage']['convex_hull_image_fraction']*100:.2f}% |")
lines+=['','所有相机均复核了 Editor 第 45/120 帧及对应 Story 原图。多边形逐对依据实际原图预先选定，排除人物、植被、天空/云、可见投影边缘及漂动杂物；mask 外全部不参与测量。SIFT 保持原尺寸、双向 ratio 0.70、互为最近描述符、完整静态支持区要求。没有 warp、homography、RANSAC、平移扣除、缩放或位置门限。','',
       '**保留极值**：24 mm / -5° 的 1.016332 px 点位于黄色球场圆弧旁重复纹理，歧义尚未解决。35 mm / -5° 的 40.584344 px 点经同坐标原图裁切实看，描述符把白楼上层窗框配到下一层相似窗框；这是跨楼层重复结构错配样态，不能直接当成镜头投影偏移。其原始 RMS 1.562569 px 和全部极值仍保留，未删点或用替代结果覆盖。该组另有未解决的 1.578403 px 球场点。诊断裁切仅用于解释，不参与测量或正式批准。','',
       '**来源**：四条封存事件序列各 150 帧，实际比较 8 张 Editor PNG 与 8 张 Story PNG；汇总重新核验来源与原图 SHA，并从每个保留点的原坐标独立重算统计。完整路径、字节数、SHA、选定事件来源、算法/区域/执行回执哈希及逐区域匹配数在 `summary.json`。没有改写任何原图、samples 或 run 证据。','',
       '**覆盖不足**：50 mm / +5° 仅 51 个描述符匹配、39 个半像素分箱位置，凸包仅画面 6.30%，集中在窄带；原始 max 1.420978 px 仍未独立解决。其他组的凸包也不代表内部每个像素均通过。','',
       '**范围**：匹配主要来自球场、墙面和少量建筑，场景深度未做米制测量；点覆盖不代表全画面，也不验证人物骨骼同步、长片全部时段或任意新相机。各路详细目视检查、动态差异和覆盖限制见对应 lane 报告。描述符重复和误配仍可能存在。独立 `story-editor-integrity-review/all4.json` 的完整性 PASS 仅证明其声明的来源/帧/相机边界，不是像素投影等价。正式 `editor_equivalence_verified` 与 `formal_calibration_approval` 均保持 false。']
with (OUT/'SUMMARY.md').open('x',encoding='utf-8',newline='\n') as stream:stream.write('\n'.join(lines)+'\n')
print(json.dumps({'output':str(OUT),'pairs':len(pairs),'original_pngs':16,'formal_calibration_approval':False}))

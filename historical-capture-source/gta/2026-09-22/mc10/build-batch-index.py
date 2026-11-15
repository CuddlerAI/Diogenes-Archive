"""Index ten explicitly selected, finalized and reviewed internal scenes only."""
from __future__ import annotations
import argparse
import hashlib
import html
import json
import os
from pathlib import Path
from urllib.parse import quote

BASE=Path(__file__).resolve().parent


def need(condition,message):
    if not condition:raise ValueError(message)


def record(path):
    path=Path(path).resolve();digest=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024*1024),b''):digest.update(chunk)
    return {'path':str(path),'bytes':path.stat().st_size,'sha256':digest.hexdigest()}


def samepath(a,b):return os.path.normcase(str(Path(a).resolve()))==os.path.normcase(str(Path(b).resolve()))


def read_json(path):
    path=Path(path).resolve()
    need(path.stat().st_size<=16*1024*1024,f'oversize index/report input: {path.name}')
    data=path.read_bytes()
    return json.loads(data),{'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}


def verify(item):
    actual=record(item['path'])
    need(actual['bytes']==item['bytes'] and actual['sha256']==item['sha256'],f'indexed file differs from its prior seal: {item["path"]}')
    return actual


def prior_record(items,path):
    matches=[v for v in items if isinstance(v,dict) and samepath(v.get('path',''),path)]
    need(len(matches)==1,f'expected one existing seal for {path}')
    return matches[0]


def validate_entry(entry,out):
    need(isinstance(entry,dict),'each selected scene must be an object')
    need(entry.get('selection')=='selected_final','candidate/failed/unselected attempts cannot be indexed')
    need(all(isinstance(entry.get(k),str) and entry[k].strip() for k in ('scene_id','scene_name','distinct_scene_basis','run_dir','review_report','preview_manifest')),'entry fields are missing')
    root=Path(entry['run_dir']).resolve();review_path=Path(entry['review_report']).resolve();preview_path=Path(entry['preview_manifest']).resolve()
    for protected in (root,review_path.parent,preview_path.parent):
        need(not out.is_relative_to(protected) and not protected.is_relative_to(out),'index output overlaps an existing run/review/preview')
    manifest,manifest_record=read_json(root/'run-manifest.json')
    need(manifest.get('schema')=='dm-gta-run-manifest/1' and manifest.get('completed_at') and manifest.get('dry_run') is False
         and manifest.get('mode')=='real-staged-operator-gated' and manifest.get('delivery_eligible') is False,'run is not a finalized real internal scene')
    need(manifest['master'].get('mock') is False and manifest['master'].get('recording_start_verified') is True
         and manifest['master'].get('player_observation_count')==1800,'master does not identify a real complete recording')
    review,review_record=read_json(review_path)
    need(review.get('schema')=='dm-gta-mc10-scene-review/1' and review.get('integrity_status')=='PASS'
         and not review.get('errors') and samepath(review['run_dir'],root) and review['run_id']==manifest['run_id'],'run has no matching successful actual scene review')
    need(prior_record(review['inputs'],root/'run-manifest.json')==manifest_record,'run manifest changed after scene review')
    required_checks=('manifest_master_and_sources','raw_fence_time_master','metadata_roundtrip',
                     'sealed_reports','actual_video_decodes','64_actual_contact_frames','inputs_unchanged')
    need(all(review.get('checks',{}).get(k)=='PASS' for k in required_checks),'successful full scene review checks are absent')
    need(review.get('raw_pngs',{}).get('count')==7200 and review['raw_pngs'].get('all_hashes_checked') is True
         and review.get('checks',{}).get('actual_video_decodes')=='PASS' and len(review.get('contact_sheets',[]))==4
         and len(review.get('selected_frames',[]))==64,'actual full scene review is incomplete')
    need(set(review.get('videos',{}))=={'1','2','3','4'},'scene review lacks four real video probes')
    decode_reports=[]
    for lane in range(1,5):
        measured=review['videos'][str(lane)]
        need(measured.get('decoded_frames')==1800 and measured.get('width')==1920 and measured.get('height')==1080
             and measured.get('fps')=='30/1' and abs(measured['video_duration_s']-60)<=1/30,'scene review video measurements are incomplete')
        decode_reports.append(verify(measured['probe']))
    preview,preview_record=read_json(preview_path)
    need(preview.get('schema')=='gta-derived-review/2' and preview.get('status')=='completed' and samepath(preview['run_dir'],root)
         and preview.get('media_generated') is True and preview.get('frame_count')==1800 and preview.get('fps')=='30/1'
         and preview.get('replay_sha256')==manifest['master']['replay_sha256'],'actual matching preview completion receipt is absent')
    probe=preview.get('preview_probe_verified',{})
    need(probe.get('frame_count')==1800 and probe.get('width')==1920 and probe.get('height')==1080 and probe.get('fps')=='30/1'
         and abs(probe.get('duration_s',0)-60)<=1/30,'preview receipt has no exact actual decoded verification')
    dependencies=manifest['inputs']['dependency_manifest']
    need(preview.get('dependency_manifest')==dependencies,'preview dependency manifest differs from the finalized scene')
    need(len(manifest.get('metadata_files',[]))==8 and len(manifest.get('delivery_files',[]))==12,'finalized core file inventory is incomplete')
    metadata=[]
    for lane in range(1,5):
        for stem in ('action_camera','systeminfo'):
            path=root/f'capture/{stem}_{lane}.json';item=prior_record(manifest['metadata_files'],path)
            need(prior_record(manifest['delivery_files'],path)==item and prior_record(review['inputs'],path)==item,'metadata manifest/review seals differ')
            metadata.append(verify(item))
    config_inputs=[]
    for item in manifest['inputs'].values():
        need(prior_record(review['inputs'],item['path'])==item,'configuration differs from reviewed manifest')
        config_inputs.append(verify(item))
    videos=[]
    for lane in range(1,5):
        path=root/f'capture/video_{lane}.mp4';item=prior_record(manifest['videos'],path)
        need(prior_record(manifest['delivery_files'],path)==item,'video differs from finalized core file inventory')
        need(prior_record(review['inputs'],path)==item,'review/video manifest seals differ')
        need(preview['source_videos'][str(lane)]['file']==item,'preview derives from different lane video bytes')
        need(review['videos'][str(lane)]['source']==item,'decode report identifies different lane bytes')
        videos.append({'lane':lane,**verify(item)})
    need(set(preview.get('event_files',{}))=={'1','2','3','4'},'preview must seal all four event files')
    events=[]
    for lane,event_record in preview['event_files'].items():
        need(samepath(event_record['path'],root/f'evidence/frame-events-lane{lane}.jsonl')
             and prior_record(review['inputs'],event_record['path'])==event_record,'preview events differ from the reviewed lane')
        need(event_record['sha256']==manifest['alignment']['lane_event_sha256'][lane],'preview event seal differs from finalized scene')
        events.append(verify(event_record))
    preview_mp4=[v for v in preview['outputs'] if Path(v['path']).name=='four-camera-preview.mp4']
    need(len(preview_mp4)==1,'preview receipt must identify one actual four-camera MP4')
    preview_video=verify(preview_mp4[0])
    preview_probes=[v for v in preview['outputs'] if Path(v['path']).name=='preview-ffprobe.json']
    need(len(preview_probes)==1,'preview must retain its actual decoded probe report')
    preview_decode_report=verify(preview_probes[0])
    posters=[v for v in preview['outputs'] if Path(v['path']).name.startswith('four-view-frame-')]
    poster=verify(posters[0]) if posters else None
    contact_sheets=[verify(v) for v in review['contact_sheets']]
    qc_path=root/'evidence/engine-qc.json';qc,qc_record=read_json(qc_path)
    need(prior_record(review['inputs'],qc_path)==qc_record,'actual QC changed after review or was not reviewed')
    need(qc['verdict']==review['retained_qc']['verdict'] and qc['counts']==review['retained_qc']['counts'],'QC summary differs from actual report')
    gate_path=root/'evidence/gta-pr405-gate.json';gate,gate_record=read_json(gate_path)
    need(prior_record(review['inputs'],gate_path)==gate_record,'gate bytes differ from reviewed source')
    return {'scene_id':entry['scene_id'].strip(),'scene_name':entry['scene_name'].strip(),
            'distinct_scene_basis':entry['distinct_scene_basis'].strip(),'selection':'selected_final',
            'run_id':manifest['run_id'],'run_dir':str(root),'master_replay_sha256':manifest['master']['replay_sha256'],
            'run_manifest':manifest_record,'review_report':review_record,'preview_manifest':preview_record,
            'preview_video':preview_video,'preview_decode_report':preview_decode_report,
            'poster':poster,'videos':videos,'decode_reports':decode_reports,'metadata_files':metadata,
            'configuration_inputs':config_inputs,'event_files':events,'contact_sheets':contact_sheets,
            'integrity_status':'PASS','visual_review_status':review.get('visual_review_status','NOT_REVIEWED'),
            'human_review_approval':False,'formal_quality_approval':False,'delivery_eligible':False,'non_trainable':True,
            'qc':{'source':qc_record,'verdict':qc['verdict'],'counts':qc['counts'],
                  'non_pass_results':[r for r in qc['results'] if r.get('status')!='PASS']},
            'gate':{'source':gate_record,'ok':gate['ok'],'passed_rules':gate['passed_rules'],'missing_rules':gate['missing_rules']},
            'preflight':manifest['preflight']}


def link(path,out):
    try:value=quote(os.path.relpath(path,out).replace('\\','/'),safe='/:')
    except ValueError:value=Path(path).as_uri()
    return html.escape(value,quote=True)


def build_html(scenes,out):
    e=html.escape;cards=[]
    for scene in scenes:
        original=' · '.join(f'<a href="{link(v["path"],out)}">原片 {v["lane"]}</a>' for v in scene['videos'])
        poster=f' poster="{link(scene["poster"]["path"],out)}"' if scene['poster'] else ''
        failures=''.join(f'<li><b>{e(str(r.get("status")))} {e(str(r.get("rule")))}</b> · {e(str(r.get("target","")))}<br>{e(str(r.get("evidence","")))}</li>' for r in scene['qc']['non_pass_results'])
        counts=' / '.join(f'{e(str(k))}: {e(str(v))}' for k,v in scene['qc']['counts'].items())
        missing=', '.join(scene['gate']['missing_rules']) or '无缺项（仍不代表本索引授予批准）'
        retained=', '.join(scene['preflight'].get('retained_failures',[])) or '见原 preflight'
        cards.append(f'''<article id="{e(scene['scene_id'],quote=True)}"><h2>{e(scene['scene_id'])} · {e(scene['scene_name'])}</h2>
<p class="muted">{e(scene['distinct_scene_basis'])}</p><p><span>来源完整性 PASS</span> <span>正式 QC {e(scene['qc']['verdict'])}</span> <span>非训练 / 无交付批准</span></p>
<video controls preload="none"{poster} src="{link(scene['preview_video']['path'],out)}"></video>
<p>{original} · <a href="{link(scene['review_report']['path'],out)}">审查报告</a> · <a href="{link(scene['qc']['source']['path'],out)}">原 QC</a></p>
<details><summary>保留的 QC 与验收缺口 · {counts}</summary><p>Gate 缺项：{e(missing)}</p><p>Preflight 保留失败：{e(retained)}</p><ul>{failures}</ul></details></article>''')
    return '''<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>十场景 GTA5 四路 MC 内部演示</title><style>body{font-family:system-ui,sans-serif;background:#10151d;color:#edf2f7;margin:0 auto;max-width:1400px;padding:32px}h1{font-size:30px}header{margin-bottom:30px}main{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,580px),1fr));gap:24px}article{background:#1c2633;border:1px solid #344355;border-radius:12px;padding:20px}video{width:100%;aspect-ratio:16/9;background:#05080c}a{color:#84c8ff}.muted{color:#b6c2d0}span{display:inline-block;background:#303c4c;border-radius:5px;padding:5px 9px;font-size:12px;margin:0 5px 5px 0}details{font-size:13px;line-height:1.6}li{margin-bottom:10px}h2{font-size:21px;margin-top:0}</style>
<header><h1>十场景 · 四路 MC 内部演示</h1><p>10 个选定场景 · 40 路原片 · 每场 60 秒 / 1800 帧 · 10 个四视图预览</p>
<p class="muted">本页仅索引真实完成且已做来源审查的产物。人物可见性和正式质量不由本页自动批准；保留原 QC FAIL / SKIP、标定与权利缺口。非训练，不具备交付资格。视频点击后才加载。</p></header><main>'''+''.join(cards)+'</main></html>'


def build_index(input_path,output_path):
    out=Path(output_path).resolve()
    need(out!=BASE and out.is_relative_to(BASE) and not out.exists(),'out must be a new directory inside out/mc10-20260908; no overwrite')
    value,input_record=read_json(input_path)
    need(isinstance(value,dict),'selected-runs input must be an object')
    need(value.get('schema')=='dm-gta-mc10-selected-runs/1' and isinstance(value.get('scenes'),list) and len(value['scenes'])==10,'final batch index requires exactly ten explicitly selected scene entries')
    scenes=[validate_entry(entry,out) for entry in value['scenes']]
    for key in ('scene_id','run_id','run_dir','master_replay_sha256'):
        need(len({s[key].casefold() for s in scenes})==10,f'duplicate {key}: retries/reused masters do not count as new scenes')
    rendered_html=build_html(scenes,out)
    lines=['# 十场景四路 MC 内部演示索引','',
           '10 个明确选定的已完成场景，40 路原片；每场 60 秒、每路 1800 帧。人物目视与正式验收没有由索引工具自动批准，真实 QC 与缺证原样保留。','',
           f'[本地观看页](<{(out/"index.html").as_posix()}>)','',
           '| 场景 | 四视图预览 | 来源审查 | 正式 QC |','|---|---|---|---|']
    for scene in scenes:
        name=html.escape((scene['scene_id']+' · '+scene['scene_name']).replace('|',' / ').replace('\n',' ').replace('\r',' '))
        lines.append(f'| {name} | [60 秒预览](<{Path(scene["preview_video"]["path"]).as_posix()}>) | [完整报告](<{Path(scene["review_report"]["path"]).as_posix()}>) | {scene["qc"]["verdict"]}；{scene["qc"]["counts"]} |')
    lines+=['','完整原片、真实 QC 非 PASS 项、gate 缺项及文件 SHA-256 见 `registry.json`。本页引用现有本地文件，不复制视频；移动源目录后链接需要重新生成。']
    # Reject unserializable/non-finite report data before reserving any output.
    json.dumps(scenes,ensure_ascii=False,allow_nan=False)
    out.mkdir(parents=True,exist_ok=False)
    with (out/'index.html').open('x',encoding='utf-8') as f:f.write(rendered_html)
    with (out/'HANDOFF.md').open('x',encoding='utf-8') as f:f.write('\n'.join(lines)+'\n')
    registry={'schema':'dm-gta-mc10-viewing-registry/1','scene_count':10,'original_video_count':40,
        'frames_per_lane':1800,'duration_per_scene_s':60,'source_png_count_from_completed_reviews':72000,
        'selection_status':'ten_explicit_final_selections_with_existing_review_and_preview',
        'scene_distinctness_automatically_verified':False,'formal_quality_approval':False,'human_review_approval':False,
        'delivery_eligible':False,'non_trainable':True,'input':input_record,'generator':record(__file__),
        'verification_scope':'Checked indexed MP4, metadata, configurations, event files, saved decode reports, poster/contact-sheet bytes and report seals; no PNG fence revalidation, video redecoding, or new QC/approval.',
        'scenes':scenes,'outputs':[record(out/'index.html'),record(out/'HANDOFF.md')]}
    with (out/'registry.json').open('x',encoding='utf-8') as f:json.dump(registry,f,ensure_ascii=False,indent=2,allow_nan=False)
    return {'status':'INDEX_CREATED','scene_count':10,'html':str(out/'index.html'),'registry':record(out/'registry.json')}


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--input',required=True,type=Path);p.add_argument('--out',required=True,type=Path)
    args=p.parse_args()
    print(json.dumps(build_index(args.input,args.out),ensure_ascii=False))


if __name__=='__main__':
    try:main()
    except (ValueError,OSError,KeyError,TypeError) as exc:
        raise SystemExit(f'Index rejected: {exc}')

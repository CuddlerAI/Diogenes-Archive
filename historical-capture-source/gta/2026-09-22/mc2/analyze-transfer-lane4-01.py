"""Run only two real lane-4 image pairs with explicit reviewed static regions."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
OUT = ROOT/'review'/'story-editor-transfer-lane4-01'
EVENTS = ROOT/'story-editor-transfer-draft-01'/'run'/'evidence'/'frame-events-lane4.jsonl'
PLAN = ROOT/'story-editor-transfer-prepared-01'/'comparison-index-plan.json'


def ref(path):
    path=Path(path).resolve()
    return {'path':str(path),'bytes':path.stat().st_size,
            'sha256':hashlib.sha256(path.read_bytes()).hexdigest()}


def write(path,value):
    with path.open('x',encoding='utf-8',newline='\n') as stream:
        json.dump(value,stream,indent=2,ensure_ascii=False,allow_nan=False);stream.write('\n')


def polygon(label,points):
    return {'label':label,'polygon':points}


regions={
    1:[polygon('near court concrete and painted lines; outside pole shadows',
               [[400,575],[1730,562],[1880,1040],[250,1040]]),
       polygon('middle white facade between lamp and palm; repeated windows retained without spatial filtering',
               [[1246,141],[1324,141],[1324,308],[1246,308]]),
       polygon('rounded building opaque facade left of palm; excluding skyline and lower vegetation',
               [[231,119],[375,111],[375,212],[231,212]]),
       polygon('far high-rise opaque facade between vegetation; excludes skyline',
               [[654,28],[700,28],[700,85],[654,85]]),
       polygon('static concrete structure upper face, above people and rail',
               [[1570,326],[1750,319],[1750,363],[1570,363]])],
    2:[polygon('near court concrete and painted lines below people and vegetation',
               [[400,958],[1775,950],[1890,1060],[290,1060]]),
       polygon('middle white facade between lamp and palm; repeated windows retained without spatial filtering',
               [[1245,548],[1322,548],[1322,704],[1245,704]]),
       polygon('rounded building opaque facade left of palm; no skyline or lower vegetation',
               [[244,515],[391,515],[391,616],[244,616]]),
       polygon('far high-rise opaque facade between vegetation; no skyline',
               [[676,267],[714,267],[714,390],[676,390]]),
       polygon('static concrete structure upper face, above people and rail',
               [[1570,712],[1735,704],[1735,757],[1570,757]])],
}
categories=['actors_and_people','vegetation','sky_clouds','shadow_edges','moving_objects','hud_and_cursor']
rows=[json.loads(line) for line in EVENTS.read_text(encoding='utf-8').splitlines() if line.strip()]
plan=json.loads(PLAN.read_text(encoding='utf-8'))
assert len(rows)==150 and [r['output_frame'] for r in rows]==list(range(150))
assert all(r['lane']==4 and r['mock'] is False and r['render_succeeded'] is True and
           r['replay_sha256']==plan['source_replay_sha256'] for r in rows)
bindings=[]
for hold,frame in ((1,45),(2,120)):
    planned=next(r for r in plan['selected_stills'] if r['lane']==4 and r['hold']==hold)
    assert planned['planned_output_frame']==frame
    story=Path(planned['story_png']['path']); story_ref=ref(story)
    assert story_ref==planned['story_png']
    event=rows[frame]; editor=Path(event['output_path']); editor_ref=ref(editor)
    assert editor_ref['sha256']==event['output_sha256'] and editor_ref['bytes']==event['output_bytes']
    bindings.append({'hold':hold,'output_frame':frame,'story':story_ref,'editor':editor_ref,
                     'selected_original_event_row':event})
OUT.mkdir(parents=True,exist_ok=False)
helper=OUT/'measure-story-editor-static.py'
shutil.copyfile(ROOT/'measure-story-editor-static.py',helper)
write(OUT/'source-bindings.json',{'schema':'dm-gta-static-transfer-diagnostic-source/1',
      'frame_events':ref(EVENTS),'comparison_plan':ref(PLAN),'sealed_lane_event_count':len(rows),
      'selected_png_count':4,'selected_png_hashes_verified':True,'bindings':bindings,
      'formal_calibration_approval':False,'editor_equivalence_verified':False})
env=os.environ.copy();env['PYTHONPATH']=str(ROOT/'analysis-runtime')
for binding in bindings:
    hold=binding['hold']; path=OUT/f'hold{hold}-regions.json'
    selection={'schema':'dm-gta-story-editor-static-regions/1',
       'selection_timing':'Both original images visually inspected before running descriptor matching.',
       'selected_by':'agent visual diagnostic selection; not formal human approval'}
    for name in ('story','editor'):
        selection[name]={'sha256':binding[name]['sha256'],
            'selection_basis':'Positive static whitelist from this original image; everything outside is excluded. No native UV or matched displacement guided selection.',
            'excluded_categories':categories,'include':regions[hold],'exclude':[]}
    write(path,selection)
    command=[sys.executable,str(helper),'--story-png',binding['story']['path'],
             '--editor-png',binding['editor']['path'],'--regions',str(path),
             '--out-dir',str(OUT/f'hold{hold}-measurement')]
    result=subprocess.run(command,env=env,text=True,capture_output=True,encoding='utf-8')
    write(OUT/f'hold{hold}-execution.json',{'argv':command,'exit_code':result.returncode,
          'stdout':result.stdout,'stderr':result.stderr,'helper':ref(helper)})
    print(result.stdout,flush=True)
    if result.returncode:
        raise SystemExit(result.stderr)
assert all(ref(Path(b[name]['path']))==b[name] for b in bindings for name in ('story','editor'))
print(json.dumps({'output':str(OUT),'source_png_hashes_unchanged':True}))

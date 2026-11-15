"""Run only two real lane-3 image pairs with explicit reviewed static regions."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
OUT = ROOT/'review'/'story-editor-transfer-lane3-01'
EVENTS = ROOT/'story-editor-transfer-draft-01'/'run'/'evidence'/'frame-events-lane3.jsonl'
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
    1:[polygon('near court concrete and painted lines; above falling leaves and away from pole shadows',
               [[580,584],[1500,570],[1830,909],[350,932]]),
       polygon('right wall upper texture; above walkers and illumination boundary',
               [[1703,299],[1900,279],[1900,410],[1703,410]]),
       polygon('rounded building facade left of foreground palm; no skyline',
               [[450,243],[551,240],[551,316],[450,316]]),
       polygon('middle white facade between lamp and palm, above pedestrians',
               [[1160,260],[1217,260],[1217,378],[1160,378]])],
    2:[polygon('near court concrete and painted lines, below people and vegetation',
               [[520,875],[1580,868],[1825,1045],[340,1060]]),
       polygon('right wall upper texture; above people and illumination boundary',
               [[1704,589],[1900,569],[1900,705],[1704,705]]),
       polygon('rounded building facade left of foreground palm; no skyline',
               [[460,525],[556,523],[556,598],[460,598]]),
       polygon('middle white facade between lamp and palm, above people',
               [[1160,540],[1215,540],[1215,665],[1160,665]])],
}
categories=['actors_and_people','vegetation','sky_clouds','shadow_edges','moving_objects','hud_and_cursor']
rows=[json.loads(line) for line in EVENTS.read_text(encoding='utf-8').splitlines() if line.strip()]
plan=json.loads(PLAN.read_text(encoding='utf-8'))
assert len(rows)==150 and [r['output_frame'] for r in rows]==list(range(150))
assert all(r['lane']==3 and r['mock'] is False and r['render_succeeded'] is True and
           r['replay_sha256']==plan['source_replay_sha256'] for r in rows)
bindings=[]
for hold,frame in ((1,45),(2,120)):
    planned=next(r for r in plan['selected_stills'] if r['lane']==3 and r['hold']==hold)
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

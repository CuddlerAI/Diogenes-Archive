"""Run only two real lane-2 image pairs with explicit reviewed static regions."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
OUT = ROOT/'review'/'story-editor-transfer-lane2-01'
EVENTS = ROOT/'story-editor-transfer-draft-01'/'run'/'evidence'/'frame-events-lane2.jsonl'
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
    1:[polygon('near court markings and concrete; above airborne leaves at road edge',
               [[650,550],[1400,550],[1740,780],[420,800]]),
       polygon('right wall upper surface, above walkers and horizontal illumination edge',
               [[1475,393],[1900,330],[1900,455],[1475,455]]),
       polygon('distant rounded building facade left of foreground palm',
               [[615,337],[679,335],[679,386],[615,389]]),
       polygon('middle white facade between lamp and palm',
               [[1097,350],[1139,350],[1139,447],[1097,447]]),
       polygon('far rectangular high-rise opaque facade, excluding sky and boundary',
               [[165,301],[209,310],[209,380],[165,380]])],
    2:[polygon('near court markings and concrete; below actors and above airborne road debris',
               [[655,755],[1400,755],[1730,1000],[420,1020]]),
       polygon('right wall upper surface; excluding all walkers and illumination boundary',
               [[1473,591],[1896,528],[1896,635],[1473,635]]),
       polygon('distant rounded building facade left of foreground palm',
               [[620,530],[682,529],[682,577],[620,579]]),
       polygon('middle white facade between lamp and palm',
               [[1097,546],[1138,546],[1138,640],[1097,640]]),
       polygon('far rectangular high-rise opaque facade, excluding sky and boundary',
               [[180,493],[225,501],[225,574],[180,574]])],
}
categories=['actors_and_people','vegetation','sky_clouds','shadow_edges','moving_objects','hud_and_cursor']
rows=[json.loads(line) for line in EVENTS.read_text(encoding='utf-8').splitlines() if line.strip()]
plan=json.loads(PLAN.read_text(encoding='utf-8'))
assert len(rows)==150 and [r['output_frame'] for r in rows]==list(range(150))
assert all(r['lane']==2 and r['mock'] is False and r['render_succeeded'] is True and
           r['replay_sha256']==plan['source_replay_sha256'] for r in rows)
bindings=[]
for hold,frame in ((1,45),(2,120)):
    planned=next(r for r in plan['selected_stills'] if r['lane']==2 and r['hold']==hold)
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

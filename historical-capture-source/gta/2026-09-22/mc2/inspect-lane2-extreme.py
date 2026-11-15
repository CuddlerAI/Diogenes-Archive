"""Diagnostic-only crops of the largest retained lane2 raw displacement."""
from pathlib import Path
import hashlib
import json
from PIL import Image, ImageDraw

root=Path(__file__).resolve().parent/'review'/'story-editor-transfer-lane2-01'
report_path=root/'hold1-measurement'/'displacement.json'
report=json.loads(report_path.read_text(encoding='utf-8'))
match=max(report['matches'],key=lambda x:x['raw_distance_px'])
out=root/'hold1-extreme-inspection';out.mkdir(exist_ok=False)
box=[1020,617,1150,747]
sheet=Image.new('RGB',(1060,565),'white');draw=ImageDraw.Draw(sheet)
draw.text((10,8),'DIAGNOSTIC ONLY: same original XY crop, 4x NEAREST display; no recenter/warp',fill='black')
for i,name in enumerate(('story','editor')):
    path=Path(report['inputs'][name]['path']); data=path.read_bytes()
    assert hashlib.sha256(data).hexdigest()==report['inputs'][name]['sha256']
    with Image.open(path) as image:
        crop=image.crop(box);crop.save(out/(name+'-raw-crop.png'))
        sheet.paste(crop.resize((520,520),Image.Resampling.NEAREST),(10+i*530,40))
    draw.text((10+i*530,24),name+' '+str(match[name+'_px']),fill='black')
    assert hashlib.sha256(path.read_bytes()).hexdigest()==report['inputs'][name]['sha256']
sheet.save(out/'diagnostic-comparison-4x.png')
receipt={'schema':'dm-gta-static-match-extreme-crop/1','source_report':str(report_path),
         'inputs':report['inputs'],'retained_original_match':match,
         'identical_original_crop_xyxy':box,'display_scale':4,'display_resampling':'nearest',
         'measurement_recenter_or_warp':False,'original_inputs_unchanged':True,
         'formal_calibration_approval':False,'diagnostic_only':True}
with (out/'crop-receipt.json').open('x',encoding='utf-8') as stream:
    json.dump(receipt,stream,indent=2);stream.write('\n')
print(out)

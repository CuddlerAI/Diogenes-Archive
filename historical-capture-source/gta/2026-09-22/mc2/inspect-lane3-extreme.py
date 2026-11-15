"""Same-coordinate diagnostic crops, retaining the original 40px match."""
from pathlib import Path
import hashlib
import json
from PIL import Image,ImageDraw
root=Path(__file__).resolve().parent/'review'/'story-editor-transfer-lane3-01'
report_path=root/'hold1-measurement'/'displacement.json';report=json.loads(report_path.read_text(encoding='utf-8'))
match=max(report['matches'],key=lambda x:x['raw_distance_px'])
out=root/'hold1-extreme-inspection';out.mkdir(exist_ok=False)
box=[1120,230,1270,390];scale=4
sheet=Image.new('RGB',(1220,690),'white');draw=ImageDraw.Draw(sheet)
draw.text((10,6),'DIAGNOSTIC: identical original XY crop, 4x nearest display; no recenter/warp',fill='black')
draw.text((10,21),'RED: Story selected keypoint; BLUE: Editor selected keypoint. Both locations marked on both images.',fill='black')
for i,name in enumerate(('story','editor')):
    path=Path(report['inputs'][name]['path'])
    assert hashlib.sha256(path.read_bytes()).hexdigest()==report['inputs'][name]['sha256']
    with Image.open(path) as image:
        crop=image.crop(box);crop.save(out/(name+'-raw-crop.png'))
        panel=crop.resize((600,640),Image.Resampling.NEAREST)
    pd=ImageDraw.Draw(panel)
    for point,color in ((match['story_px'],'red'),(match['editor_px'],'blue')):
        x,y=(point[0]-box[0])*scale,(point[1]-box[1])*scale
        pd.ellipse((x-8,y-8,x+8,y+8),outline=color,width=2)
    sheet.paste(panel,(10+i*610,45));draw.text((10+i*610,34),name,fill='black')
    assert hashlib.sha256(path.read_bytes()).hexdigest()==report['inputs'][name]['sha256']
sheet.save(out/'diagnostic-comparison-4x.png')
with (out/'crop-receipt.json').open('x',encoding='utf-8') as stream:
    json.dump({'schema':'dm-gta-static-match-extreme-crop/1','source_report':str(report_path),
      'inputs':report['inputs'],'retained_original_match':match,'identical_original_crop_xyxy':box,
      'display_scale':4,'display_resampling':'nearest','display_annotations':'Both raw keypoint coordinates on each pane, without repositioning images',
      'measurement_recenter_or_warp':False,'original_inputs_unchanged':True,
      'formal_calibration_approval':False,'diagnostic_only':True},stream,indent=2);stream.write('\n')
print(out)

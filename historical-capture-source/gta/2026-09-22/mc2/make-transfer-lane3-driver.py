"""Prepare the reviewed lane3 image-pair driver; no measurement or pipe calls."""
from pathlib import Path
root=Path(__file__).resolve().parent
source=(root/'analyze-transfer-lane1-02.py').read_text(encoding='utf-8')
source=source.replace('lane-1','lane-3').replace('transfer-lane1-02','transfer-lane3-01')
source=source.replace('frame-events-lane1.jsonl','frame-events-lane3.jsonl')
source=source.replace("r['lane']==1", "r['lane']==3")
start=source.index('regions={\n');end=source.index('categories=',start)
regions="""regions={
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
"""
source=source[:start]+regions+source[end:]
target=root/'analyze-transfer-lane3-01.py';compile(source,str(target),'exec')
with target.open('x',encoding='utf-8',newline='\n') as stream:stream.write(source)
print(target)

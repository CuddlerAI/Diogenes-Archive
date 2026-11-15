"""Prepare lane2 driver from the already reviewed lane1 driver, without running SIFT."""
from pathlib import Path

root=Path(__file__).resolve().parent
source=(root/'analyze-transfer-lane1-02.py').read_text(encoding='utf-8')
source=source.replace('lane-1','lane-2').replace('transfer-lane1-02','transfer-lane2-01')
source=source.replace('frame-events-lane1.jsonl','frame-events-lane2.jsonl')
source=source.replace("r['lane']==1", "r['lane']==2")
start=source.index('regions={\n'); end=source.index('categories=',start)
regions="""regions={
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
"""
source=source[:start]+regions+source[end:]
target=root/'analyze-transfer-lane2-01.py'
compile(source,str(target),'exec')
with target.open('x',encoding='utf-8',newline='\n') as stream:
    stream.write(source)
print(target)

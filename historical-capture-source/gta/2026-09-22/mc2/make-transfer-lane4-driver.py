"""Prepare lane4 reviewed image-pair driver without running matching or pipe calls."""
from pathlib import Path
root=Path(__file__).resolve().parent
source=(root/'analyze-transfer-lane1-02.py').read_text(encoding='utf-8')
source=source.replace('lane-1','lane-4').replace('transfer-lane1-02','transfer-lane4-01')
source=source.replace('frame-events-lane1.jsonl','frame-events-lane4.jsonl')
source=source.replace("r['lane']==1", "r['lane']==4")
start=source.index('regions={\n');end=source.index('categories=',start)
regions="""regions={
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
"""
source=source[:start]+regions+source[end:]
target=root/'analyze-transfer-lane4-01.py';compile(source,str(target),'exec')
with target.open('x',encoding='utf-8',newline='\n') as stream:stream.write(source)
print(target)

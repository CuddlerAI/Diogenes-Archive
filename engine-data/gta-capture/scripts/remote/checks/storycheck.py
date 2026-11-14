"""storycheck.py <png>: 'story' once the screen is the game world (not the green-tinted loading screens), else
'loading'; with the green share and the mean brightness. A start menu with an advert can read as story: trust it
 only for a game that was already running."""
import sys
from PIL import Image
im=Image.open(sys.argv[1]).convert('RGB').resize((64,36)); px=list(im.get_flattened_data()) if hasattr(im,'get_flattened_data') else list(im.getdata())
g=sum(1 for p in px if p[1]>p[0]+25 and p[1]>p[2]+25)/len(px); m=sum(sum(p) for p in px)/len(px)/3
print('story' if g<0.2 and m>40 else 'loading', round(g,2), round(m))

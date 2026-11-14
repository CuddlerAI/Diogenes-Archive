"""greenphone.py <png>: the share (%) of green in the bottom-right corner where the protagonist's phone shows a call
(over 2 = ringing; under 1 can be a plant in the picture). simeon.sh polls it."""
import sys
from PIL import Image
import numpy as np
im=np.asarray(Image.open(sys.argv[1]).convert("RGB")).astype(int)
h,w,_=im.shape
r=im[int(h*0.42):int(h*0.98), int(w*0.78):int(w*0.985)]
g=(r[:,:,1]>r[:,:,0]+40)&(r[:,:,1]>r[:,:,2]+25)&(r[:,:,1]>90)
print(round(100*g.mean(),2))

"""shvdialog.py <png>: 'dialog' when the frame is Script Hook V's update question (a light grey/white box filling the
screen centre: the game is black or a picture), else 'no'."""
import sys
from PIL import Image
import numpy as np
im = np.asarray(Image.open(sys.argv[1]).convert("L")).astype(float)
h, w = im.shape
c = im[int(h * 0.15):int(h * 0.85), int(w * 0.1):int(w * 0.9)]
print("dialog" if (c > 200).mean() > 0.6 else "no")

"""screencheck.py items <bar.png> | lit <bar.png> | row <cliplist-small.png>: the start menu's item count, whether
Story Mode (rightmost) is the lit item, which clip row is selected."""
import sys
from PIL import Image
def col_max(im, x0, x1, y0, y1):
    return max(max(im.getpixel((x, y))[:3]) for x in range(x0, x1) for y in range(y0, y1))
kind, path = sys.argv[1], sys.argv[2]
im = Image.open(path).convert("RGB")
if kind == "items":    # x 660-695: the ad item's text with five items, the bar between 退出 and 设置 with four
    print(5 if col_max(im, 660, 695, 20, 60) > 130 else 4)
elif kind == "lit":    # lit text is white, the rest grey
    story = col_max(im, 960, 1070, 20, 60)
    others = max(col_max(im, x0, x0 + 100, 20, 60) for x0 in (600, 700, 800))
    print("lit" if story >= 235 and others < 225 else f"not-lit {story} {others}")
elif kind == "row":    # the clip list's middle column, rows 27 px apart from y 138 (1280x720)
    rows = [sum(sum(im.getpixel((x, 138 + 27 * k))[:3]) for x in range(520, 760, 8)) for k in range(16)]
    print(rows.index(max(rows)) + 1)

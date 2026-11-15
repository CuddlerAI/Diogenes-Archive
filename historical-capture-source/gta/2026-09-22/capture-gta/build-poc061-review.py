"""Create explicitly derived review media; never modify native frames or evidence."""
import argparse
import hashlib
import json
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont, ImageStat

parser = argparse.ArgumentParser()
parser.add_argument('--quad', action='store_true')
args = parser.parse_args()
root = Path(r'C:\capture\gta\runs\poc-061')
out = root / 'preview'
out.mkdir(exist_ok=True)
font = ImageFont.truetype(r'C:\Windows\Fonts\arial.ttf', 20)
small = ImageFont.truetype(r'C:\Windows\Fonts\arial.ttf', 15)
indices = list(range(0, 450, 30)) + [449]
all_rows = {}
report = {'schema': 'gta-derived-review/1', 'non_trainable': True,
          'formal_pr405_pass': False, 'human_review_approval': False,
          'method': '16 sampled times per lane; contact sheets are not a full-frame collision proof',
          'source_frames_unchanged': True, 'lanes': {}}
for lane in range(1, 5):
    rows = [json.loads(line) for line in
            (root / 'evidence' / f'frame-events-lane{lane}.jsonl').read_text().splitlines()]
    assert len(rows) == 450 and [r['output_frame'] for r in rows] == list(range(450))
    all_rows[lane] = rows
    sheet = Image.new('RGB', (1920, 1232), '#151515')
    d = ImageDraw.Draw(sheet)
    d.text((12, 8), f'POC061 CAMERA {lane} - INTERNAL / NON-TRAINABLE - sampled visual review only', font=font, fill='white')
    selected = []
    for tile, frame in enumerate(indices):
        row = rows[frame]
        path = Path(row['output_path'])
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        assert sha == row['output_sha256']
        with Image.open(path) as im:
            thumb = im.convert('RGB').resize((480, 270), Image.Resampling.LANCZOS)
        x, y = (tile % 4) * 480, 40 + (tile // 4) * 298
        sheet.paste(thumb, (x, y))
        d.text((x + 5, y + 273), f'{frame / 30:.3f}s  frame {frame}', font=small, fill='white')
        selected.append({'frame': frame, 'sha256': sha, 'source': str(path),
                         'mean_rgb': ImageStat.Stat(thumb).mean})
    sheet.save(out / f'camera-{lane}-contact.jpg', quality=95)
    report['lanes'][str(lane)] = {'frame_count': len(rows),
        'unique_native_frame_hashes': len({r['output_sha256'] for r in rows}),
        'selected_frames': selected}

for frame in indices:
    sheet = Image.new('RGB', (1920, 1152), '#151515')
    d = ImageDraw.Draw(sheet)
    d.text((12, 7), f'POC061  {frame / 30:.3f}s / frame {frame} - SAME REPLAY CLOCK - INTERNAL, NOT PR405 ACCEPTED', font=font, fill='white')
    for lane in range(1, 5):
        with Image.open(all_rows[lane][frame]['output_path']) as im:
            thumb = im.convert('RGB').resize((960, 540), Image.Resampling.LANCZOS)
        x, y = ((lane - 1) % 2) * 960, 36 + ((lane - 1) // 2) * 558
        sheet.paste(thumb, (x, y))
        d.text((x + 5, y + 540), f'CAMERA {lane}', font=small, fill='white')
    sheet.save(out / f'four-view-frame-{frame:06d}.jpg', quality=94)

if args.quad:
    manifest = json.loads((root / 'config' / 'dependency-manifest.json').read_text())
    ffmpeg = manifest['components']['ffmpeg']['path']
    command = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-n', '-filter_complex_threads', '2']
    for lane in range(1, 5):
        command += ['-i', str(root / 'capture' / f'video_{lane}.mp4')]
    filters = []
    for lane in range(4):
        filters.append(f'[{lane}:v]scale=960:540,pad=960:576:0:0:black,'
                       f"drawtext=fontfile='C\\:/Windows/Fonts/arial.ttf':text='CAM {lane+1} | INTERNAL NON-TRAINABLE':"
                       f'fontcolor=white:fontsize=20:x=12:y=548[c{lane}]')
    filters.append('[c0][c1][c2][c3]xstack=inputs=4:layout=0_0|960_0|0_576|960_576[v]')
    command += ['-filter_complex', ';'.join(filters), '-map', '[v]', '-map', '0:a:0',
                '-c:v', 'libx264', '-threads', '4', '-preset', 'fast', '-crf', '20',
                '-c:a', 'aac', '-b:a', '192k', '-t', '15', '-movflags', '+faststart',
                str(out / 'poc061-four-camera-preview.mp4')]
    subprocess.run(command, check=True)
report['outputs'] = [{'name': p.name, 'bytes': p.stat().st_size,
                      'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
                     for p in sorted(out.iterdir()) if p.is_file() and p.name != 'review-media-manifest.json']
(out / 'review-media-manifest.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
print(json.dumps({'preview_dir': str(out), 'files': len(report['outputs']),
                  'frame_counts': {k: v['frame_count'] for k, v in report['lanes'].items()}}))

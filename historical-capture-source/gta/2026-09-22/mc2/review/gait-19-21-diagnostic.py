"""Read-only, bounded native-PNG gait contact strips; no pixel reconstruction."""
import hashlib
import io
import json
from datetime import datetime, timezone
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

BASE = Path(__file__).resolve().parent.parent
RUN = BASE / 'run-mc2-beach-002-reexport-02'
OUT = BASE / 'review' / 'retry02-gait-19-21-diagnostic'
FRAMES = list(range(570, 630))
# Fixed image-coordinate crops only. No body/world coordinates or calibration.
CROPS = {1: (835, 700, 1065, 1060), 2: (835, 660, 1065, 1020)}

def sha(data):
    return hashlib.sha256(data).hexdigest()

def main():
    OUT.mkdir(exist_ok=False)
    font = ImageFont.truetype('C:/Windows/Fonts/arial.ttf', 17)
    title_font = ImageFont.truetype('C:/Windows/Fonts/arial.ttf', 22)
    records = []
    outputs = []
    events_before = {}
    for lane in (1, 2):
        event_path = RUN / 'evidence' / f'frame-events-lane{lane}.jsonl'
        raw_events = event_path.read_bytes()
        events_before[str(event_path)] = sha(raw_events)
        rows = [json.loads(line) for line in raw_events.splitlines() if line.strip()]
        crops = []
        for frame in FRAMES:
            row = rows[frame]
            assert row['output_frame'] == row['replay_frame'] == frame
            assert row['lane'] == lane and row['render_succeeded'] and not row['mock']
            path = Path(row['output_path'])
            raw = path.read_bytes()
            assert len(raw) == row['output_bytes'] and sha(raw) == row['output_sha256']
            with Image.open(io.BytesIO(raw)) as original:
                assert original.format == 'PNG' and original.size == (1920, 1080)
                cropped = original.convert('RGB').crop(CROPS[lane])
            crops.append(cropped)
            records.append({'lane': lane, 'frame': frame, 'replay_time_ms': row['replay_time_ms'],
                            'path': str(path), 'bytes': len(raw), 'sha256': sha(raw),
                            'crop_xyxy': CROPS[lane]})
        # Consecutive frames, left-to-right then top-to-bottom. Exact source
        # pixel copies inside each tile; labels occupy added margins only.
        w, h = crops[0].size
        for first in range(0, len(crops), 20):
            sheet = Image.new('RGB', (10 * w, 64 + 2 * (h + 28)), '#202020')
            draw = ImageDraw.Draw(sheet)
            draw.text((12, 9), f'DIAGNOSTIC CROP / NOT DELIVERY / CAM {lane} / consecutive frames / no resizing', font=title_font, fill='white')
            for offset, crop in enumerate(crops[first:first + 20]):
                x = (offset % 10) * w
                y = 64 + (offset // 10) * (h + 28)
                sheet.paste(crop, (x, y))
                frame = FRAMES[first + offset]
                draw.text((x + 6, y + h + 3), f'{frame} | {frame / 30:.3f}s', font=font, fill='white')
            target = OUT / f'lane{lane}-frames-{FRAMES[first]}-{FRAMES[first+19]}.png'
            sheet.save(target)
            outputs.append({'path': str(target), 'sha256': sha(target.read_bytes())})
    for event_path, digest in events_before.items():
        assert sha(Path(event_path).read_bytes()) == digest
    receipt = {'schema': 'dm-gta-bounded-pixel-diagnostic/1',
               'created_utc': datetime.now(timezone.utc).isoformat(),
               'frame_scope': {'lanes': [1, 2], 'start_inclusive': 570, 'end_exclusive': 630,
                               'seconds_start': 19, 'seconds_end_exclusive': 21},
               'method': 'SHA-verified native PNG fixed-coordinate lossless crop, consecutive contacts; added-margin labels only',
               'source_events_sha256_unchanged': events_before,
               'source_frame_count_sha_verified': len(records),
               'source_frames': records, 'derived_outputs': outputs,
               'human_review_approval': False, 'formal_sync_approval': False,
               'scope_limits': ['diagnostic crops only, not delivery media',
                                'no skeleton ground truth or camera-calibration inference',
                                'no source image or video modified']}
    target = OUT / 'diagnostic-receipt.json'
    target.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
    print(json.dumps({'output': str(OUT), 'sha_verified_native_frames': len(records), 'contact_sheets': len(outputs)}))

if __name__ == '__main__':
    main()

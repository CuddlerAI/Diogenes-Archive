"""All-frame decode/luma diagnostics, explicitly not a collision or rights verifier."""
import json
import subprocess
from pathlib import Path

root = Path(r'C:\capture\gta\runs\poc-061')
manifest = json.loads((root / 'config' / 'dependency-manifest.json').read_text())
ffmpeg = manifest['components']['ffmpeg']['path']
ffprobe = manifest['components']['ffprobe']['path']
report = {'schema': 'gta-video-visual-diagnostics/1', 'non_trainable': True,
          'formal_pr405_pass': False, 'collision_verified': False,
          'dynamic_visibility_verified': False,
          'method': 'Decode every delivered frame; 64x36 grayscale luma/MAD are diagnostic only',
          'lanes': {}}
for lane in range(1, 5):
    video = root / 'capture' / f'video_{lane}.mp4'
    probe = json.loads(subprocess.check_output([ffprobe, '-v', 'error', '-count_frames',
        '-show_streams', '-show_format', '-of', 'json', str(video)]))
    decoded = subprocess.run([ffmpeg, '-hide_banner', '-loglevel', 'error', '-xerror',
        '-threads', '2', '-i', str(video), '-map', '0:v:0', '-vf', 'scale=64:36,format=gray',
        '-fps_mode', 'passthrough', '-f', 'rawvideo', 'pipe:1'], capture_output=True, check=True).stdout
    n = 64 * 36
    assert len(decoded) == 450 * n, f'lane {lane}: unexpected decoded frame count'
    frames = [decoded[i:i+n] for i in range(0, len(decoded), n)]
    means = [sum(frame) / n for frame in frames]
    differences = [sum(abs(a-b) for a, b in zip(frames[i-1], frames[i])) / n
                   for i in range(1, len(frames))]
    low_motion_run = longest = 0
    for difference in differences:
        low_motion_run = low_motion_run + 1 if difference < 0.03 else 0
        longest = max(longest, low_motion_run)
    report['lanes'][str(lane)] = {
        'decoded_frames': len(frames), 'decode_errors': False,
        'black_frame_candidates': [i for i, frame in enumerate(frames)
                                   if means[i] < 5 and max(frame) < 12],
        'mean_luma_min': min(means), 'mean_luma_max': max(means),
        'successive_frame_MAD_min': min(differences),
        'successive_frame_MAD_max': max(differences),
        'longest_near_static_transition_run': longest,
        'probe': probe,
    }
target = root / 'evidence' / 'video-visual-diagnostics.json'
target.write_text(json.dumps(report, indent=2), encoding='utf-8')
print(json.dumps({'report': str(target), 'lanes': {lane: {k:v for k,v in value.items()
                  if k != 'probe'} for lane,value in report['lanes'].items()}}))

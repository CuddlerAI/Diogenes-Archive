"""Bind completed lane1 diagnostics to two explicitly distinguished AI observers."""
import hashlib
import json
import struct
from datetime import datetime, timezone
from pathlib import Path

BASE = Path(__file__).resolve().parent
RUN = BASE/'scene02-port/run-02-reexport-01'
REVIEW = RUN/'review'
OUT = REVIEW/'lane1-independent-ai-observations-01.json'
assert not OUT.exists()
SESSION = 'ebe0f3b1-56ec-48db-b414-42b1f9a44c4e'
TAPE_SHA = '3e9c8d63c19a6a95b8c51748798169288868fd3f1966d7f2028155bc62cbcf87'

def record(path):
    path = Path(path).resolve()
    before = path.stat()
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for data in iter(lambda: f.read(4*1024*1024), b''):
            digest.update(data)
    after = path.stat()
    assert (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns), path
    return dict(path=str(path), bytes=after.st_size, sha256=digest.hexdigest())

def read(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))

event_path = RUN/'evidence/frame-events-lane1.jsonl'
event = record(event_path)
data = event_path.read_bytes()
rows = [json.loads(x) for x in data.splitlines() if x.strip()]
assert len(rows) == 1800 and [x['output_frame'] for x in rows] == list(range(1800))
replay = rows[0]['replay_sha256']
assert all(x['lane'] == 1 and x['mock'] is False and x['render_succeeded'] is True and
           x['editor_binding']['session_id'] == SESSION and x['replay_sha256'] == replay and
           Path(x['output_path']).parent.name == 'render_0060' for x in rows)
tape = record(RUN/'config/camera-tape.json')
assert tape['sha256'] == TAPE_SHA
arm_path = RUN/'evidence/native-lane-1-arm.json'
collect_path = RUN/'evidence/native-lane-1-result.json'
arm = read(arm_path)
collect = read(collect_path) if collect_path.exists() else None
assert arm['ok'] and arm['lane']['editor_session_id'] == SESSION
if collect is not None:
    assert collect['ok'] and collect['lane']['frame_events_sha256'] == event['sha256'] and collect['lane']['frame_count'] == 1800

full_path = REVIEW/'lane1-cursor-full-01.json'
full = read(full_path)
assert full['status'] == 'NO_KNOWN_TEMPLATE_CANDIDATE_IN_CHECKED_FRAMES' and not full['errors']
assert len(full['files']) == 1
sf = full['files'][0]
assert sf['event_snapshot_sha256'] == event['sha256'] and sf['checked_frame_count'] == 1800 and not sf['candidate_frames']
assert [x['output_frame'] for x in sf['checked_frames']] == list(range(1800))
assert all(x['png_sha256_verified'] and x['output_sha256'] == rows[x['output_frame']]['output_sha256'] for x in sf['checked_frames'])
early_path = REVIEW/'lane1-early-cursor-01.json'
early = read(early_path)
assert not early['errors'] and early['status'] == 'NO_KNOWN_TEMPLATE_CANDIDATE_IN_CHECKED_FRAMES'
ef = early['files'][0]
assert [x['output_frame'] for x in ef['checked_frames']] == [0, 114, 115]
assert all(x['output_sha256'] == rows[x['output_frame']]['output_sha256'] for x in ef['checked_frames'])
# The early report read a growing .partial file. Its exact byte snapshot must
# remain a prefix of the now sealed final event file; do not pretend it was final.
assert hashlib.sha256(data[:ef['event_snapshot_bytes']]).hexdigest() == ef['event_snapshot_sha256']

cm_path = REVIEW/'lane1-contact-01/review-media-manifest.json'
cm = read(cm_path)
assert cm['event_files']['1'] == event and cm['replay_sha256'] == replay
sheet = next(x for x in cm['outputs'] if Path(x['path']).name == 'camera-1-contact.jpg')
assert record(sheet['path']) == sheet and len(cm['selected_frames']['1']) == 16
for s in cm['selected_frames']['1']:
    assert s['sha256'] == rows[s['frame']]['output_sha256'] and s['bytes'] == rows[s['frame']]['output_bytes']

pngs = {}
for frame in [115, 857, 858]:
    row = rows[frame]
    png = record(row['output_path'])
    assert png['sha256'] == row['output_sha256'] and png['bytes'] == row['output_bytes']
    with Path(png['path']).open('rb') as f:
        header = f.read(24)
    assert header[:8] == b'\x89PNG\r\n\x1a\n' and struct.unpack('>II', header[16:24]) == (1920, 1080)
    pngs[str(frame)] = dict(file=png, frame=frame, source_t_ns=row['source_t_ns'], dimensions=[1920, 1080])

report = dict(schema='dm-gta-limited-ai-visual-observations/1', created_at=datetime.now(timezone.utc).isoformat(),
    run_dir=str(RUN), lane=1, status='LIMITED_AI_SAMPLES_AND_KNOWN_CURSOR_SCAN_COMPLETE',
    replay_sha256=replay, editor_session_id=SESSION, event_file=event,
    camera_tape=tape, native_arm=record(arm_path),
    native_collect=record(collect_path) if collect is not None else None,
    native_collect_status='BOUND_COMPLETE' if collect is not None else 'NOT_PRESENT_AT_REPORT_TIME',
    observed_pngs=pngs,
    independent_agent_observations=dict(observer='/root/calibration_audit', observer_kind='AI agent',
        method='Actually viewed both original1920x1080PNG images with view_image(detail=original).',
        frames=[857, 858], observed_frame_count=2, observations=[
            dict(frame=frame, person_complete_with_head_hands_and_shoes_inside_frame=True,
                 walking_pose_observed=True, foot_clear_of_bottom_edge=True,
                 approximate_bottom_clearance_px=[180, 200], clearance_method='visual estimate only; no formal bbox measurement',
                 obvious_hud_or_cursor_observed=False, obvious_body_crop_observed=False,
                 scenery='warehouse/parking yard with painted ground markings') for frame in [857,858]]),
    root_observations=dict(observer='/root', observer_kind='root AI agent',
        source='Root message explicitly reports completed actual view_image of original115and858.',
        frames=[115,858], observed_frame_count=2,
        observations=[dict(frame=115, full_person_reported_visible=True, walking_pose_observed=True,
                           obvious_hud_or_cursor_observed=False),
                      dict(frame=858, full_person_reported_visible=True, walking_pose_observed=True,
                           obvious_hud_or_cursor_observed=False, approximate_bottom_clearance_px=[180,200],
                           clearance_method='root visual estimate only; no formal bbox measurement')]),
    combined_scope=dict(unique_frames=[115,857,858], unique_frame_count=3,
                        observation_instances=4, full_continuous_video_review=False),
    early_cursor_scan=dict(report=record(early_path), checked_frames=[0,114,115], candidates=0,
                           partial_snapshot_is_exact_prefix_of_sealed_events=True),
    full_cursor_scan=dict(report=record(full_path), checked_frame_count=1800, known_template_candidates=0,
                          errors=[], scope=full['scope'], template=full['template']),
    generated_contact=dict(manifest=record(cm_path), sheet=sheet, sample_indices=cm['sample_indices'],
                           contact_sheet_visually_reviewed_in_this_report=False),
    filesystem_followup=record(REVIEW/'lane1-filesystem-followup-01.json'),
    whole_image_cursor_free_verified=False, all_frames_subject_visibility_verified=False,
    human_review_approval=False, independent_human_review=False, formal_quality_gate_pass=False,
    calibration_approval=False, delivery_eligible=False, granted_rules=[], non_trainable=True,
    limitations=['Only three distinct original frames are covered by these explicitly attributed AI observations.',
                 'The180–200px foot clearance is a visual estimate for the named857/858samples, not measured bbox evidence or all-frame minimum.',
                 'The1800-frame cursor scan checks only a pinned fixed-ROI template; it does not prove full-image UI/cursor absence.',
                 'No other camera lane, complete scene, calibration, player origin or formal approval is established.',
                 'Original images, event files and configuration are not modified.'])
with OUT.open('x', encoding='utf-8', newline='\n') as f:
    json.dump(report, f, ensure_ascii=False, indent=2); f.write('\n')
print(json.dumps(dict(report=record(OUT), events=event, full_scan=record(full_path), contact=sheet,
                     independent_frames=[857,858], root_frames=[115,858]), ensure_ascii=False))

"""Seal the root agent's explicitly reported seven raw-PNG observations only."""
import hashlib
import json
import struct
from datetime import datetime, timezone
from pathlib import Path

BASE = Path(__file__).resolve().parent
RUN = BASE / 'scene02-port/run-02'
OUT = RUN / 'review/root-visual-observations-lane1-raw-01.json'
if OUT.exists():
    raise SystemExit('Refusing to replace a prior observation receipt')

def record(path):
    path = Path(path).resolve()
    before = path.stat()
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for data in iter(lambda: f.read(4 * 1024 * 1024), b''):
            digest.update(data)
    after = path.stat()
    assert (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns), path
    return dict(path=str(path), bytes=after.st_size, sha256=digest.hexdigest())

def verify(expected):
    actual = record(expected['path'])
    assert actual['sha256'] == expected['sha256'] and actual['bytes'] == expected['bytes'], actual
    return actual

event_path = RUN/'evidence/frame-events-lane1.jsonl'
event = record(event_path)
assert event['sha256'] == '2fb2f492ebea9bd8fef5d1b8ea0bd3158de65b341f508dd4f1afde8cadc9ef59'
rows = [json.loads(line) for line in event_path.read_text(encoding='utf-8').splitlines() if line.strip()]
assert len(rows) == 1800 and [x['output_frame'] for x in rows] == list(range(1800))
replay = rows[0]['replay_sha256']
session = 'dbac45ba-782c-4f73-acf3-97dc01e64503'
assert all(x['schema'] == 'dm-gta-frame/1' and x['lane'] == 1 and x['mock'] is False and
           x['render_succeeded'] is True and x['replay_sha256'] == replay and
           x['editor_binding']['session_id'] == session and
           Path(x['output_path']).parent.name == 'render_0058' for x in rows)

scan_path = RUN/'review/lane1-cursor-full-01.json'
scan = json.loads(scan_path.read_text(encoding='utf-8'))
assert scan['status'] == 'NO_KNOWN_TEMPLATE_CANDIDATE_IN_CHECKED_FRAMES' and not scan['errors']
assert len(scan['files']) == 1
sf = scan['files'][0]
assert sf['event_snapshot_sha256'] == event['sha256'] and sf['event_snapshot_bytes'] == event['bytes']
assert sf['checked_frame_count'] == 1800 and not sf['candidate_frames'] and not sf['errors']
assert [x['output_frame'] for x in sf['checked_frames']] == list(range(1800))
assert all(x['png_sha256_verified'] and x['output_sha256'] == rows[x['output_frame']]['output_sha256']
           for x in sf['checked_frames'])

observations = []
for frame in [115, 600, 820, 840, 858, 875, 900]:
    row = rows[frame]
    source = verify(dict(path=row['output_path'], bytes=row['output_bytes'], sha256=row['output_sha256']))
    with Path(source['path']).open('rb') as f:
        header = f.read(24)
    assert header[:8] == b'\x89PNG\r\n\x1a\n' and struct.unpack('>II', header[16:24]) == (1920, 1080)
    observation = dict(frame=frame, source_t_ns=row['source_t_ns'], source_png=source,
        native_dimensions=[1920, 1080],
        root_reported_observation='人物完整可见，未见明显 HUD 或指针；仓库堆场环境清晰，可见行走姿态。',
        report_writer_independently_viewed_image=False)
    if frame == 858:
        observation['root_reported_edge_note'] = dict(
            description='脚接近画面下边缘，根代理目测约 25 像素；人物仍完整。',
            approximate_bottom_margin_px=25, method='root visual estimate only',
            formal_bounding_box_measured=False, all_frame_minimum_margin_claimed=False)
    observations.append(observation)

contact_path = RUN/'review/lane1-contact-01/review-media-manifest.json'
contact = json.loads(contact_path.read_text(encoding='utf-8'))
assert contact['event_files']['1'] == event and contact['replay_sha256'] == replay
sheet = next(x for x in contact['outputs'] if Path(x['path']).name == 'camera-1-contact.jpg')
verify(sheet)
assert len(contact['selected_frames']['1']) == 16
for selected in contact['selected_frames']['1']:
    row = rows[selected['frame']]
    assert selected['sha256'] == row['output_sha256'] and selected['bytes'] == row['output_bytes']

report = dict(schema='dm-gta-root-limited-visual-observations/1', created_at=datetime.now(timezone.utc).isoformat(),
    run_dir=str(RUN), replay_sha256=replay, status='LIMITED_ROOT_OBSERVATIONS_RECORDED',
    observation_attribution=dict(observer='/root', observer_kind='root AI agent',
        reported_viewing_method='actual view_image of raw PNGs 115, 600, 820, 840, 858, 875 and 900',
        source='Root task instruction explicitly reports completed viewing; timestamps not supplied.',
        observed_at=None, report_writer='/root/calibration_audit', report_writer_independently_viewed_images=False),
    observation_scope=dict(lanes=[1], raw_pngs_observed=7, contact_frames_observed_by_this_report=0,
        full_continuous_video_review=False, total_distinct_reported_visual_frames=7),
    event_file=event, editor_session_id=session, observations=observations,
    generated_contact_sheet=dict(manifest=record(contact_path), sheet=sheet,
        sample_indices=contact['sample_indices'], included_as_already_viewed=False,
        note='Generated and sent to root for later viewing; this receipt does not claim the contact sheet has been viewed.'),
    known_cursor_scan=dict(report=record(scan_path), status=scan['status'], checked_frame_count=1800,
        known_template_candidate_count=0, scope=scan['scope'], template=scan['template'],
        whole_image_cursor_free_verified=False),
    report_writer_binding_checks=dict(real_completed1800_events_and_identity=True,
        seven_pngs_current_bytes_sha256=True, seven_pngs_dimensions=True,
        full_scanner_bound_to_same_events=True, all1800_scanner_hashes_match_event_seals=True,
        contact_sheet_and_manifest_bound=True),
    whole_image_cursor_free_verified=False, all_frames_subject_visibility_verified=False,
    human_review_approval=False, independent_human_review=False, formal_quality_gate_pass=False,
    calibration_approval=False, delivery_accepted=False, delivery_eligible=False, granted_rules=[], non_trainable=True,
    limits=['Only seven raw frames explicitly reported by root are visual observations in this receipt.',
        'The approximately25px bottom margin at frame858 is a visual estimate, not measured bbox evidence or a full-clip minimum.',
        'All1800-frame cursor scan covers only the pinned known template at its fixed ROI; moved/different/scaled cursors or other HUD may be missed.',
        'The report writer verified file bindings but did not independently view any image.',
        'No formal rule, calibration, delivery or human approval is granted; other lanes are outside scope.'])
with OUT.open('x', encoding='utf-8', newline='\n') as f:
    json.dump(report, f, ensure_ascii=False, indent=2); f.write('\n')
print(json.dumps(dict(report=record(OUT), contact_sheet=sheet,
    scan=record(scan_path), checked_frames=1800, known_template_candidates=0), ensure_ascii=False))

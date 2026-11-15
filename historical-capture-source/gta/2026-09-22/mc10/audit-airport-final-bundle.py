"""Read only the completed airport bundle; write a new local diagnostic.

Reuse the sealed 7200-PNG scan and real decode reports; never rerun or grant QC.
"""
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

BASE = Path(__file__).resolve().parent
RUN = BASE / 'scene01-airport/run-03'
OUT = BASE / 'review/scene01-airport-final-bundle-01'
if OUT.exists():
    raise SystemExit('Refusing existing output directory')

checks, failures, files, reused = [], [], {}, []

def record(path):
    path = Path(path).resolve()
    key = str(path).casefold()
    if key not in files:
        before = path.stat()
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for data in iter(lambda: stream.read(4 * 1024 * 1024), b''):
                digest.update(data)
        after = path.stat()
        if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            raise ValueError(f'Input changed during read: {path}')
        files[key] = dict(path=str(path), bytes=after.st_size, sha256=digest.hexdigest())
    return files[key]

def check(name, condition):
    checks.append(dict(id=name, status='PASS' if condition else 'FAIL'))
    if not condition:
        failures.append(name)

def verify(item, label):
    current = record(item['path'])
    check(label, current['sha256'] == item['sha256'] and
          ('bytes' not in item or current['bytes'] == item['bytes']))
    return current

def read(path):
    record(path)
    return json.loads(Path(path).read_text(encoding='utf-8'))

review_path = BASE / 'review/scene01-airport-01/scene-review.json'
preview_path = BASE / 'preview/scene01-airport-01/review-media-manifest.json'
manifest = read(RUN / 'run-manifest.json')
review = read(review_path)
preview = read(preview_path)
check('review_exact_report_supplied_by_root', record(review_path)['sha256'] ==
      'df798c9e01f4ba76fefe6b2bb16ff3d212c7034814e68932e3d53b13cafcdc80')
check('finalized_real_manifest', bool(manifest.get('completed_at')) and
      manifest.get('dry_run') is False and manifest.get('delivery_eligible') is False)
check('completed_preview', preview.get('status') == 'completed' and preview.get('media_generated') is True)
check('same_run', Path(review['run_dir']).resolve() == RUN and Path(preview['run_dir']).resolve() == RUN)
check('review_integrity', review['integrity_status'] == 'PASS' and not review['errors'] and
      all(v == 'PASS' for v in review['checks'].values()))
for item in review['inputs']:
    if Path(item['path']).suffix.casefold() == '.exe':
        reused.append(dict(file=item, reason='Prior actual review records executable provenance; no new tool execution or executable access is attempted.'))
    else:
        verify(item, 'review_input:' + Path(item['path']).name)
for item in [review['source'], review['contact_sheet_source'], *review['verification_sources']]:
    verify(item, 'review_source:' + Path(item['path']).name)
for item in [*manifest['inputs'].values(), *manifest['delivery_files']]:
    verify(item, 'manifest_file:' + Path(item['path']).name)
check('twelve_delivery_files', len(manifest['delivery_files']) == 12 and len(manifest['metadata_files']) == 8)
replay = manifest['master']['replay_sha256']
check('same_master', replay == preview['replay_sha256'] == review['alignment']['replay_sha256'])
verify(preview['dependency_manifest'], 'preview_dependency_manifest')
check('7200_original_scan_retained', review['raw_pngs']['count'] == 7200 and review['raw_pngs']['all_hashes_checked'] is True)
videos = {}
for lane in range(1, 5):
    key = str(lane)
    video = review['videos'][key]
    src = verify(video['source'], f'lane{lane}_video')
    verify(video['probe'], f'lane{lane}_actual_decode_report')
    verify(preview['source_videos'][key]['file'], f'lane{lane}_preview_source')
    check(f'lane{lane}_three_video_seals', src == manifest['videos'][lane - 1] == preview['source_videos'][key]['file'])
    probe = read(video['probe']['path'])
    vs = [x for x in probe['streams'] if x['codec_type'] == 'video']
    aus = [x for x in probe['streams'] if x['codec_type'] == 'audio']
    check(f'lane{lane}_actual_decode_dimensions_clock', len(vs) == 1 and
          int(vs[0]['nb_read_frames']) == 1800 and vs[0]['width'] == 1920 and vs[0]['height'] == 1080 and
          vs[0]['avg_frame_rate'] == '30/1' and abs(float(vs[0]['duration']) - 60) < .001 and
          abs(float(probe['format']['duration']) - 60) < .001)
    check(f'lane{lane}_actual_codec_audio', vs[0]['codec_name'] == 'h264' and vs[0]['pix_fmt'] == 'yuv420p' and
          len(aus) == 1 and aus[0]['codec_name'] == 'aac' and int(aus[0]['sample_rate']) == 48000 and aus[0]['channels'] == 2)
    events = verify(preview['event_files'][key], f'lane{lane}_events')
    check(f'lane{lane}_same_event_seals', events == review['raw_pngs']['index_sources'][key] and
          events['sha256'] == manifest['alignment']['lane_event_sha256'][key])
    native_result = read(RUN / f'evidence/native-lane-{lane}-result.json')
    check(f'lane{lane}_collect_bound', native_result['ok'] is True and native_result['lane']['frame_count'] == 1800 and
          native_result['lane']['frame_events_sha256'] == events['sha256'] and native_result['lane']['mock'] is False)
    videos[key] = dict(file=src, actual_probe=record(video['probe']['path']),
                       observed_decode_frames=1800, width=1920, height=1080, fps='30/1', duration_s=60,
                       raw_frame_pattern=native_result['lane']['frame_pattern'],
                       new_decode_performed=False)

for item in preview['outputs']:
    verify(item, 'preview_output:' + Path(item['path']).name)
quad = next(x for x in preview['outputs'] if Path(x['path']).name == 'four-camera-preview.mp4')
quad_probe_path = next(x['path'] for x in preview['outputs'] if Path(x['path']).name == 'preview-ffprobe.json')
quad_probe = read(quad_probe_path)
qv = next(x for x in quad_probe['streams'] if x['codec_type'] == 'video')
check('quad_actual_decode_report', int(qv['nb_read_frames']) == 1800 and qv['width'] == 1920 and
      qv['height'] == 1080 and qv['avg_frame_rate'] == '30/1' and abs(float(quad_probe['format']['duration']) - 60) < .001)
gate = read(RUN / 'evidence/gta-pr405-gate.json')
check('gate_retained_in_manifest', gate == manifest['gta_pr405_gate'])
check('no_approval_or_delivery', gate['ok'] is False and gate['delivery_eligible'] is False and
      preview['human_review_approval'] is False and review['formal_quality_gate'] is False)
visual_reports = []
for name in ['root-visual-observations-lanes12-01.json', 'root-visual-observations-lane3-01.json', 'root-visual-observations-lane4-01.json']:
    path = RUN / 'review' / name
    vr = read(path)
    check('visual_report_scope:' + name, vr['replay_sha256'] == replay and
          vr['human_review_approval'] is False and vr['formal_quality_gate_pass'] is False and
          vr['observation_attribution']['report_writer_independently_viewed_images'] is False)
    for lane in vr['lanes']:
        for k in ['contact_manifest', 'contact_sheet', 'event_file']:
            verify(lane[k], f'visual_lane{lane["lane"]}_{k}')
        verify(lane['known_cursor_scan']['report'], f'visual_lane{lane["lane"]}_cursor_report')
        scanner = read(lane['known_cursor_scan']['report']['path'])
        check(f'visual_lane{lane["lane"]}_known_cursor_result', not scanner['errors'] and
              scanner['known_cursor_candidate_in_checked_frames'] is False and
              lane['known_cursor_scan']['checked_frame_count'] == 1800)
        # The prior report already binds selected PNGs and scanner entries to all1800 events.
        check(f'visual_lane{lane["lane"]}_scope_not_promoted', vr['whole_image_cursor_free_verified'] is False)
    visual_reports.append(dict(file=record(path), scope=vr['observation_scope']))

receipt_path = BASE / 'replus-cursor-refresh-derived-02/capture-build-receipt-v3.json'
receipt = read(receipt_path)
verify(receipt['replus_build'], 'real_replus_build_receipt')
verify(receipt['source_provenance']['exact_patch'], 'exact_reviewed_replus_patch')
build = read(receipt['replus_build']['path'])
verify(build['artifact'], 'built_replus_candidate')
check('real_two_step_build', build['asi_built'] is True and len(build['invocations']) == 2 and
      all(x['returncode'] == 0 for x in build['invocations']))
for invocation in build['invocations']:
    verify(dict(path=invocation['log'], sha256=invocation['log_sha256']), 'real_build_log')
source_verification = read(BASE / 'replus-cursor-refresh-derived-02/source-verification.json')
check('retained_complete_source_verification', source_verification['status'] == 'PASS' and
      source_verification['evidence']['reviewed_replus_patch_sha256'] == receipt['source_provenance']['exact_patch']['sha256'])
runtime = read(RUN / 'review/cursor-runtime-audit-01.json')
verify(runtime['log_snapshot'], 'runtime_cursor_log_snapshot')
qc_path = RUN / 'evidence/engine-qc.json'
formal_qc = {'status': 'NOT_PRESENT', 'counts': None, 'failures': None, 'skips': None}
if qc_path.exists():
    q = read(qc_path)
    formal_qc = {'status': 'PRESENT_SEPARATE_FROM_ORIGINAL_REVIEW', 'file': record(qc_path), 'top_level_keys': list(q)}

result = dict(schema='dm-gta-airport-final-bundle-audit/1', created_at=datetime.now(timezone.utc).isoformat(),
    run_dir=str(RUN), scope='Read-only independent file-seal and saved-report audit; reused full raw scan and actual decoding, no new pixel scan or decode.',
    integrity_status='PASS' if not failures else 'FAIL', checks=checks, failures=failures,
    scene_review=record(review_path), preview_manifest=record(preview_path), run_manifest=record(RUN/'run-manifest.json'),
    prior_raw_scan=review['raw_pngs'], raw_pngs_rehashed_this_audit=0, new_decode_performed=False,
    videos=videos, quad=quad, alignment=review['alignment'],
    formal_qc_at_audit=formal_qc, formal_qc_in_original_scene_review=review['retained_qc'],
    retained_preflight=manifest['preflight'],
    retained_gate=dict(ok=gate['ok'], passed_rules=gate['passed_rules'], missing_rules=gate['missing_rules'],
                       internal_poc_usable=gate['internal_poc_usable'], delivery_eligible=gate['delivery_eligible']),
    visual_reports=visual_reports, source_repair_receipt=record(receipt_path),
    verified_file_count=len(files), verified_files=list(files.values()), reused_evidence=reused,
    formal_quality_gate=False, human_review_approval=False, calibration_approval=False,
    delivery_eligible=False, non_trainable=True, granted_rules=[],
    limitations=['Formal QC absence is a retained reporting gap, not a successful result.',
                'FOV-control K/pose consistency does not prove physical scale, filmback or approved projection calibration.',
                'Root visual observations are limited samples, not independent human approval or all-frame subject visibility.',
                'Known-cursor scans are fixed-ROI template checks, not full-image cursor absence.',
                'Only this scene is audited; no other candidate is counted as completed.'])
OUT.mkdir(parents=True, exist_ok=False)
with (OUT/'audit.json').open('x', encoding='utf-8', newline='\n') as f:
    json.dump(result, f, ensure_ascii=False, indent=2); f.write('\n')
print(json.dumps(dict(output=str(OUT/'audit.json'), integrity_status=result['integrity_status'],
    checks=len(checks), failures=failures, verified_files=len(files), formal_qc=formal_qc), ensure_ascii=False))
raise SystemExit(bool(failures))

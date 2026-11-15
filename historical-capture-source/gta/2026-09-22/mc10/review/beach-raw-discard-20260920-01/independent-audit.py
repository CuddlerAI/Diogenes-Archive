"""Read-only verification of the completed beach disposition; writes new audit only."""
import hashlib
import json
import os
from pathlib import Path
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
PLAN_DIR = BASE / 'review/completed-raw-retention-20260910-01'
STAGE = BASE / 'r2-staging-20260910-01/finished'
SCOPE = {f'render_{i:04}' for i in range(44, 48)}
PINS = {
    'retention-plan.json': 'a47b0ae412e54dac5e79a1c8456a90ff46c4d77a70ac9665663f5d0616aafefc',
    'discard-candidates.jsonl': 'bef30adc161234396cc019a01e46146b2668680cd7bb555d5206b68d7fee3b0f',
    'retained-files.jsonl': '808220b5f1fcf402550bb093f9510347d45019bc0fc5e5020692ae5d09ac388e',
    'protected-scope.json': 'feb35778951ddac6a09a90a4e334b6af97ff3e0495aa57ab7b0d8a294f81cac5',
}
report = {'schema': 'dm-gta-beach-disposition-independent-audit/1', 'started_at_utc': datetime.now(timezone.utc).isoformat(),
          'status': 'RUNNING', 'raw_scope': sorted(SCOPE), 'checks': [], 'input_files': [],
          'preserved_pngs': [], 'preserved_sidecars': [], 'protected_finished_members': [],
          'remote_check_performed': False, 'source_files_modified': False, 'deletion_performed_by_this_audit': False,
          'formal_delivery_eligible': False, 'complete_raw_remote_backup_verified': False,
          'exact_discarded_pixel_reconstruction_available': False,
          'scope_exclusion': 'No PNG access to render_0068 or later, airport, or Sandy. Existing completed media are checked.'}

def check(condition, name):
    if not condition:
        raise AssertionError(name)
    report['checks'].append({'name': name, 'status': 'PASS'})

def statrow(path):
    s = Path(path).stat(follow_symlinks=False)
    if getattr(s, 'st_file_attributes', 0) & 0x400:
        raise AssertionError(f'Reparse point: {path}')
    return {'bytes': s.st_size, 'mtime_ns': s.st_mtime_ns, 'ctime_ns': s.st_ctime_ns,
            'device': s.st_dev, 'file_id': s.st_ino, 'nlink': s.st_nlink}

def safe_path(path):
    p = Path(path).absolute()
    for ancestor in (p, *p.parents):
        if getattr(ancestor.stat(follow_symlinks=False), 'st_file_attributes', 0) & 0x400:
            raise AssertionError(f'Reparse path: {ancestor}')
    return p

def record(path, expected_sha=None, expected_bytes=None, expected_stat=None):
    p = safe_path(path)
    before = statrow(p)
    h = hashlib.sha256()
    with p.open('rb') as stream:
        for block in iter(lambda: stream.read(4 * 1024 * 1024), b''):
            h.update(block)
    after = statrow(p)
    assert before == after, f'Changed while hashing: {p}'
    if expected_stat is not None:
        assert before == {k: int(v) for k, v in expected_stat.items()}, f'Identity changed: {p}'
    if expected_sha is not None:
        assert h.hexdigest() == expected_sha, f'SHA mismatch: {p}'
    if expected_bytes is not None:
        assert before['bytes'] == expected_bytes, f'Bytes mismatch: {p}'
    return {'path': str(p), 'bytes': before['bytes'], 'sha256': h.hexdigest(),
            'stat': before, 'source_before_after_stable': True, 'sha256_method': 'fresh_read_this_audit'}

def read_json(path):
    return json.loads(Path(path).read_text(encoding='utf-8-sig'))

def read_rows(path):
    return [json.loads(line) for line in Path(path).read_text(encoding='utf-8-sig').splitlines() if line]

def norm(path):
    return os.path.normcase(os.path.abspath(path))

out = HERE / 'independent-release-audit.json'
assert not out.exists(), f'Will not overwrite {out}'
try:
    report['input_files'].append(record(__file__))
    report['input_files'].append(record(BASE / 'discard-completed-beach-raw-20260920.ps1', 'b88c1fade09fcc571aad1426c2b97543a991e81ea09d05b6b14a3e1498981c06'))
    for name, pin in PINS.items():
        report['input_files'].append(record(PLAN_DIR / name, pin))
    receipt = read_json(HERE / 'discard-receipt.json')
    report['input_files'].append(record(HERE / 'discard-receipt.json'))
    report['input_files'].append(record(HERE / 'discard-journal.jsonl'))
    check(receipt['plan_pins'] == PINS and receipt['status'] == 'completed_intentional_lossy_discard', 'completed_receipt_and_fixed_plan_pins')
    plan = read_json(PLAN_DIR / 'retention-plan.json')
    keeps = [r for r in read_rows(PLAN_DIR / 'retained-files.jsonl') if r['render'] in SCOPE]
    drops = [r for r in read_rows(PLAN_DIR / 'discard-candidates.jsonl') if r['render'] in SCOPE]
    all_rows = keeps + drops
    check(len(keeps) == 444 and len(drops) == 6756 and len({norm(r['original_path']) for r in all_rows}) == 7200, 'exact_7200_png_partition')
    check(sum(r['bytes'] for r in drops) == 19163025762 and sum(r['bytes'] for r in keeps) == 1296402367, 'planned_partition_bytes')
    for r in all_rows:
        p = Path(r['original_path'])
        assert norm(p.parent) == norm(Path('C:/capture/gta/replus-output') / r['render'])
        assert p.name == f"frame_{r['frame']:06}.png" and r['scene'] == 'beach'
    for render in SCOPE:
        check({r['frame'] for r in all_rows if r['render'] == render} == set(range(1800)), f'{render}_complete_original_index')
    for render in ('render_0044', 'render_0045'):
        check(set(range(570, 630)) <= {r['frame'] for r in keeps if r['render'] == render}, f'{render}_continuous_gait_window_retained')

    journal = read_rows(HERE / 'discard-journal.jsonl')
    check(len(journal) == len(drops) * 2, 'journal_exact_13512_records')
    expected = {norm(r['original_path']): r for r in drops}
    seen = set()
    for index in range(0, len(journal), 2):
        intent, deleted = journal[index:index + 2]
        assert intent['action'] == 'intent' and deleted['action'] == 'deleted'
        assert intent['file'] == deleted['file'] and intent['at_utc'] <= deleted['at_utc']
        assert intent.get('complete_remote_backup_verified') is False
        f = intent['file']; key = norm(f['source_path']); r = expected[key]
        assert key not in seen and not os.path.lexists(f['source_path'])
        assert f['bytes'] == r['bytes'] and f['sha256'] == r['sha256'] and f['frame'] == r['frame'] and f['render'] == r['render']
        assert {k: int(v) for k, v in f['identity'].items()} == r['current_stat']
        seen.add(key)
    check(seen == set(expected), 'journal_pairs_exactly_match_all_absent_candidates_with_identity')
    check(receipt['deleted_files'] == 6756 and receipt['deleted_logical_bytes'] == 19163025762 and receipt['pending_intent'] is None and receipt['error'] is None, 'receipt_journal_count_bytes_closed')

    sidecars = receipt['sidecars_pre_discard_sha256']
    check(len(sidecars) == 8, 'eight_preserved_sidecars')
    for render in sorted(SCOPE):
        directory = safe_path(Path('C:/capture/gta/replus-output') / render)
        wanted = {Path(r['original_path']).name for r in keeps + sidecars if r['render'] == render}
        actual = list(directory.iterdir())
        check(all(p.is_file() and not p.is_symlink() for p in actual) and {p.name for p in actual} == wanted, f'{render}_exact_remaining_leaf_membership')
    for r in keeps:
        rr = record(r['original_path'], r['sha256'], r['bytes'], r['current_stat'])
        rr.update(render=r['render'], frame=r['frame'], native_event=r['native_event'])
        report['preserved_pngs'].append(rr)
    for r in sidecars:
        report['preserved_sidecars'].append(record(r['original_path'], r['sha256'], r['bytes'], r['current_stat']))
    check(len(report['preserved_pngs']) == 444, 'all_444_retained_png_actual_sha_and_original_identity_match')
    check(len(report['preserved_sidecars']) == 8, 'all_8_sidecar_actual_sha_and_original_identity_match')
    report['renders'] = []
    for rp in plan['renders']:
        if rp['render'] not in SCOPE:
            continue
        er = rp['native_event']; report['input_files'].append(record(er['path'], er['sha256'], er['bytes']))
        events = read_rows(er['path']); assert len(events) == 1800
        for r in [r for r in all_rows if r['render'] == rp['render']]:
            e = events[r['frame']]
            assert e['output_frame'] == r['frame'] and norm(e['output_path']) == norm(r['original_path'])
            assert e['output_sha256'] == r['sha256'] and e['output_bytes'] == r['bytes']
            assert e['replay_sha256'] == r['native_event']['replay_sha256'] and e['editor_binding']['session_id'] == r['native_event']['editor_session_id']
        report['renders'].append({k: rp[k] for k in ('render', 'retained_png_count', 'retained_png_bytes', 'candidate_discard_count', 'candidate_discard_bytes', 'native_event')})
    check(len(report['renders']) == 4, 'four_original_sealed_events_unchanged_and_7200_rows_bound')

    for relative, pin in receipt['finished_evidence_pins'].items():
        report['input_files'].append(record(BASE / relative, pin))
    mapping = read_rows(STAGE / 'SOURCE-RESTORE-MAPPING.jsonl')
    manifest = read_rows(STAGE / 'UPLOAD-MANIFEST.jsonl')
    check(len(mapping) == len(manifest) == 215, 'finished_215_mapping_and_manifest_members')
    indexed = {r['object_relative_path']: r for r in manifest}
    check(len(indexed) == 215 and set(indexed) == {r['object_relative_path'] for r in mapping}, 'finished_mapping_exact_key_set')
    stage_expected = set(indexed) | {'UPLOAD-MANIFEST.jsonl', 'SOURCE-RESTORE-MAPPING.jsonl', 'README-INTERNAL-DEMO.md'}
    check({p.relative_to(STAGE).as_posix() for p in STAGE.rglob('*') if p.is_file()} == stage_expected, 'finished_stage_exact_218_members')
    for row in mapping:
        m = indexed[row['object_relative_path']]
        assert m['bytes'] == row['bytes'] and m['sha256'] == row['sha256'] and norm(m['path']) == norm(row['source_path'])
        source = safe_path(row['source_path']); dest = safe_path(row['staged_path'])
        assert norm(dest) == norm(STAGE / row['object_relative_path'])
        before_stage = statrow(dest)
        actual = record(source, row['sha256'], row['bytes'])
        st = actual['stat']; assert before_stage == statrow(dest)
        assert (st['device'], st['file_id']) == (before_stage['device'], before_stage['file_id'])
        assert st == before_stage and st['device'] == row['file_identity']['device'] and st['file_id'] == row['file_identity']['inode']
        assert st['mtime_ns'] == row['source_mtime_ns']
        actual.update(object_relative_path=row['object_relative_path'], staged_path=str(dest), stage_same_identity_verified=True)
        report['protected_finished_members'].append(actual)
    mp4s = [r for r in report['protected_finished_members'] if r['path'].lower().endswith('.mp4')]
    check(len(mp4s) == 15 and sum(r['bytes'] for r in mp4s) == 2091163359, 'all_15_actual_mp4_sha_preserved')
    check(sum(r['bytes'] for r in report['protected_finished_members']) == 2475614786, 'all_215_originals_fresh_sha_and_stage_hardlink_identity_match')
    by_source = {norm(r['path']): r for r in report['protected_finished_members']}
    for scene in ('beach', 'airport', 'sandy'):
        rm = read_json(STAGE / scene / 'run-manifest.json')
        qm = read_json(STAGE / scene / 'preview/review-media-manifest.json')
        assert rm['completed_at'] and rm['dry_run'] is False and rm['delivery_eligible'] is False
        assert len(rm['videos']) == 4 and len(rm['metadata_files']) == 8
        assert qm['status'] == 'completed' and qm['source_videos_unchanged'] is True
        for f in rm['videos'] + rm['metadata_files'] + [f for f in qm['outputs'] if f['path'].lower().endswith('.mp4')]:
            observed = by_source[norm(f['path'])]
            assert observed['sha256'] == f['sha256'] and observed['bytes'] == f['bytes']
        check(True, f'{scene}_sealed_run_and_quad_bindings_preserved')
    remote = read_json(BASE / 'review/r2-upload-20260910-01/finished-attempt02/upload-report.json')
    check(remote['status'] == 'verified' and remote['copy_exit_code'] == remote['check_exit_code'] == 0 and remote['destination_after'] == {'count':218, 'bytes':2476060486}, 'preserved_historical_finished_upload_verified')
    lines = (BASE / 'review/r2-upload-20260910-01/finished-attempt02/rclone-check.txt').read_text().splitlines()
    check(len(lines) == 218 and all(line.startswith('= ') for line in lines) and {line[2:] for line in lines} == stage_expected, 'historical_remote_check_exact_218_equal_lines')
    report['historical_r2_verification'] = {'destination': remote['destination'], 'finished_at': remote['finished_at'], 'reused_original_evidence_not_new_network_check': True}
    report['summary'] = {'deleted_png_absent': 6756, 'deleted_logical_bytes': 19163025762,
        'retained_png_sha_verified': 444, 'sidecar_sha_verified': 8, 'journal_intents':6756,
        'journal_deleted':6756, 'sealed_event_files_sha_verified':4, 'sealed_event_rows_bound':7200,
        'protected_finished_originals_fresh_sha_verified':215, 'protected_mp4_fresh_sha_verified':15,
        'finished_stage_members_verified':218}
    report['status'] = 'PASS_WITH_DECLARED_IRRECOVERABLE_RAW_LOSS'
except Exception as exc:
    report['status'] = 'FAIL'
    report['error'] = repr(exc)
finally:
    report['completed_at_utc'] = datetime.now(timezone.utc).isoformat()
    with out.open('x', encoding='utf-8', newline='\n') as f:
        json.dump(report, f, ensure_ascii=False, indent=2); f.write('\n')
    print(json.dumps({'status':report['status'], 'report':str(out), 'checks':len(report['checks']), 'summary':report.get('summary'), 'error':report.get('error')}, ensure_ascii=False))
if report['status'] == 'FAIL':
    raise SystemExit(1)

"""Freeze one completed internal scene for the existing R2 upload wrapper.

Default is read-only validation. --stage makes a NEW independent byte-copy archive.
No credentials, network, game calls, source edits, overwrite, or automatic approval.
"""
from __future__ import annotations
import argparse
import datetime as dt
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
REMOTE_ROOT = 'r2:diogenes-tempbucket/gta-mc-demos-20260910/'


def need(value, message):
    if not value:
        raise ValueError(message)


def stable_stat(path):
    s = Path(path).stat()
    return {'bytes': s.st_size, 'mtime_ns': s.st_mtime_ns, 'ctime_ns': s.st_ctime_ns,
            'device': s.st_dev, 'inode': s.st_ino, 'nlink': s.st_nlink}


def no_reparse(path):
    path = Path(os.path.abspath(path))
    for item in [path, *path.parents]:
        if item.exists():
            need(not item.is_symlink() and not (item.stat().st_file_attributes & 1024),
                 f'Reparse paths are not accepted: {item}')
    return path


def seal(path):
    path = no_reparse(path)
    need(path.is_file(), f'Required file absent: {path}')
    before = stable_stat(path)
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for data in iter(lambda: f.read(4 * 1024 * 1024), b''):
            digest.update(data)
    need(stable_stat(path) == before, f'Source changed during hashing: {path}')
    return {'path': str(path), 'bytes': before['bytes'], 'sha256': digest.hexdigest(),
            'source_stat': before}


def json_read(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))


def write_new(path, value):
    with Path(path).open('x', encoding='utf-8', newline='\n') as f:
        json.dump(value, f, ensure_ascii=False, indent=2, allow_nan=False)
        f.write('\n')


def safe_paths(run, review, preview, out, destination, *, base=BASE):
    base = Path(base).resolve()
    run, review, preview, out = map(no_reparse, (run, review, preview, out))
    need(all(p.is_relative_to(base) for p in (run, review, preview, out)),
         'All paths must stay inside this task batch')
    need(out.parent.parent == base and re.fullmatch(r'r2-staging-\d{8}-\d{2}', out.parent.name)
         and re.fullmatch(r'finished-[a-z0-9-]+', out.name), 'Use a new named R2 staging child')
    need(not out.exists(), 'Frozen staging already exists; never overwrite or resume staging')
    for protected in (run, review.parent, preview):
        need(not out.is_relative_to(protected) and not protected.is_relative_to(out),
             'Staging must not overlap input run, review or preview')
    suffix = destination.removeprefix(REMOTE_ROOT)
    need(destination.startswith(REMOTE_ROOT) and re.fullmatch(r'finished-\d{8}-[a-z0-9-]+', suffix),
         'Destination must be an explicit new finished-date-scene prefix under this task root')
    return run, review, preview, out


def existing_validator():
    path = BASE / 'build-batch-index.py'
    spec = importlib.util.spec_from_file_location('existing_scene_index_contract', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, seal(path)


def collect(args):
    run, review_path, preview, out = safe_paths(args.run_dir, args.review_report,
                                               args.preview_dir, args.out, args.destination)
    need(re.fullmatch(r'[a-z0-9]+(?:-[a-z0-9]+)*', args.scene_id), 'Invalid scene object prefix')
    # The existing validator checks finalized 4x1800/1080p30/60s videos, actual
    # full review, source seals, completed quad and actual QC. It does not grant QC.
    validator, validator_source = existing_validator()
    entry = {'selection': 'selected_final', 'scene_id': args.scene_id,
             'scene_name': args.scene_id, 'distinct_scene_basis': 'One explicitly chosen completed run; no batch-count inference',
             'run_dir': str(run), 'review_report': str(review_path),
             'preview_manifest': str(preview / 'review-media-manifest.json')}
    selected = validator.validate_entry(entry, out)
    manifest = json_read(run / 'run-manifest.json')
    review = json_read(review_path)
    preview_data = json_read(preview / 'review-media-manifest.json')
    need(review.get('human_review_approval') is False and review.get('delivery_eligible') is False
         and review.get('non_trainable') is True, 'Expected unapproved internal review boundary')
    selected_paths = {}

    def add(path, prefix, parent):
        path = no_reparse(path)
        need(path.is_relative_to(parent) and path.is_file(), f'Out-of-root or absent evidence: {path}')
        relative = (PurePosixPath(args.scene_id) / prefix / path.relative_to(parent).as_posix()).as_posix()
        need(relative not in selected_paths or selected_paths[relative] == path, 'Object collision')
        selected_paths[relative] = path

    def subtree(folder, suffixes):
        need(folder.is_dir(), f'Required source folder absent: {folder}')
        for path in sorted(folder.rglob('*')):
            no_reparse(path)
            if path.is_file() and path.suffix.lower() in suffixes:
                add(path, '', run)

    add(run / 'run-manifest.json', '', run)
    for row in manifest['delivery_files']:
        add(Path(row['path']), '', run)
    for name in ('plan.json', 'covis.json'):
        add(run / 'capture' / name, '', run)
    subtree(run / 'config', {'.json', '.jsonl'})
    add(run / 'config/camera-tape.bin', '', run)
    subtree(run / 'evidence', {'.json', '.jsonl'})
    subtree(run / 'source', {'.clip', '.wav', '.jsonl'})
    # Preserve actual finite visual observations/contact sheets, not whole RAW.
    if (run / 'review').exists():
        subtree(run / 'review', {'.json', '.jsonl', '.md', '.jpg', '.png'})
        for visual in (run / 'review').glob('root-visual-*'):
            if visual.is_dir():
                subtree(visual, {'.log'})
    for key in ('replay_path', 'player_telemetry_path'):
        add(Path(manifest['master'][key]), '', run)
    for key in ('audio_path', 'audio_report_path'):
        add(Path(manifest['audio'][key]), '', run)
    required_evidence = ['native-record-result.json', 'alignment-report.json', 'metadata-report.json',
                         'editor-replay-binding.json', 'engine-qc.json', 'gta-pr405-gate.json', 'preflight.json']
    required_evidence += [f'native-lane-{lane}-{phase}.json' for lane in range(1, 5) for phase in ('arm', 'result')]
    required_evidence += [f'frame-events-lane{lane}.jsonl' for lane in range(1, 5)]
    for name in required_evidence:
        add(run / 'evidence' / name, '', run)
    for path in sorted(review_path.parent.rglob('*')):
        if path.is_file() and path.suffix.lower() in {'.json', '.jsonl', '.jpg', '.md'}:
            add(path, 'review/' + review_path.parent.name, review_path.parent)
    add(preview / 'review-media-manifest.json', 'preview', preview)
    for row in preview_data['outputs']:
        add(Path(row['path']), 'preview', preview)
    # Match all applicable prior manifest/review/preview seals while inventorying.
    prior = {}
    def retain_seals(value):
        if isinstance(value, dict):
            if all(k in value for k in ('path', 'bytes', 'sha256')):
                key = os.path.normcase(str(Path(value['path']).absolute()))
                pin = (value['bytes'], value['sha256'])
                need(key not in prior or prior[key] == pin, f'Conflicting prior file seals: {key}')
                prior[key] = pin
            for child in value.values():
                retain_seals(child)
        elif isinstance(value, list):
            for child in value:
                retain_seals(child)
    for value in (manifest, review, preview_data):
        retain_seals(value)
    rows = []
    for relative, path in sorted(selected_paths.items()):
        actual = seal(path)
        pin = prior.get(os.path.normcase(str(path)))
        need(pin is None or (actual['bytes'], actual['sha256']) == pin, f'Prior seal differs: {path}')
        rows.append({'scene': args.scene_id, 'object_relative_path': relative, **actual,
                     'sha256_method': 'fresh_read_for_inventory', 'existing_sha_binding': pin,
                     'current_size_and_sha_verified': True})
    expected_videos = {f'{args.scene_id}/capture/video_{i}.mp4' for i in range(1, 5)}
    expected_videos.add(f'{args.scene_id}/preview/four-camera-preview.mp4')
    need({r['object_relative_path'] for r in rows if r['path'].lower().endswith('.mp4')} == expected_videos,
         'Exactly four lane videos and one quad are required')
    return out, rows, selected, validator_source


def freeze(out, rows, selected, destination, validator_source):
    out = no_reparse(out)
    need(not out.exists(), 'Frozen staging already exists')
    ancestor = out.parent
    while not ancestor.exists():
        ancestor = ancestor.parent
    payload_bytes = sum(row['bytes'] for row in rows)
    need(shutil.disk_usage(ancestor).free > payload_bytes + 256 * 1024 * 1024,
         'Insufficient space for independent frozen copies plus reserve')
    # Only allow safe object names before reserving output.
    for row in rows:
        relative = PurePosixPath(row['object_relative_path'])
        need(not relative.is_absolute() and '..' not in relative.parts and '\\' not in str(relative)
             and ':' not in str(relative), 'Unsafe object relative path')
    out.mkdir(parents=True, exist_ok=False)
    mappings = []
    for row in rows:
        source = no_reparse(row['path'])
        need(stable_stat(source) == row['source_stat'], f'Source stat changed before copy: {source}')
        target = out.joinpath(*PurePosixPath(row['object_relative_path']).parts)
        target.parent.mkdir(parents=True, exist_ok=True)
        with source.open('rb') as src, target.open('xb') as dst:
            shutil.copyfileobj(src, dst, 4 * 1024 * 1024)
        actual = seal(target)
        need((actual['bytes'], actual['sha256']) == (row['bytes'], row['sha256']), 'Copied bytes differ')
        need(stable_stat(source) == row['source_stat'] and seal(source)['sha256'] == row['sha256'],
             f'Source changed during copy: {source}')
        mappings.append({'object_relative_path': row['object_relative_path'], 'source_path': str(source),
                         'staged_path': str(target), 'bytes': row['bytes'], 'sha256': row['sha256'],
                         'method': 'independent_byte_copy', 'source_stat_before_after_stable': True,
                         'source_stat': row['source_stat']})
    for row in rows:
        need(stable_stat(row['path']) == row['source_stat'] and seal(row['path'])['sha256'] == row['sha256'],
             'Source changed before final freeze; incomplete output must not be uploaded')
    for name, values in [('UPLOAD-MANIFEST.jsonl', rows), ('SOURCE-RESTORE-MAPPING.jsonl', mappings)]:
        with (out / name).open('x', encoding='utf-8', newline='\n') as f:
            for value in values:
                f.write(json.dumps(value, ensure_ascii=False, allow_nan=False) + '\n')
    write_new(out / 'SCENE-REGISTRY.json', selected)
    with (out / 'README-INTERNAL-DEMO.md').open('x', encoding='utf-8', newline='\n') as f:
        f.write('One completed GTA internal scene: four 60-second independent videos and one quad.\n\n'
                'Existing full scene integrity review and actual decoding evidence were required. '
                'Actual QC FAIL/SKIP, gate missing rules, calibration and rights limitations remain in SCENE-REGISTRY.json '
                'and the original evidence. No human approval, delivery eligibility or training eligibility is granted.\n\n'
                'These are independent byte copies, frozen after source stability and SHA checks. '
                'No complete RAW PNG archive or complete external toolchain archive is included. '
                'Original absolute paths in evidence are historical provenance; restore mapping gives staged object locations.\n\n'
                'Upload is a separate ROOT action. A prepared archive is not proof of remote upload. '
                'Do not overwrite this directory; corrections require a new staging directory and remote prefix.\n')
    files = [seal(p) for p in sorted(out.rglob('*')) if p.is_file()]
    receipt = {'schema': 'dm-gta-single-scene-frozen-staging/1', 'status': 'prepared_not_uploaded',
               'created_at_utc': dt.datetime.now(dt.timezone.utc).isoformat(), 'source': str(out),
               'destination': destination, 'source_files': len(rows), 'source_bytes': payload_bytes,
               'objects_excluding_this_receipt': len(files), 'bytes_excluding_this_receipt': sum(v['bytes'] for v in files),
               'files': files, 'source_stable': True, 'generator': seal(__file__), 'validator': validator_source,
               'copied_video_count': sum(r['path'].lower().endswith('.mp4') for r in rows),
               'formal_quality_approval': False, 'human_review_approval': False,
               'delivery_eligible': False, 'non_trainable': True, 'complete_raw_archive': False,
               'upload_executed': False, 'old_frozen_staging_modified': False}
    write_new(out / 'STAGING-RECEIPT.json', receipt)
    all_files = list(out.rglob('*'))
    actual_files = [p for p in all_files if p.is_file()]
    return {'status': 'prepared_not_uploaded', 'source': str(out), 'destination': destination,
            'count': len(actual_files), 'bytes': sum(p.stat().st_size for p in actual_files),
            'receipt': seal(out / 'STAGING-RECEIPT.json'), 'source_stable': True}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--run-dir', type=Path, default=BASE / 'scene06-north-beach/run-02')
    p.add_argument('--review-report', type=Path, default=BASE / 'review/scene06-north-beach-02/scene-review.json')
    p.add_argument('--preview-dir', type=Path, default=BASE / 'preview/scene06-north-beach-02')
    p.add_argument('--scene-id', default='north-beach')
    p.add_argument('--out', type=Path, default=BASE / 'r2-staging-20260920-01/finished-north-beach')
    p.add_argument('--destination', default=REMOTE_ROOT + 'finished-20260920-north-beach')
    p.add_argument('--stage', action='store_true', help='After completed final review, create NEW frozen copies; no upload')
    args = p.parse_args()
    out, rows, selected, validator_source = collect(args)
    if args.stage:
        result = freeze(out, rows, selected, args.destination, validator_source)
    else:
        result = {'status': 'sources_verified_only_no_staging_or_upload', 'source_files': len(rows),
                  'source_bytes': sum(r['bytes'] for r in rows), 'planned_out': str(out),
                  'destination': args.destination, 'formal_qc': selected['qc'], 'gate': selected['gate']}
    print(json.dumps(result, ensure_ascii=False, allow_nan=False))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError) as exc:
        raise SystemExit(f'Staging refused; no upload performed: {exc}')

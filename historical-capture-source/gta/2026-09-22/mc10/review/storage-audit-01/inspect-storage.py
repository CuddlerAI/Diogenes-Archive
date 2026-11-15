"""Read-only task-scoped storage inventory. Writes only this report directory."""
from pathlib import Path
import ctypes
from ctypes import wintypes
import collections
import datetime
import json
import os

HERE = Path(__file__).resolve().parent
CAPTURE = HERE.parents[3]
OUT = CAPTURE / 'out'
assert CAPTURE.name == 'gta-capture'
EXCLUDE = {'source', 'config', 'evidence', 'capture', 'preview', 'review', 'samples',
           'analysis-runtime', 'logs', '.git', 'sdk', '__pycache__'}
INTERMEDIATE = {'.obj', '.pch', '.idb', '.ilk', '.tlog'}

def scan_files(path):
    result = []
    errors = []
    for base, dirs, files in os.walk(path, followlinks=False, onerror=lambda e: errors.append(str(e))):
        dirs[:] = [d for d in dirs if not (Path(base) / d).is_symlink()]
        for name in files:
            p = Path(base) / name
            try:
                if not p.is_symlink():
                    result.append({'path': str(p), 'bytes': p.stat().st_size})
            except OSError as e:
                errors.append(str(e))
    return result, errors

def find_candidates():
    builds, scratch, errors = [], [], []
    for stamp in ('mc2-20260908', 'mc10-20260908'):
        for base, dirs, files in os.walk(OUT / stamp, followlinks=False,
                                       onerror=lambda e: errors.append(str(e))):
            dirs[:] = [d for d in dirs if d not in EXCLUDE and not (Path(base)/d).is_symlink()]
            if 'CMakeCache.txt' in files:
                all_files, more_errors = scan_files(base)
                errors.extend(more_errors)
                subset = [f for f in all_files if Path(f['path']).suffix.lower() in INTERMEDIATE]
                builds.append({'path': str(Path(base)), 'total_bytes': sum(f['bytes'] for f in all_files),
                               'total_files': len(all_files),
                               'intermediate_bytes': sum(f['bytes'] for f in subset),
                               'intermediate_files': subset,
                               'classification': 'generated intermediates only; retain source, SDK, receipts, logs, binaries, CMake configuration and projects'})
                dirs[:] = []
            elif Path(base).name == 'media-scratch':
                all_files, more_errors = scan_files(base)
                errors.extend(more_errors)
                scratch.append({'path': str(Path(base)), 'bytes': sum(f['bytes'] for f in all_files),
                                'files': len(all_files),
                                'classification': 'do not reclaim unless no finalize in progress and failure-diagnostic retention reviewed'})
                dirs[:] = []
    return builds, scratch, errors

def volumes():
    k = ctypes.WinDLL('kernel32', use_last_error=True)
    first = k.FindFirstVolumeW
    first.argtypes = [wintypes.LPWSTR, wintypes.DWORD]
    first.restype = wintypes.HANDLE
    next_volume = k.FindNextVolumeW
    next_volume.argtypes = [wintypes.HANDLE, wintypes.LPWSTR, wintypes.DWORD]
    next_volume.restype = wintypes.BOOL
    close = k.FindVolumeClose
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    paths = k.GetVolumePathNamesForVolumeNameW
    paths.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
    paths.restype = wintypes.BOOL
    free_space = k.GetDiskFreeSpaceExW
    free_space.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ctypes.c_ulonglong), ctypes.POINTER(ctypes.c_ulonglong), ctypes.POINTER(ctypes.c_ulonglong)]
    free_space.restype = wintypes.BOOL
    rows, errors = [], []
    buf = ctypes.create_unicode_buffer(1024)
    handle = first(buf, len(buf))
    if handle == wintypes.HANDLE(-1).value:
        return rows, [{'stage': 'FindFirstVolumeW', 'winerror': ctypes.get_last_error()}]
    try:
        while True:
            name = buf.value
            pathbuf = ctypes.create_unicode_buffer(32768)
            required = wintypes.DWORD()
            ok = paths(name, pathbuf, len(pathbuf), ctypes.byref(required))
            mountpaths = []
            if ok:
                mountpaths = pathbuf[:required.value].rstrip('\0').split('\0') if required.value > 1 else []
            else:
                errors.append({'stage': 'GetVolumePathNamesForVolumeNameW', 'volume': name, 'winerror': ctypes.get_last_error()})
            available, total, free = (ctypes.c_ulonglong() for _ in range(3))
            okfree = free_space(name, ctypes.byref(available), ctypes.byref(total), ctypes.byref(free))
            rows.append({'volume': name, 'mount_paths': mountpaths,
                         'available_bytes': available.value if okfree else None,
                         'total_bytes': total.value if okfree else None,
                         'free_bytes': free.value if okfree else None,
                         'free_space_winerror': None if okfree else ctypes.get_last_error()})
            if not next_volume(handle, buf, len(buf)):
                if ctypes.get_last_error() != 18:
                    errors.append({'stage': 'FindNextVolumeW', 'winerror': ctypes.get_last_error()})
                break
    finally:
        close(handle)
    return rows, errors

def bound_intermediate_paths():
    """Read receipt-like small JSON records, excluding raw/media and source plans."""
    refs, read, errors = {}, 0, []
    def visit(value, doc):
        if isinstance(value, dict):
            p = value.get('path')
            if isinstance(p, str) and ('sha256' in value or 'bytes' in value):
                refs.setdefault(os.path.normcase(os.path.abspath(p)), []).append(str(doc))
            for item in value.values():
                visit(item, doc)
        elif isinstance(value, list):
            for item in value:
                visit(item, doc)
    for stamp in ('mc2-20260908', 'mc10-20260908'):
        for base, dirs, files in os.walk(OUT/stamp, followlinks=False):
            dirs[:] = [d for d in dirs if d not in {'source','samples','capture','analysis-runtime','.git','sdk'}
                       and not (Path(base)/d).is_symlink()]
            for name in files:
                if not name.endswith('.json') or not any(token in name for token in ('receipt','manifest','inventory')):
                    continue
                p = Path(base)/name
                try:
                    if p.stat().st_size <= 8_000_000:
                        visit(json.loads(p.read_text(encoding='utf-8')), p)
                        read += 1
                except (OSError, ValueError) as e:
                    errors.append({'path': str(p), 'error': str(e)})
    return refs, read, errors

if __name__ == '__main__':
    destination = HERE/'inventory.json'
    if destination.exists():
        raise SystemExit('Refusing to overwrite existing report')
    volume_rows, volume_errors = volumes()
    builds, scratch, walk_errors = find_candidates()
    refs, receipt_count, receipt_errors = bound_intermediate_paths()
    for build in builds:
        for item in build['intermediate_files']:
            item['direct_receipt_references'] = refs.get(os.path.normcase(item['path']), [])
        build['unreferenced_intermediate_bytes'] = sum(f['bytes'] for f in build['intermediate_files'] if not f['direct_receipt_references'])
    doc = {'schema': 'dm-gta-readonly-storage-audit/1',
           'observed_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
           'scope': [str(OUT/s) for s in ('mc2-20260908','mc10-20260908')],
           'writes': 'only newly created report directory; no deletion/move/repath',
           'raw_render_directories_walked': False, 'hashed_raw_frames': False,
           'volumes': volume_rows, 'volume_errors': volume_errors,
           'builds': builds, 'media_scratch': scratch,
           'intermediate_suffixes': sorted(INTERMEDIATE),
           'receipt_like_json_read': receipt_count,
           'unreferenced_build_intermediate_bytes': sum(b['unreferenced_intermediate_bytes'] for b in builds),
           'all_build_directory_bytes_not_reclaimable_whole': sum(b['total_bytes'] for b in builds),
           'walk_errors': walk_errors, 'receipt_read_errors': receipt_errors,
           'deletion_authorized_by_this_report': False,
           'scope_limit': 'Candidates are generated object/incremental-tracker files, not whole build directories. Receipt reference scan is bounded to task JSON. No C:/capture raw or user-directory inventory.'}
    destination.write_text(json.dumps(doc, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
    print(json.dumps({k:v for k,v in doc.items() if k not in ('builds',)}, ensure_ascii=False, indent=2))
    for b in sorted(builds, key=lambda b:b['unreferenced_intermediate_bytes'], reverse=True):
        print(json.dumps({k:v for k,v in b.items() if k != 'intermediate_files'}, ensure_ascii=False))

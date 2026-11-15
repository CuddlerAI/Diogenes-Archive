"""Diagnostic SIFT displacement on original-size Story and Editor PNGs.

No camera model, predicted UV, warp, homography, translation correction,
RANSAC, or displacement-based match filtering is used. Requires explicit
image-hash-bound static include polygons and dynamic exclusion declarations.
All pixels outside included regions are excluded from feature measurement.
OpenCV/numpy are imported only for measurement; --help needs only Python.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import sys

RATIO = 0.70
EXCLUDED_CATEGORIES = {
    'actors_and_people', 'vegetation', 'sky_clouds', 'shadow_edges',
    'moving_objects', 'hud_and_cursor',
}


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def ref(path):
    path = Path(path).resolve()
    return {'path': str(path), 'bytes': path.stat().st_size, 'sha256': sha(path)}


def read_png(path, cv, np):
    with path.open('rb') as stream:
        if stream.read(8) != b'\x89PNG\r\n\x1a\n':
            raise ValueError('input must be a real PNG: ' + str(path))
    image = cv.imdecode(np.fromfile(str(path), dtype=np.uint8), cv.IMREAD_COLOR)
    if image is None:
        raise ValueError('PNG decoder failed')
    return image


def static_mask(spec, shape, cv, np):
    height, width = shape[:2]
    if set(spec.get('excluded_categories', [])) != EXCLUDED_CATEGORIES:
        raise ValueError('each image must explicitly exclude all dynamic/overlay categories')
    includes = spec.get('include', [])
    if not includes or not spec.get('selection_basis'):
        raise ValueError('each image requires visually selected static polygons and a reason')
    mask = np.zeros((height, width), dtype=np.uint8)
    for regions, value in ((includes, 255), (spec.get('exclude', []), 0)):
        for region in regions:
            polygon = region.get('polygon')
            if not region.get('label') or not isinstance(polygon, list) or len(polygon) < 3:
                raise ValueError('each polygon requires >=3 original-pixel vertices and a label')
            if any(not isinstance(p, list) or len(p) != 2 or
                   any(not isinstance(v, (int, float)) or isinstance(v, bool) or
                       not math.isfinite(v) for v in p) or
                   not (0 <= p[0] < width and 0 <= p[1] < height) for p in polygon):
                raise ValueError('polygon vertex is outside the unscaled image')
            cv.fillPoly(mask, [np.rint(polygon).astype(np.int32)], value)
    # Do not permit descriptor support to reach beyond the image border.
    mask[0, :] = mask[-1, :] = mask[:, 0] = mask[:, -1] = 0
    if not np.any(mask):
        raise ValueError('the static mask is empty')
    return mask


def features(image, mask, cv, np):
    sift = cv.SIFT_create(nfeatures=0, nOctaveLayers=3,
                          contrastThreshold=0.04, edgeThreshold=10, sigma=1.6)
    keypoints, descriptors = sift.detectAndCompute(cv.cvtColor(image, cv.COLOR_BGR2GRAY), mask)
    if descriptors is None:
        return [], None, len(keypoints)
    clearance = cv.distanceTransform(mask, cv.DIST_L2, cv.DIST_MASK_PRECISE)
    # Conservative support margin: a descriptor must stay inside static pixels,
    # not merely have its center inside a static polygon.
    selected = [i for i, k in enumerate(keypoints)
                if clearance[int(round(k.pt[1])), int(round(k.pt[0]))] >= 6 * k.size + 2]
    return ([keypoints[i] for i in selected], descriptors[selected] if selected else None,
            len(keypoints))


def ratio_matches(first, second, cv):
    if first is None or second is None or len(first) < 2 or len(second) < 2:
        return {}
    output = {}
    for pair in cv.BFMatcher(cv.NORM_L2).knnMatch(first, second, k=2):
        if len(pair) == 2 and pair[1].distance > 0 and pair[0].distance < RATIO * pair[1].distance:
            a, b = pair
            output[a.queryIdx] = (a.trainIdx, float(a.distance), float(a.distance / b.distance))
    return output


def coverage(points, width, height, cv, np):
    if not points:
        return {'unique_half_pixel_locations': 0, 'bbox': None, 'convex_hull_image_fraction': 0,
                'occupied_3x3_cells': []}
    array = np.asarray(points, dtype=np.float32)
    return {'unique_half_pixel_locations': len({(round(x * 2), round(y * 2)) for x, y in points}),
            'bbox': [*array.min(axis=0).tolist(), *array.max(axis=0).tolist()],
            'convex_hull_image_fraction': float(cv.contourArea(cv.convexHull(array))) / (width * height),
            'occupied_3x3_cells': sorted({f'{min(2,int(y*3/height))},{min(2,int(x*3/width))}'
                                        for x, y in points})}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--story-png', required=True, type=Path)
    parser.add_argument('--editor-png', required=True, type=Path)
    parser.add_argument('--regions', required=True, type=Path,
                        help='dm-gta-story-editor-static-regions/1 JSON, image-bound by sha256')
    parser.add_argument('--out-dir', required=True, type=Path, help='new diagnostic directory only')
    args = parser.parse_args()
    import cv2 as cv
    import numpy as np
    cv.setNumThreads(1)
    paths = {'story': args.story_png.resolve(), 'editor': args.editor_png.resolve()}
    inputs = {name: ref(path) for name, path in paths.items()}
    regions_ref = ref(args.regions)
    regions = json.loads(args.regions.read_text(encoding='utf-8'))
    if regions.get('schema') != 'dm-gta-story-editor-static-regions/1':
        raise ValueError('static regions schema mismatch')
    for name in paths:
        if regions[name].get('sha256') != inputs[name]['sha256']:
            raise ValueError('static regions were selected on different image bytes: ' + name)
    images = {name: read_png(path, cv, np) for name, path in paths.items()}
    if images['story'].shape != images['editor'].shape:
        raise ValueError('original images must have identical dimensions; resizing is forbidden')
    height, width = images['story'].shape[:2]
    masks = {name: static_mask(regions[name], image.shape, cv, np) for name, image in images.items()}
    extracted = {name: features(image, masks[name], cv, np) for name, image in images.items()}
    story_k, story_d, _ = extracted['story']; editor_k, editor_d, _ = extracted['editor']
    forward = ratio_matches(story_d, editor_d, cv)
    reverse = ratio_matches(editor_d, story_d, cv)
    matches = []
    for source, (target, distance, ratio) in sorted(forward.items()):
        if target not in reverse or reverse[target][0] != source:
            continue
        a, b = story_k[source], editor_k[target]
        dx, dy = b.pt[0] - a.pt[0], b.pt[1] - a.pt[1]
        matches.append({'story_px': list(a.pt), 'editor_px': list(b.pt),
                        'raw_displacement_px': [dx, dy], 'raw_distance_px': math.hypot(dx, dy),
                        'descriptor_l2': distance, 'forward_ratio': ratio,
                        'reverse_ratio': reverse[target][2],
                        'story_keypoint_size_px': a.size, 'editor_keypoint_size_px': b.size})
    distances = np.asarray([m['raw_distance_px'] for m in matches])
    vectors = np.asarray([m['raw_displacement_px'] for m in matches])
    statistics = None if not matches else {
        'raw_distance_p50_px': float(np.percentile(distances, 50)),
        'raw_distance_p95_px': float(np.percentile(distances, 95)),
        'raw_distance_max_px': float(distances.max()),
        'raw_distance_rms_px': float(np.sqrt(np.mean(distances ** 2))),
        'median_raw_displacement_px': np.median(vectors, axis=0).tolist()}
    report = {'schema': 'dm-gta-story-editor-static-displacement/1',
              'status': 'diagnostic_matches' if matches else 'no_usable_mutual_matches',
              'inputs': inputs, 'regions': regions_ref, 'helper': ref(__file__),
              'opencv_version': cv.__version__, 'opencv_module': ref(cv.__file__),
              'numpy_version': np.__version__, 'image_dimensions': [width, height],
              'method': {'feature': 'SIFT original-size grayscale', 'ratio_threshold_both_directions': RATIO,
                         'reciprocal_nearest_match_required': True, 'descriptor_static_margin': '6 * keypoint.size + 2 pixels',
                         'spatial_match_filter': None, 'warp_or_homography': None,
                         'ransac': False, 'translation_applied': False, 'resizing_applied': False},
              'features': {name: {'detected_inside_mask': data[2], 'static_support_retained': len(data[0]),
                                   'mask_image_fraction': float(np.count_nonzero(masks[name]))/(width*height)}
                           for name, data in extracted.items()},
              'match_count': len(matches), 'statistics_all_mutual_matches': statistics,
              'coverage': {name: coverage([m[name+'_px'] for m in matches], width, height, cv, np)
                           for name in paths}, 'matches': matches,
              'limitations': ['Static polygons are explicit visual selections, not semantic ground truth.',
                              'Descriptor mismatches remain possible; all retained matches require review.',
                              'No declared metric scene depth; apparent depth diversity is visual only.',
                              'Story-to-Editor provenance/event and camera binding must be checked separately.'],
              'formal_calibration_approval': False, 'editor_equivalence_verified': False}
    if ref(args.regions) != regions_ref or any(ref(path) != inputs[name] for name, path in paths.items()):
        raise ValueError('source image or static selection changed during measurement')
    args.out_dir.mkdir(parents=True, exist_ok=False)
    with (args.out_dir/'displacement.json').open('x', encoding='utf-8', newline='\n') as stream:
        json.dump(report, stream, ensure_ascii=False, indent=2, allow_nan=False); stream.write('\n')
    for name, mask in masks.items():
        ok, data = cv.imencode('.png', mask)
        if not ok:
            raise ValueError('failed to encode diagnostic static mask')
        with (args.out_dir/(name+'-static-mask.png')).open('xb') as stream:
            stream.write(data.tobytes())
    print(json.dumps({'output': str(args.out_dir.resolve()), 'matches': len(matches),
                      'statistics_all_mutual_matches': statistics, 'formal_calibration_approval': False}))


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        print(type(exc).__name__ + ': ' + str(exc), file=sys.stderr)
        raise SystemExit(2)

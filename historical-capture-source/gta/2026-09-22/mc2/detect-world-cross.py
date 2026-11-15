"""Independent whole-PNG magenta world_cross detector. No UV/world inputs.

Only source pixels determine candidates. This diagnostic does not grant any
calibration approval. Coordinates use pixel centers (first center = 0.5,0.5).
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter


PARAMETERS = {
    'loose_min_red_blue': 60,
    'loose_min_magenta_excess': 30,
    'loose_min_red_blue_balance': 0.35,
    'strict_min_red_blue': 100,
    'strict_min_magenta_excess': 60,
    'strict_min_red_blue_balance': 0.45,
    'grouping_dilation_radius_px': 1,
    'minimum_original_pixels': 12,
    'minimum_strict_pixels': 7,
    'minimum_span_px': 7,
    'maximum_span_px': 256,
    'fit_band_halfwidth_px': 2.0,
    'arm_minimum_pixels': 2,
    'arm_minimum_extent_px': 3.0,
    'minimum_cross_weight_fraction': 0.80,
    'maximum_line_rms_px': 1.25,
    'maximum_orthogonality_error_deg': 15,
    'maximum_threshold_center_shift_px': 1.5,
    'maximum_loose_color_pixels': 50000,
    'maximum_png_pixels': 16777216,
}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def connected_groups(mask):
    # Dilation is used only to GROUP adjacent colored samples. It never enters
    # the measured center, support, residuals, or any saved source image.
    expanded = np.asarray(Image.fromarray(mask.astype(np.uint8)*255).filter(
        ImageFilter.MaxFilter(3))) > 0
    h, w = mask.shape
    active = set(map(int, np.flatnonzero(expanded)))
    flat = mask.ravel()
    groups = []
    while active:
        first = active.pop()
        pending, originals = [first], []
        while pending:
            value = pending.pop()
            if flat[value]:
                originals.append(value)
            y, x = divmod(value, w)
            for dy in (-1, 0, 1):
                if not 0 <= y+dy < h:
                    continue
                for dx in (-1, 0, 1):
                    if not 0 <= x+dx < w or (dx == 0 and dy == 0):
                        continue
                    other = value + dy*w + dx
                    if other in active:
                        active.remove(other)
                        pending.append(other)
        if originals:
            indices = np.asarray(originals, dtype=np.int64)
            groups.append((indices % w, indices // w))
    return sorted(groups, key=lambda group: (-len(group[0]), int(group[1].min()), int(group[0].min())))


def weighted_line(independent, dependent, weights):
    total = weights.sum()
    a = float(np.sum(weights*independent)/total)
    b = float(np.sum(weights*dependent)/total)
    variance = float(np.sum(weights*(independent-a)**2))
    if variance <= 1e-8:
        raise ValueError('line has no measurable extent')
    slope = float(np.sum(weights*(independent-a)*(dependent-b))/variance)
    intercept = b-slope*a
    residual = dependent-(slope*independent+intercept)
    rms = float(np.sqrt(np.sum(weights*residual**2)/total))
    return slope, intercept, rms


def fit_cross(xs, ys, weights):
    # Peak column/row support initializes a geometric fit, not a predicted UV.
    xbins, ybins = xs.astype(int), ys.astype(int)
    x0 = float(np.argmax(np.bincount(xbins, weights=weights)))+0.5
    y0 = float(np.argmax(np.bincount(ybins, weights=weights)))+0.5
    band = PARAMETERS['fit_band_halfwidth_px']
    for _ in range(3):
        horizontal = (np.abs(ys-y0) <= band) & (np.abs(xs-x0) >= 2)
        vertical = (np.abs(xs-x0) <= band) & (np.abs(ys-y0) >= 2)
        if int(horizontal.sum()) < 4 or int(vertical.sum()) < 4:
            raise ValueError('insufficient separate horizontal/vertical line support')
        hs, hi, hr = weighted_line(xs[horizontal],ys[horizontal],weights[horizontal])
        vs, vi, vr = weighted_line(ys[vertical],xs[vertical],weights[vertical])
        denominator = 1-hs*vs
        if abs(denominator) < 0.1:
            raise ValueError('line intersection is unstable')
        x0 = (vs*hi+vi)/denominator
        y0 = hs*x0+hi
    hdist = np.abs(ys-(hs*xs+hi))/np.sqrt(1+hs*hs)
    vdist = np.abs(xs-(vs*ys+vi))/np.sqrt(1+vs*vs)
    fraction = float(weights[(hdist <= band) | (vdist <= band)].sum()/weights.sum())
    angle = float(np.degrees(np.arccos(np.clip(
        abs(hs+vs)/np.sqrt((1+hs*hs)*(1+vs*vs)), 0, 1))))
    arms = {}
    for name, along, near in (
        ('left',x0-xs,hdist <= band),('right',xs-x0,hdist <= band),
        ('up',y0-ys,vdist <= band),('down',ys-y0,vdist <= band)):
        chosen = near & (along >= 2)
        arms[name] = {'pixels':int(chosen.sum()),
                      'extent_px':float(along[chosen].max()) if chosen.any() else 0.0}
    return {'center_px':[float(x0),float(y0)],
            'horizontal_line_y_eq_ax_plus_b':[hs,hi],
            'vertical_line_x_eq_ay_plus_b':[vs,vi],
            'horizontal_rms_px':hr,'vertical_rms_px':vr,
            'intersection_angle_deg':angle,'cross_weight_fraction':fraction,
            'arms':arms}


def evaluate(xs, ys, weight, strict, shape, number):
    w, h = shape
    x, y = xs.astype(float)+0.5, ys.astype(float)+0.5
    values = weight[ys,xs]
    strong = strict[ys,xs]
    box = [int(xs.min()),int(ys.min()),int(xs.max())+1,int(ys.max())+1]
    bw, bh = box[2]-box[0],box[3]-box[1]
    result = {'candidate_id':number,'original_pixel_count':len(xs),
              'strict_pixel_count':int(strong.sum()),'bbox_xyxy':box,
              'accepted':False,'rejection_reasons':[]}
    reasons = result['rejection_reasons']
    if len(xs) < PARAMETERS['minimum_original_pixels']:
        reasons.append('too_few_colored_pixels')
    if int(strong.sum()) < PARAMETERS['minimum_strict_pixels']:
        reasons.append('too_few_strict_colored_pixels')
    if min(bw,bh) < PARAMETERS['minimum_span_px']:
        reasons.append('cross_too_small_to_resolve_four_arms')
    if max(bw,bh) > PARAMETERS['maximum_span_px']:
        reasons.append('colored_component_too_large')
    if not 0.4 <= bw/bh <= 2.5:
        reasons.append('component_not_two_axis_cross_extent')
    if box[0] == 0 or box[1] == 0 or box[2] == w or box[3] == h:
        reasons.append('colored_component_touches_image_edge')
    if reasons:
        return result
    try:
        fit = fit_cross(x,y,values)
        result['fit'] = fit
        cx,cy = fit['center_px']
        if not box[0] <= cx <= box[2] or not box[1] <= cy <= box[3]:
            reasons.append('intersection_outside_colored_component')
        if max(fit['horizontal_rms_px'],fit['vertical_rms_px']) > PARAMETERS['maximum_line_rms_px']:
            reasons.append('line_fit_residual_too_large')
        if 90-fit['intersection_angle_deg'] > PARAMETERS['maximum_orthogonality_error_deg']:
            reasons.append('line_intersection_not_near_orthogonal')
        if fit['cross_weight_fraction'] < PARAMETERS['minimum_cross_weight_fraction']:
            reasons.append('too_much_colored_weight_off_cross')
        for name,arm in fit['arms'].items():
            if arm['pixels'] < PARAMETERS['arm_minimum_pixels'] or arm['extent_px'] < PARAMETERS['arm_minimum_extent_px']:
                reasons.append('unresolved_arm_'+name)
        strict_fit = fit_cross(x[strong],y[strong],values[strong])
        shift = float(np.linalg.norm(np.asarray(fit['center_px'])-strict_fit['center_px']))
        result['strict_center_px'] = strict_fit['center_px']
        result['threshold_center_shift_px'] = shift
        if shift > PARAMETERS['maximum_threshold_center_shift_px']:
            reasons.append('center_unstable_across_color_thresholds')
        result['accepted'] = not reasons
    except ValueError as exc:
        reasons.append(str(exc))
    return result


def detect(raw):
    with Image.open(io.BytesIO(raw)) as source:
        if source.format != 'PNG':
            raise ValueError('input must be a PNG')
        w,h = source.size
        if w*h > PARAMETERS['maximum_png_pixels']:
            raise ValueError('PNG exceeds bounded analysis size')
        rgb = np.asarray(source.convert('RGB'),dtype=np.float32)
    low = np.minimum(rgb[:,:,0],rgb[:,:,2])
    high = np.maximum(rgb[:,:,0],rgb[:,:,2])
    excess = low-rgb[:,:,1]
    balance = low/np.maximum(high,1)
    masks = {}
    for mode in ('loose','strict'):
        masks[mode] = ((low >= PARAMETERS[mode+'_min_red_blue']) &
            (excess >= PARAMETERS[mode+'_min_magenta_excess']) &
            (balance >= PARAMETERS[mode+'_min_red_blue_balance']))
    color_count = int(masks['loose'].sum())
    result = {'width':w,'height':h,'loose_color_pixels':color_count,
              'strict_color_pixels':int(masks['strict'].sum()),'candidates':[],
              'status':'not_detected','selected_candidate_id':None,'center_px':None}
    if not color_count:
        result['failure_reason'] = 'no_magenta_color_samples'
        return result
    if color_count > PARAMETERS['maximum_loose_color_pixels']:
        result['status'] = 'ambiguous'
        result['failure_reason'] = 'too_many_magenta_pixels_for_bounded_cross_detection'
        return result
    weight = np.maximum(excess,0)*np.sqrt(low/255)
    groups = connected_groups(masks['loose'])
    for number,(xs,ys) in enumerate(groups,1):
        result['candidates'].append(evaluate(xs,ys,weight,masks['strict'],(w,h),number))
    accepted = [candidate for candidate in result['candidates'] if candidate['accepted']]
    result['accepted_candidate_count'] = len(accepted)
    if len(accepted) > 1:
        result['status'] = 'ambiguous'
        result['failure_reason'] = 'multiple_independent_cross_candidates'
    elif accepted:
        chosen = accepted[0]
        result['status'] = 'detected_unique'
        result['selected_candidate_id'] = chosen['candidate_id']
        result['center_px'] = chosen['fit']['center_px']
        result['center_normalized'] = [result['center_px'][0]/w,result['center_px'][1]/h]
        result['localization_sensitivity_px'] = chosen['threshold_center_shift_px']
        result['sensitivity_is_accuracy_or_confidence_bound'] = False
    else:
        result['failure_reason'] = 'no_candidate_passed_color_and_four_arm_geometry_checks'
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--png',type=Path,required=True)
    parser.add_argument('--out',type=Path,required=True,help='New JSON diagnostic; existing files refused')
    args = parser.parse_args()
    if args.out.exists():
        parser.error('refusing to overwrite any existing report')
    raw = args.png.read_bytes()
    report = {'schema':'dm-gta-independent-world-cross-detection/1',
              'created_utc':datetime.now(timezone.utc).isoformat(),
              'input':{'path':str(args.png.resolve()),'bytes':len(raw),'sha256':digest(raw)},
              'detector_source':{'path':str(Path(__file__).resolve()),'sha256':digest(Path(__file__).read_bytes())},
              'method':'whole-PNG magenta segmentation, connected candidates, independent orthogonal-line intersection',
              'coordinate_convention':'image edge origin; pixel centers at column+0.5,row+0.5',
              'native_uv_used':False,'predicted_uv_used':False,'search_region':'entire_png',
              'parameters':PARAMETERS,'formal_calibration_approval':False,
              'editor_equivalence_verified':False,'source_pixels_modified':False,
              'limits':['Color and cross geometry cannot prove marker identity against an identical scene object.',
                        'Small, occluded, clipped, heavily blurred, or multiply present markers remain unresolved.',
                        'Threshold sensitivity is diagnostic, not an accuracy or confidence bound.']}
    try:
        report['detection'] = detect(raw)
    except Exception as exc:
        report['detection'] = {'status':'error','error':str(exc),'center_px':None}
    report['source_sha256_unchanged'] = digest(args.png.read_bytes()) == report['input']['sha256']
    if not report['source_sha256_unchanged']:
        report['detection'] = {'status':'error','error':'source PNG changed during analysis','center_px':None}
    args.out.parent.mkdir(parents=True,exist_ok=True)
    with args.out.open('x',encoding='utf-8',newline='\n') as stream:
        json.dump(report,stream,ensure_ascii=False,indent=2,allow_nan=False)
        stream.write('\n')
    status = report['detection']['status']
    print(json.dumps({'status':status,'report':str(args.out.resolve()),
                      'center_px':report['detection'].get('center_px')},ensure_ascii=False))
    return 0 if status == 'detected_unique' else 1


if __name__ == '__main__':
    raise SystemExit(main())

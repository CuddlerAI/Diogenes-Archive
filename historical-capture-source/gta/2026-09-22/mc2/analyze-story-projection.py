"""Analyze immutable Story PNGs independently; never approve Editor calibration."""
import argparse
import importlib.util
import json
import math
from pathlib import Path

import numpy as np
from gta_capture.calibration import camera_basis, solve_projection
from gta_capture.jsonio import sha256_file, utc_now


def module(name, filename):
    spec=importlib.util.spec_from_file_location(name,Path(__file__).with_name(filename))
    value=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def record(path):
    path=Path(path).resolve()
    return {'path':str(path),'bytes':path.stat().st_size,'sha256':sha256_file(path)}


def stats(values):
    if not values: return None
    a=np.asarray(values,dtype=float)
    return {'count':len(a),'rms':float(np.sqrt(np.mean(a*a))),
            'median':float(np.median(a)),'p95':float(np.percentile(a,95)),
            'p99':float(np.percentile(a,99)),'max':float(np.max(a))}


def ray(observation):
    camera=observation['camera_before']
    basis=np.asarray(camera_basis(camera['quaternion']))
    relative=np.asarray(observation['point_world'])-camera['position']
    x,depth,z=basis@relative
    if depth<2: raise ValueError('observed point is too near or behind camera')
    return [float(x/depth),float(-z/depth)],float(depth)


def fit(rows):
    rays=np.asarray([r['ray'] for r in rows])
    pixels=np.asarray([r['pixel'] for r in rows])
    xu=np.column_stack([rays[:,0],np.ones(len(rows))])
    yv=np.column_stack([rays[:,1],np.ones(len(rows))])
    if np.linalg.matrix_rank(xu)!=2 or np.linalg.matrix_rank(yv)!=2:
        raise ValueError('degenerate measured pixel fit')
    fx,cx=np.linalg.lstsq(xu,pixels[:,0],rcond=None)[0]
    fy,cy=np.linalg.lstsq(yv,pixels[:,1],rcond=None)[0]
    if fx<=0 or fy<=0: raise ValueError('nonpositive measured focal length')
    return {'fx':float(fx),'fy':float(fy),'cx':float(cx),'cy':float(cy)}


def residual(row,k):
    predicted=[k['fx']*row['ray'][0]+k['cx'],k['fy']*row['ray'][1]+k['cy']]
    return math.dist(predicted,row['pixel'])


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--manifest',type=Path,required=True)
    p.add_argument('--out-dir',type=Path,required=True)
    args=p.parse_args()
    batch=module('story_batch','story-projection-batch.py')
    detector=module('story_marker','detect-world-cross.py')
    manifest_path,plan=batch.load_manifest(args.manifest)
    args.out_dir.mkdir(parents=True,exist_ok=False)
    rows=[]
    omitted=[]
    for sample in plan['samples']:
        attempts=batch.attempts_for(manifest_path.parent,sample)
        complete=[a for a in attempts if (a/'capture-receipt.json').is_file()]
        if len(complete)>1: raise ValueError('multiple successful attempts require review')
        if not complete:
            omitted.append({'sample_id':sample['sample_id'],'reason':'not captured','attempts':list(map(str,attempts))})
            continue
        attempt=complete[0]
        batch.verify_attempt(attempt,manifest_path,sample)
        png=attempt/'point.png'
        png_binding=record(png)
        # Pixel detector receives ONLY PNG bytes. Native UV is read afterwards.
        detection=detector.detect(png.read_bytes())
        if record(png)!=png_binding: raise ValueError('PNG changed during analysis')
        observation=json.loads((attempt/'observation.json').read_text(encoding='utf-8'))
        if (observation['width'],observation['height'])!=(1920,1080):
            raise ValueError('sample dimensions differ from planned native 1920x1080')
        row={'sample_id':sample['sample_id'],'png':png_binding,
             'capture_receipt':record(attempt/'capture-receipt.json'),
             'observation':record(attempt/'observation.json'),
             'nominal_focal_mm':observation['nominal_focal_mm'],
             'detection':detection,'used_in_fit':False}
        if detection['status']=='detected_unique':
            normalized,depth=ray(observation)
            pixel=detection['center_px']
            native=[observation['screen_normalized'][0]*observation['width'],
                    observation['screen_normalized'][1]*observation['height']]
            row.update({'used_in_fit':True,'pixel':pixel,'native_pixel':native,
                        'native_to_pixel_error_px':math.dist(pixel,native),
                        'ray':normalized,'depth_units':depth,
                        'camera':observation['camera_before'],'world':observation['point_world']})
        else: omitted.append({'sample_id':sample['sample_id'],'reason':detection['status']})
        rows.append(row)
    accepted=[r for r in rows if r['used_in_fit']]
    per_focal=[]
    for focal in (18,24,35,50):
        group=[r for r in accepted if r['nominal_focal_mm']==focal]
        result={'nominal_focal_mm':focal,'accepted_samples':len(group)}
        if len(group)>=8:
            k=fit(group)
            errors=[residual(r,k) for r in group]
            loo=[]
            for i,r in enumerate(group):
                loo.append(residual(r,fit(group[:i]+group[i+1:])))
            result.update({'pixel_intrinsics_with_fitted_principal_point':k,
                           'fit_error_px':stats(errors),'leave_one_out_error_px':stats(loo),
                           'native_to_pixel_error_px':stats([r['native_to_pixel_error_px'] for r in group]),
                           'measured_spread_fraction':np.ptp(np.asarray([r['pixel'] for r in group]),axis=0).tolist(),
                           'depth_units_range':[min(r['depth_units'] for r in group),max(r['depth_units'] for r in group)]})
            result['measured_spread_fraction']=[result['measured_spread_fraction'][0]/1920,result['measured_spread_fraction'][1]/1080]
        per_focal.append(result)
    report={'schema':'dm-gta-story-pixel-calibration-analysis/1','created_at':utc_now(),
            'manifest':record(manifest_path),'analysis_source':record(__file__),
            'detector_source':record(Path(__file__).with_name('detect-world-cross.py')),
            'detector_parameters':detector.PARAMETERS,
            'pixel_coordinate_convention':'image edge origin; pixel centers column+0.5,row+0.5',
            'native_uv_used_for_detection':False,'planned_uv_used_for_detection':False,
            'planned_count':len(plan['samples']),'captured_count':len(rows),'independently_detected_count':len(accepted),
            'omitted_or_unresolved':omitted,'per_focal':per_focal,'samples':rows,
            'approval_status':'draft','editor_equivalence_verified':False,'delivery_eligible':False,
            'metric_scale_approved':False,'non_trainable':True,
            'limits':['Story measurements only; transfer to Rockstar Editor requires separate real pixel evidence.',
                      'Nominal focal labels do not establish physical sensor dimensions or customer acceptance.',
                      'World coordinates are observed GTA units; no independent real-metre scale approval.',
                      'Marker threshold sensitivity is not an accuracy or confidence bound.']}
    try:
        solution=solve_projection([{'focal_mm':r['nominal_focal_mm'],'pixel':r['pixel'],
                                    'world':r['world'],'camera':r['camera']} for r in accepted],width=1920,height=1080)
        report['existing_centered_solver_diagnostic']=solution
        crosscheck=stats([r['native_to_pixel_error_px'] for r in accepted])
        report['native_to_independent_pixel_error_px']=crosscheck
        report['numerical_sampling_criteria_met']=(len(accepted)>=100 and solution['reprojection']['error_px']<=1
                                                  and crosscheck['p95']<=1 and crosscheck['p99']<=2)
    except Exception as exc:
        report['existing_centered_solver_error']=str(exc)
        report['numerical_sampling_criteria_met']=False
    target=args.out_dir/'analysis.json'
    with target.open('x',encoding='utf-8') as f: json.dump(report,f,ensure_ascii=False,indent=2,allow_nan=False)
    print(json.dumps({key:report[key] for key in ('captured_count','independently_detected_count','numerical_sampling_criteria_met','approval_status')}))
    return 0


if __name__=='__main__': raise SystemExit(main())

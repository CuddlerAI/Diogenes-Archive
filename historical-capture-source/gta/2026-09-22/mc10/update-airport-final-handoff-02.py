"""Add read-only binding diagnostics and update the airport viewing handoff.

No game, decoder, PNG renderer, formal QC writer, or approval action is called.
"""
import hashlib
import json
import re
import struct
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
RUN = BASE / 'scene01-airport/run-03'
OUT = BASE / 'review/scene01-airport-final-bundle-02'
OBS_PATH = BASE / 'review/scene01-preview-decoded-01/root-visual-observations-01.json'
if OUT.exists() or OBS_PATH.exists():
    raise SystemExit('Refusing to replace an existing additive report')
sys.path.insert(0, str(BASE.parent.parent))
from gta_capture.qc_bridge import _bind_or_verify_gate, _qc_bridge_record

files, checks = {}, []
def record(path):
    p = Path(path).resolve()
    key = str(p).casefold()
    if key not in files:
        before = p.stat()
        h = hashlib.sha256()
        with p.open('rb') as f:
            for b in iter(lambda: f.read(4 * 1024 * 1024), b''):
                h.update(b)
        after = p.stat()
        assert (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns), str(p)
        files[key] = dict(path=str(p), bytes=after.st_size, sha256=h.hexdigest())
    return files[key]

def read(path):
    record(path)
    return json.loads(Path(path).read_text(encoding='utf-8'))

def check(label, condition):
    checks.append(dict(id=label, status='PASS' if condition else 'FAIL'))
    if not condition:
        raise ValueError(label)

def verify(item, label):
    actual = record(item['path'])
    check(label, actual['sha256'] == item['sha256'] and actual['bytes'] == item['bytes'])
    return actual

manifest = read(RUN / 'run-manifest.json')
qc_path = RUN / 'evidence/engine-qc.json'
qc = read(qc_path)
gate, current = _bind_or_verify_gate(RUN, manifest, allow_bind=False)
expected = _qc_bridge_record(RUN, gate, current, exit_code=qc['gta_capture_bridge']['exit_code'], stderr=qc['gta_capture_bridge']['stderr'])
check('qc_binding_exact_current_files_and_gate', expected == qc['gta_capture_bridge'])
check('formal_fail_retained', qc['verdict'] == 'FAIL' and expected['exit_code'] == 1 and gate['ok'] is False)
counts = Counter(x['status'] for x in qc['results'])
check('qc_actual_result_counts', all(counts.get(k, 0) == v for k, v in qc['counts'].items()) and
      qc['counts'] == {'PASS': 148, 'FAIL': 1, 'SKIP': 14, 'UNVERIFIED': 0})
failures = [x for x in qc['results'] if x['status'] == 'FAIL']
skips = [x for x in qc['results'] if x['status'] == 'SKIP']
check('sole_actual_input_failure', len(failures) == 1 and failures[0]['rule'] == 'num.input_stream' and
      failures[0]['target'] == 'action_camera_1.json')
old_audit_path = BASE / 'review/scene01-airport-final-bundle-01/audit.json'
old_audit = read(old_audit_path)
check('old_audit_unchanged', record(old_audit_path)['sha256'] == 'cbf1390305546872f08c0fa6c1f46a443f7f056b84726e730ad269f011663520')
old_review_path = BASE / 'review/scene01-airport-01/scene-review.json'
old_review = read(old_review_path)
check('old_review_unchanged', record(old_review_path)['sha256'] == 'df798c9e01f4ba76fefe6b2bb16ff3d212c7034814e68932e3d53b13cafcdc80')
new_review_path = BASE / 'review/scene01-airport-02/scene-review.json'
review = read(new_review_path)
check('new_review_exact_sha', record(new_review_path)['sha256'] == 'e4bf81a67bed59d84caf962d659b80ad166cb09a0ddb7b95b89d1fee0e1b8a92')
check('new_review_completed_integrity', review['integrity_status'] == 'PASS' and not review['errors'] and
      all(v == 'PASS' for v in review['checks'].values()))
check('new_review_retains_actual_qc', review['retained_qc']['verdict'] == qc['verdict'] and
      review['retained_qc']['counts'] == qc['counts'] and review['retained_qc']['failures'] == failures)
by_path = {str(Path(x['path']).resolve()).casefold(): x for x in review['inputs']}
for path in [qc_path, RUN/'evidence/gta-pr405-gate.json', RUN/'run-manifest.json']:
    verify(by_path[str(path).casefold()], 'new_review_bound:' + path.name)
check('same_png_scan_and_event_set', review['raw_pngs'] == old_review['raw_pngs'])
check('same_metadata_and_video_seals', current['delivery_files'] == qc['gta_capture_bridge']['delivery_files'])
for item in current['inputs'].values():
    verify(item, 'current_input:' + Path(item['path']).name)
for item in current['delivery_files'].values():
    verify(item, 'current_delivery:' + Path(item['path']).name)
for item in current['qc_sidecars'].values():
    verify(item, 'current_qc_sidecar:' + Path(item['path']).name)
for lane in range(1, 5):
    v = review['videos'][str(lane)]
    check(f'lane{lane}_same_media_source', v['source'] == old_review['videos'][str(lane)]['source'])
    verify(v['probe'], f'lane{lane}_new_decode_receipt')
    probe = read(v['probe']['path'])
    video = next(x for x in probe['streams'] if x['codec_type'] == 'video')
    check(f'lane{lane}_saved_actual_decode_1800', int(video['nb_read_frames']) == 1800 and
          video['width'] == 1920 and video['height'] == 1080 and video['avg_frame_rate'] == '30/1' and
          abs(float(probe['format']['duration']) - 60) < .001)
    verify(review['raw_pngs']['index_sources'][str(lane)], f'lane{lane}_same_sealed_events')

preview_manifest_path = BASE/'preview/scene01-airport-01/review-media-manifest.json'
preview = read(preview_manifest_path)
quad_record = next(x for x in preview['outputs'] if Path(x['path']).name == 'four-camera-preview.mp4')
verify(quad_record, 'encoded_preview_source_sha')
check('quad_same_as_prior_audit', quad_record == old_audit['quad'])
observations = []
for n, frame in enumerate([262, 900, 1799], 1):
    path = OBS_PATH.parent / f'frame_{n:02d}.png'
    rec = record(path)
    with path.open('rb') as f:
        header = f.read(24)
    check(f'encoded_preview_png{n}_dimensions', header[:8] == b'\x89PNG\r\n\x1a\n' and
          header[12:16] == b'IHDR' and struct.unpack('>II', header[16:24]) == (1920, 1080))
    observations.append(dict(encoded_frame_index_reported_by_root=frame, frame_index_base=0,
        output_png=rec, dimensions=[1920, 1080], lanes_visible=[1, 2, 3, 4],
        root_reported_observation='四格人物均完整；机场环境一致；未见明显 HUD 或指针。' +
            ('前两张可见行走姿态。' if n < 3 else '最后一张为自然结束站立。') +
            ('俯视格人物靠近上边缘但完整。' if frame == 262 else ''),
        writer_independently_viewed_image=False,
        extraction_frame_index_independently_verified_by_writer=False))
observation = dict(schema='dm-gta-root-limited-encoded-preview-observations/1',
    created_at=datetime.now(timezone.utc).isoformat(), run_dir=str(RUN),
    status='LIMITED_ROOT_OBSERVATIONS_RECORDED', source_encoded_video=quad_record,
    source_preview_manifest=record(preview_manifest_path),
    observation_attribution=dict(observer='/root', observer_kind='root AI agent',
        source='Root task message explicitly reports completed decoding and viewing of encoded preview frames 262, 900 and 1799.',
        report_writer='/root/calibration_audit', report_writer_independently_viewed_images=False,
        actual_extraction_argv_not_supplied=True, extraction_indices_source='root explicit report; not inferred from PNG filenames'),
    observations=observations, encoded_frame_count_sampled=3, tile_observation_count=12,
    full_continuous_video_review=False, subject_visibility_all_frames_verified=False,
    whole_image_cursor_free_verified=False, human_review_approval=False, independent_human_review=False,
    formal_quality_gate_pass=False, calibration_approval=False, delivery_eligible=False,
    non_trainable=True, granted_rules=[], limitations=[
        'Only the three root-reported encoded preview samples are described; this writer checks bytes/hash/dimensions, not visual contents.',
        'Three frames cannot prove all1800-frame subject visibility, clean UI, synchronization of bones, or absence of clipping.',
        'Quad tiles are 960x540 derivatives, not full-resolution original PNGs.',
        'No extraction is rerun and no command/timestamp is fabricated.'])

OUT.mkdir(parents=True, exist_ok=False)
with OBS_PATH.open('x', encoding='utf-8', newline='\n') as f:
    json.dump(observation, f, ensure_ascii=False, indent=2); f.write('\n')
obs_record = record(OBS_PATH)
audit = dict(schema='dm-gta-airport-final-bundle-audit/2', created_at=datetime.now(timezone.utc).isoformat(),
    run_dir=str(RUN), integrity_status='PASS', checks=checks, failures=[],
    prior_audit=record(old_audit_path), prior_review=record(old_review_path),
    current_review=record(new_review_path), engine_qc=record(qc_path),
    gate=record(RUN/'evidence/gta-pr405-gate.json'), run_manifest=record(RUN/'run-manifest.json'),
    actual_qc=dict(verdict=qc['verdict'], counts=qc['counts'], failures=failures, skips=skips),
    gate_core_unchanged=True, gate_only_added_run_binding=True,
    retained_gate=dict(ok=gate['ok'], passed_rules=gate['passed_rules'], missing_rules=gate['missing_rules'],
                       internal_poc_usable=gate['internal_poc_usable'], delivery_eligible=gate['delivery_eligible']),
    encoded_preview_observations=obs_record, verified_files=list(files.values()),
    full_png_scan_reused_from_current_review=True, raw_pngs_rehashed_this_audit=0, new_decode_performed=False,
    human_review_approval=False, formal_quality_gate=False, calibration_approval=False,
    delivery_eligible=False, non_trainable=True, granted_rules=[],
    limitations=['PASS means these new reports correctly bind unchanged scene media; formal QC remains FAIL.',
                 'The first audit and first scene review are historical pre-QC records and remain unchanged.',
                 'The imported gate verifier is called with allow_bind=False; no evidence/gate is written.'])
audit_path = OUT/'audit.json'
with audit_path.open('x', encoding='utf-8', newline='\n') as f:
    json.dump(audit, f, ensure_ascii=False, indent=2); f.write('\n')

def link(label, path):
    return f'[{label}](<{Path(path).as_posix()}>)'

readme_path = BASE/'scene01-airport/README-已完成内部Demo.md'
text = readme_path.read_text(encoding='utf-8')
text = text.replace('/review/scene01-airport-01/scene-review.json', '/review/scene01-airport-02/scene-review.json')
text = text.replace('df798c9e01f4ba76fefe6b2bb16ff3d212c7034814e68932e3d53b13cafcdc80',
                    'e4bf81a67bed59d84caf962d659b80ad166cb09a0ddb7b95b89d1fee0e1b8a92')
text = text.replace('[本次独立绑定审计]', '[首次独立绑定审计（正式 QC 落盘前的历史记录）]')
start = text.index('截至本次审计，`evidence/engine-qc.json` 尚未存在')
end = text.index('\n\n', start)
new_qc_text = (
    f'现已落盘并绑定 {link("正式 engine-qc.json", qc_path)}：**148 PASS / 1 FAIL / 14 SKIP / 0 UNVERIFIED，verdict FAIL**。'
    '唯一失败是第一路 `num.input_stream`：镜头累计转动约 142°，1800 帧的 `mouse_dx/dy` 却恒为零。'
    '这是程序控制镜头与未采集物理鼠标输入的实际限制，不能将零占位值当成真实零输入，也不伪造数值过门。'
    '14 项 SKIP 包括第 2–4 路各自的尺度、玩家速度和输入字段共 9 项，俯视机位不适用的 roll/变速合同共 4 项，'
    '以及未附 `dyn_metrics.json` 的动态活跃比例 1 项。'
    f'逐项原文保存在 {link("新增正式 QC 绑定审计", audit_path)}，其 SHA-256 为 `{record(audit_path)["sha256"]}`。'
    '最新第二版场景审查已经记录这个 FAIL 和正式 gate 的新绑定；首次审查及首次审计原字节保留，仅作历史记录。'
    'run manifest、媒体与八个元数据未变；gate 仅新增 run_binding，原通过/缺失规则不变。'
    '已封存 `trajectory-verdict.json` 继续为 FAIL、没有授予规则。')
text = text[:start] + new_qc_text + text[end:]
marker = '\n\n正式验收保持原状态：'
assert marker in text
new_observation_text = (
    f'根代理另实际解码最终四视图 MP4 的第 262、900、1799 帧并逐张看过全部四格，'
    '三张均报告人物完整、机场环境一致、未见明显 HUD/指针；前两张为行走姿态，末张为自然结束站立。'
    '第 262 帧俯视格靠近上边缘但人物完整。'
    f'{link("编码后预览有限观察回执", OBS_PATH)}绑定最终 MP4 和三张解码 PNG 的实际 SHA。'
    '该观察来自根代理 AI 的明确报告；本页作者只核验文件 hash/尺寸，没有独立看图，未重新解码，也未将三帧外推为全片批准。')
text = text.replace(marker, '\n\n' + new_observation_text + marker, 1)
for target in re.findall(r'\]\(<([^>]+)>\)', text):
    assert Path(target).exists(), target
readme_path.write_text(text, encoding='utf-8', newline='\n')
print(json.dumps(dict(readme=str(readme_path), readme_sha256=hashlib.sha256(readme_path.read_bytes()).hexdigest(),
    audit=str(audit_path), audit_sha256=record(audit_path)['sha256'], checks=len(checks),
    observations=obs_record, qc_counts=qc['counts']), ensure_ascii=False))

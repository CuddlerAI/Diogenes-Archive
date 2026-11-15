"""Bounded read-only space budget from prior audited event byte counts; no PNG scan."""
from pathlib import Path
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
import sys

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
PACKAGE = BASE.parents[1]
RUN = BASE / 'scene03-sandy-airfield/run-02'
OUTPUT = HERE / 'inventory.json'
if OUTPUT.exists():
    raise SystemExit('refuse to overwrite existing audit')

def ref(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return {'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()}

def read(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))

def event_summary(path, *, expected=None):
    path = Path(path)
    raw = path.read_bytes()
    observed = {'path': str(path.resolve()), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()}
    if expected is not None:
        assert observed == expected, 'event evidence no longer matches existing completed-scene review'
    complete = raw.rsplit(b'\n', 1)[0] if raw and not raw.endswith(b'\n') else raw
    rows = [json.loads(line) for line in complete.splitlines() if line.strip()]
    for frame, row in enumerate(rows):
        assert row['output_frame'] == frame and row['mock'] is False and row['render_succeeded'] is True
        assert type(row['output_bytes']) is int and row['output_bytes'] > 0
    sizes = [row['output_bytes'] for row in rows]
    roots = sorted({str(Path(row['output_path']).parent) for row in rows})
    return {'event_file_snapshot': observed, 'mutable_partial_snapshot': path.suffix == '.partial',
            'complete_event_count': len(rows), 'raw_png_bytes_from_native_event_records': sum(sizes),
            'raw_pngs_rehashed_by_this_audit': False, 'render_directories_from_events': roots,
            'average_png_bytes': sum(sizes) / len(sizes) if sizes else None,
            'min_png_bytes': min(sizes) if sizes else None, 'max_png_bytes': max(sizes) if sizes else None,
            'per300_source_frame_size_trend': [
                {'first_frame': i, 'last_frame': min(i + 300, len(sizes)) - 1,
                 'count': len(sizes[i:i+300]), 'bytes': sum(sizes[i:i+300]),
                 'mean_png_bytes': sum(sizes[i:i+300]) / len(sizes[i:i+300])}
                for i in range(0, len(sizes), 300)]}

prior_path = BASE / 'review/storage-audit-01/inventory.json'
prior = read(prior_path)
volume_helper = BASE / 'review/storage-audit-01/inspect-storage.py'
spec = importlib.util.spec_from_file_location('prior_storage_volume_only', volume_helper)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
# Reuse only the existing read-only volume enumerator. Do not run inventory walks.
volumes, volume_errors = module.volumes()
now = datetime.now(timezone.utc).isoformat()
c = next(v for v in volumes if 'C:\\' in v['mount_paths'])
free = c['available_bytes']
assert isinstance(free, int) and not volume_errors

airport_review_path = BASE / 'review/scene01-airport-02/scene-review.json'
airport = read(airport_review_path)
assert airport['integrity_status'] == 'PASS' and airport['raw_pngs']['count'] == 7200
airport_lanes = {lane: event_summary(value['path'], expected=value)
                 for lane, value in airport['raw_pngs']['index_sources'].items()}
assert all(v['complete_event_count'] == 1800 for v in airport_lanes.values())
airport_raw = {lane: value['raw_png_bytes_from_native_event_records'] for lane, value in airport_lanes.items()}
assert sum(airport_raw.values()) == airport['raw_pngs']['bytes']
video_refs = {lane: v['source'] for lane, v in airport['videos'].items()}
for value in video_refs.values():
    assert Path(value['path']).stat().st_size == value['bytes']
preview_path = BASE / 'preview/scene01-airport-01/review-media-manifest.json'
preview = read(preview_path)
assert preview['status'] == 'completed'
for value in preview['outputs']:
    assert Path(value['path']).stat().st_size == value['bytes']
video_total = sum(v['bytes'] for v in video_refs.values())
preview_total = sum(v['bytes'] for v in preview['outputs'])
media_total = video_total + preview_total

current = {}
for lane in ('1', '2', '3', '4'):
    path = RUN / f'evidence/frame-events-lane{lane}.jsonl'
    if not path.exists():
        path = Path(str(path) + '.partial')
    if path.exists():
        current[lane] = event_summary(path)
    else:
        current[lane] = {'complete_event_count': 0, 'raw_png_bytes_from_native_event_records': 0,
                         'event_file_snapshot': None, 'status': 'no_event_file_at_snapshot'}
assert current['1']['complete_event_count'] == 1800
current_raw = sum(v['raw_png_bytes_from_native_event_records'] for v in current.values())
scenario_overhead = 250_000_000  # Explicit estimate, not a measured allocation.
audio_scratch_budget = 48_000_000
system_headroom = 5_000_000_000
remaining_after_sandy = 8

def budget(multiplier):
    planned_lanes = {}
    for lane, data in current.items():
        count = data['complete_event_count']
        if count == 1800:
            value = data['raw_png_bytes_from_native_event_records']
        else:
            prefix_projection = (data['raw_png_bytes_from_native_event_records'] / count * 1800) if count else 0
            value = math.ceil(max(airport_raw[lane], prefix_projection) * multiplier)
        planned_lanes[lane] = value
    sandy_remaining_raw = sum(planned_lanes.values()) - current_raw
    sandy_steady_growth = sandy_remaining_raw + math.ceil(media_total * multiplier) + scenario_overhead
    scratch = max(planned_lanes.values()) + math.ceil(max(v['bytes'] for v in video_refs.values()) * multiplier) + audio_scratch_budget
    free_after_sandy = free - sandy_steady_growth
    free_at_sandy_encoding_peak = free_after_sandy - scratch
    future_scene = math.ceil((sum(airport_raw.values()) + media_total) * multiplier) + scenario_overhead
    future_scratch = math.ceil(max(airport_raw.values()) * multiplier) + math.ceil(max(v['bytes'] for v in video_refs.values()) * multiplier) + audio_scratch_budget
    total_growth_for_ten = sandy_steady_growth + remaining_after_sandy * future_scene
    additional_capacity_for_ten = max(0, total_growth_for_ten + max(scratch, future_scratch) + system_headroom - free)
    return {'unrecorded_lane_reference_multiplier': multiplier,
            'sandy_estimated_total_raw_bytes_by_lane': planned_lanes,
            'sandy_remaining_raw_growth_bytes': sandy_remaining_raw,
            'sandy_estimated_steady_growth_bytes_from_now': sandy_steady_growth,
            'sandy_estimated_free_after_finalization_bytes': free_after_sandy,
            'encoding_additional_peak_scratch_bytes': scratch,
            'sandy_estimated_min_free_at_encoding_peak_bytes': free_at_sandy_encoding_peak,
            'sandy_estimated_encoding_peak_free_above_5GB': free_at_sandy_encoding_peak > system_headroom,
            'estimated_per_additional_scene_steady_bytes': future_scene,
            'remaining_scenes_after_sandy': remaining_after_sandy,
            'estimated_growth_to_ten_completed_scenes_from_now_bytes': total_growth_for_ten,
            'estimated_additional_capacity_to_ten_with_scratch_and_headroom_bytes': additional_capacity_for_ten}

budgets = {'airport_reference': budget(1.0), 'plus25percent_for_uncaptured_data': budget(1.25)}
old_mounts = sorted(p for v in prior['volumes'] for p in v['mount_paths'])
new_mounts = sorted(p for v in volumes for p in v['mount_paths'])
report = {'schema': 'dm-gta-readonly-storage-audit/2', 'observed_at_utc': now,
          'scope': 'Volume free space plus existing audited small event/manifest files. No RAW directory walk, PNG hash/read, deletion or move.',
          'sources': {'prior_storage_audit': ref(prior_path), 'reused_volume_helper': ref(volume_helper),
                      'airport_completed_scene_review': ref(airport_review_path), 'airport_preview_manifest': ref(preview_path),
                      'current_media_source': ref(PACKAGE / 'gta_capture/media.py'), 'audit_helper': ref(__file__)},
          'volume_measurement': {'method': 'existing ordinary read-only Win32 volume enumeration/free-space queries',
                                 'Get_Volume_attempt': 'access denied; no escalation; ordinary volume enumeration succeeded',
                                 'volumes': volumes, 'errors': volume_errors, 'mounted_paths': new_mounts,
                                 'new_mounted_paths_since_audit01': sorted(set(new_mounts) - set(old_mounts)),
                                 'new_volume_count_since_audit01': len({v['volume'] for v in volumes} - {v['volume'] for v in prior['volumes']}),
                                 'current_C_available_bytes': free},
          'airport_actual_reference': {'raw_pngs': 7200, 'raw_bytes_by_lane': airport_raw,
                                       'raw_total_bytes': sum(airport_raw.values()), 'lane_event_checks': airport_lanes,
                                       'four_final_mp4_bytes': video_total, 'preview_and_contact_derivatives_bytes': preview_total,
                                       'bytes_basis': 'Raw summed from native event records whose file hashes match prior full review; video/preview sizes from existing receipts and current stat only.'},
          'sandy_current_snapshot': {'run': str(RUN), 'lanes': current, 'accounted_raw_bytes_already_on_disk': current_raw,
                                     'lane1_render0064_is_complete_at_this_snapshot': True,
                                     'completed_scene': False, 'finalization_not_proven_by_this_storage_audit': True},
          'budget_assumptions': {'all_paths_on_C': True, 'successful_completed_scenes_now': 1,
                                 'only_selected_completed_scene': 'scene01-airport/run-03', 'target_scenes': 10,
                                 'scene_frames_per_lane': 1800, 'lanes_per_scene': 4,
                                 'scenario_audio_metadata_config_review_budget_bytes': scenario_overhead,
                                 'extra_audio_scratch_budget_bytes': audio_scratch_budget,
                                 'operating_headroom_budget_bytes': system_headroom,
                                 'old_failed_attempts_retained': True, 'additional_failed_attempts_budgeted': False,
                                 'unknown_future_scene_sizes_are_estimates_not_upper_bounds': True},
          'budgets': budgets,
          'media_scratch_contract': {'source_lines': [309, 337, 343, 364, 373],
                                     'png_copies_retained_simultaneously': 'one successfully processed lane at a time',
                                     'extra_encoded_silent_video': 'one current lane alongside mux output',
                                     'success_releases_only_owned_current_lane_copies': True,
                                     'failure_retains_current_scratch': True,
                                     'no_raw_original_deletion_implied': True,
                                     'prior_audit01_scratch_description_is_historical': True},
          'limits': ['Free space changes during rendering and other application activity; estimates must be refreshed before later lanes/finalization.',
                     'No new mounted data volume found. Two existing unmounted small volumes are not presumed usable recording targets.',
                     'Neither estimate guarantees future compression ratios, reshoot volume, cache growth or other applications use.',
                     'No proposed cleanup, transfer or existing RAW path change is authorized by this report.'],
          'writes': 'only new storage-audit-02 report files', 'raw_pngs_rehashed': False,
          'raw_render_directories_walked': False, 'game_API_or_UI_used': False,
          'deletion_or_move_performed': False, 'formal_quality_approved': False, 'delivery_eligible': False}
with OUTPUT.open('x', encoding='utf-8') as stream:
    json.dump(report, stream, ensure_ascii=False, indent=2)
    stream.write('\n')

gb = lambda x: f'{x / 1e9:.2f}'
gib = lambda x: f'{x / (1024**3):.2f}'
lines = ['# 只读空间审计：Sandy 收尾可预算，十场仍有容量缺口', '',
         f'采样时间：{now}。C 盘实测可用 **{gb(free)} GB / {gib(free)} GiB**；仍只有 C:\\ 挂载，未发现新卷或新增挂载点。两个无挂载小卷与上次相同，未作为采集空间。Get-Volume 被拒绝后，复用上次普通只读卷枚举成功，没有申请升级权限。', '',
         '以下 GB 均为十进制；实际记录与估算分别标明。未遍历旧 RAW、未重新读取或哈希任何 PNG，未删移文件。', '',
         '## 实际体量', '',
         '| 项目 | GB | 来源 |', '|---|---:|---|']
for lane, value in airport_raw.items():
    lines.append(f'| 已完成机场 lane{lane} 的1800原PNG | {gb(value)} | 事件字节数，事件文件SHA匹配既有完整审查 |')
lines += [f'| 机场四路RAW合计 | {gb(sum(airport_raw.values()))} | 7200帧既有实测 |',
          f'| 机场四个最终MP4 | {gb(video_total)} | 既有审查记录及本次stat |',
          f'| 机场预览和联系表等派生物 | {gb(preview_total)} | 既有manifest及本次stat |',
          f'| 当前Sandy lane1／render_0064 | {gb(current["1"]["raw_png_bytes_from_native_event_records"])} | 本次快照已封存完整1800帧，不再外推第一路 |', '',
          '0064每300源帧的大小走势保存在机器报告；均值约3.30 MB/PNG，单帧范围约2.98–3.67 MB。其余路尚未完整生成的部分须估算，不能把第一路均值当作所有焦距的实测值。', '',
          '## 同卷继续完成 Sandy', '',
          '已有一路RAW占用已包含在当前剩余空间内，不重复扣减。剩余三路以机场对应路为基准，另列未知数据增加25%的预算情形；音频、元数据和配置等预留0.25 GB/场，运行余量另预留5 GB。这些是预算假设，不是硬性门槛或大小上界。', '',
          '| 从当前快照起估算 | 机场基准 | 未采集数据+25% |', '|---|---:|---:|']
labels = [('其余RAW增量', 'sandy_remaining_raw_growth_bytes'),
          ('完成编码后预计剩余', 'sandy_estimated_free_after_finalization_bytes'),
          ('编码额外峰值scratch', 'encoding_additional_peak_scratch_bytes'),
          ('编码峰值时预计最少剩余', 'sandy_estimated_min_free_at_encoding_peak_bytes')]
for label, key in labels:
    lines.append(f'| {label} GB | {gb(budgets["airport_reference"][key])} | {gb(budgets["plus25percent_for_uncaptured_data"][key])} |')
lines += ['', '当前 media.py 会先独占复制一路完整PNG再编码／mux，验证成功后只释放本轮已登记的派生副本，再处理下一路。因此峰值要加**一整路RAW副本、一份当前silent MP4及音频副本**；不能只预算最终MP4，也无需再按四路RAW同时复制计算。失败路scratch会保留。本次没有清理授权，也没有清理动作。', '',
          '## 十场目标', '',
          '目前只有机场一场完成；Sandy若完成才计第二场，此后仍有八场。旧港区问题attempt及其他旧RAW均保留在现有占用中，估算没有假设删除它们；未来失败重导也尚未计入。', '',
          f'按机场基准，完成十场并覆盖一路编码scratch和5 GB运行余量，当前仍缺约 **{gb(budgets["airport_reference"]["estimated_additional_capacity_to_ten_with_scratch_and_headroom_bytes"])} GB**；未采集数据增加25%的预算为 **{gb(budgets["plus25percent_for_uncaptured_data"]["estimated_additional_capacity_to_ten_with_scratch_and_headroom_bytes"])} GB**。这不是已新增容量，也不是未来场景压缩率保证。', '',
          '现有空间可按这两个预算继续完成当前Sandy收尾，但无法承诺十场全部原始数据保留。后续每路前应依据当时实际剩余与已生成体量刷新预算。本报告没有向用户重复询问路径，没有操作游戏、进程、配置或外部服务。', '',
          '完整机器记录：`inventory.json`；复核工具：`inspect-storage-02.py`。仅做存储预算，不授予任何质量或标定批准。', '']
with (HERE / 'SUMMARY.md').open('x', encoding='utf-8') as stream:
    stream.write('\n'.join(lines))
print(json.dumps({'inventory': ref(OUTPUT), 'summary': ref(HERE / 'SUMMARY.md'),
                  'C_available_GB': free/1e9, 'budgets': budgets}, ensure_ascii=False))

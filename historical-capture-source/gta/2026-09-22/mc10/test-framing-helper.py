"""Synthetic boundary checks for the ignored offline helper; no game operations."""
import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('framing_helper', BASE/'derive-recorded-body-framing.py')
helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(helper)
sys.path.insert(0, str(helper.PACKAGE/'tests'))
from helpers import scenario_dict


class FramingHelperTests(unittest.TestCase):
    def records(self):
        return [{'camera_position': [0, 1, 0], 'camera_rotation_quaternion': [0, 0, 0, 1],
                 'camera_intrinsics': {'fx': 100, 'fy': 100, 'cx': 500, 'cy': 500}} for _ in range(2)]

    def test_body_box_follows_time_varying_entity_height(self):
        records = self.records()
        records[1]['camera_position'][1] += 2
        entity = helper.np.array([[0, 0, 5], [0, 2, 5]], dtype=float)
        report = helper.project_body_envelope(records, entity, {'width': 1000, 'height': 1000},
                                              -1.2, 1.85, include_frames=True)
        self.assertEqual(report['in_frustum_ratio'], 1.0)
        self.assertAlmostEqual(report['frame_margins'][0]['min_edge_margin_px'],
                               report['frame_margins'][1]['min_edge_margin_px'])

    def test_offscreen_body_is_failure_with_exact_frame_index(self):
        entity = helper.np.array([[0, 0, 5], [0, -100, 5]], dtype=float)
        report = helper.project_body_envelope(self.records(), entity, {'width': 1000, 'height': 1000},
                                              -1.2, 1.85, include_frames=True)
        self.assertEqual(report['in_frustum_ratio'], 0.5)
        self.assertEqual(report['out_of_frustum_frames'], [1])
        self.assertEqual(report['worst_output_frame'], 1)
        self.assertLess(report['min_edge_margin_px'], 0)

    def test_behind_camera_is_failure_and_json_has_no_nonfinite_numbers(self):
        report = helper.project_body_envelope(self.records(), helper.np.array([[0, 0, -5]]*2),
                     {'width': 1000, 'height': 1000}, -1.2, 1.85, include_frames=True)
        self.assertEqual(report['near_plane_failed_frames'], [0, 1])
        self.assertEqual(report['in_frustum_ratio'], 0.0)
        self.assertIsNone(report['min_edge_margin_px'])
        json.dumps(report, allow_nan=False)

    def test_existing_output_refused_before_reading_or_rewriting_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root/'keep.txt'; marker.write_bytes(b'preserve existing evidence')
            args = SimpleNamespace(record_run_dir=root/'missing-run', source_plan=root/'missing-source',
                                   out_dir=root, scenario=root/'missing-scenario.json')
            with self.assertRaisesRegex(helper.ContractError, 'output already exists'):
                helper.source_inputs(args)
            self.assertEqual(marker.read_bytes(), b'preserve existing evidence')
            self.assertEqual(list(root.iterdir()), [marker])

    def test_incomplete_record_refused_without_creating_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); (root/'evidence').mkdir()
            (root/'scenario.json').write_text(json.dumps(scenario_dict(60)), encoding='utf-8')
            (root/'evidence/native-record-result.json').write_text(
                json.dumps({'ok': False, 'mock_fixture': True}), encoding='utf-8')
            args = SimpleNamespace(record_run_dir=root, source_plan=root/'incomplete-plan',
                                   out_dir=root/'new-output', scenario=root/'scenario.json')
            with self.assertRaisesRegex(helper.ContractError, 'incomplete or unsuccessful'):
                helper.source_inputs(args)
            self.assertFalse(args.out_dir.exists())


if __name__ == '__main__':
    output = BASE/'review/framing-helper-boundary-checks-01.json'
    if output.exists():
        raise SystemExit('refusing to overwrite existing test receipt')
    stream = io.StringIO()
    result = unittest.TextTestRunner(stream=stream, verbosity=2).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(FramingHelperTests))
    receipt = {'schema': 'dm-gta-offline-helper-test-result/1',
        'helper': helper.file_record(BASE/'derive-recorded-body-framing.py'),
        'test_source': helper.file_record(Path(__file__)), 'scope': 'synthetic unit boundaries; no game or calibration',
        'tests_run': result.testsRun, 'failures': len(result.failures), 'errors': len(result.errors),
        'passed': result.wasSuccessful(), 'actual_unittest_output': stream.getvalue()}
    helper.write_json_atomic(output, receipt)
    print(stream.getvalue(), end='')
    print('receipt:', output)
    raise SystemExit(0 if result.wasSuccessful() else 1)

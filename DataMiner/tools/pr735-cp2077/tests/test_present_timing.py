import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "present_timing", Path(__file__).parents[1] / "Summarize-PresentTiming.py")
timing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(timing)


class PresentTimingTests(unittest.TestCase):
    def setUp(self):
        self.header = {"schema": "dm-perf-frames-v5", "qpf": 1000,
                       "record_count": 2, "wrapped": False}
        self.rows = [
            {"seq": 1, "qpc": 100, "present_call": {
                "thread_id": 7, "original_begin_qpc": 110, "original_end_qpc": 111,
                "commit_begin_qpc": 112, "commit_lock_acquired_qpc": 113,
                "commit_recorded_qpc": 114}},
            {"seq": 2, "qpc": 140, "present_call": {
                "thread_id": 7, "entry_qpc": 130, "entry_lock_acquired_qpc": 139,
                "previous": {"seq": 1, "commit_return_qpc": 115, "hook_exit_qpc": 116}}},
        ]

    def test_partition_closes_original_anchor_interval(self):
        result = timing.summarize(self.header, self.rows)
        interval = result["slowest_intervals"][0]
        self.assertFalse(result["acceptance"])
        self.assertEqual(result["decomposed_intervals"], 1)
        self.assertEqual(interval["duration_ms"], 40)
        self.assertEqual(interval["diagnostic"]["sum_ms"], 40)
        self.assertEqual(interval["diagnostic"]["parts_ms"]["original_present"], 1)
        self.assertEqual(interval["diagnostic"]["parts_ms"]["between_hooks"], 14)
        self.assertEqual(interval["diagnostic"]["largest_interval"], "between_hooks")

    def test_missing_or_cross_thread_exit_never_looks_like_zero_work(self):
        for key, value in (("previous", None), ("thread_id", 8)):
            rows = copy.deepcopy(self.rows)
            rows[1]["present_call"][key] = value
            result = timing.summarize(self.header, rows)
            self.assertEqual(result["decomposed_intervals"], 0)
            self.assertFalse(result["slowest_intervals"][0]["diagnostic"]["available"])

    def test_nonconsecutive_or_nonmonotonic_diagnostics_are_unavailable(self):
        rows = copy.deepcopy(self.rows)
        rows[1]["seq"] = 3
        self.assertFalse(timing.decompose(*rows, 1000)["available"])
        rows = copy.deepcopy(self.rows)
        rows[1]["present_call"]["previous"]["hook_exit_qpc"] = 141
        self.assertFalse(timing.decompose(*rows, 1000)["available"])

    def test_legacy_data_keeps_interval_but_cannot_invent_attribution(self):
        self.header["schema"] = "dm-perf-frames-v4"
        for row in self.rows:
            row.pop("present_call")
        interval = timing.summarize(self.header, self.rows)["slowest_intervals"][0]
        self.assertEqual(interval["duration_ms"], 40)
        self.assertEqual(interval["diagnostic"]["reason"], "present_call_not_recorded")

    def test_corrupt_record_count_is_rejected(self):
        self.header["record_count"] = 3
        with self.assertRaises(ValueError):
            timing.summarize(self.header, self.rows)

    def test_synthetic_stalls_are_charged_to_their_actual_boundary(self):
        for part in ("original_present", "between_hooks", "next_entry_admission_and_lock_wait"):
            with self.subTest(part=part):
                rows = copy.deepcopy(self.rows)
                right = rows[1]["present_call"]
                if part == "original_present":
                    for key in ("original_end_qpc", "commit_begin_qpc", "commit_lock_acquired_qpc", "commit_recorded_qpc"):
                        rows[0]["present_call"][key] += 2000
                    for key in ("commit_return_qpc", "hook_exit_qpc"):
                        right["previous"][key] += 2000
                if part != "next_entry_admission_and_lock_wait":
                    right["entry_qpc"] += 2000
                right["entry_lock_acquired_qpc"] += 2000
                rows[1]["qpc"] += 2000
                detail = timing.decompose(*rows, 1000)
                self.assertTrue(detail["available"])
                self.assertEqual(detail["largest_interval"], part)
                self.assertEqual(detail["sum_ms"], 2040)


def v6_rows(count=1000):
    rows = []
    for seq in range(count):
        qpc = 1000 + seq * 100
        rows.append({"seq": seq, "qpc": qpc, "present_call": {
            "epoch": 3, "generation": 4, "swapchain_key": 5, "thread_id": seq % 23 + 1,
            "entry_qpc": qpc - 10, "entry_lock_acquired_qpc": qpc - 5,
            "original_begin_qpc": qpc + 1, "original_end_qpc": qpc + 2,
            "commit_begin_qpc": qpc + 3, "commit_lock_acquired_qpc": qpc + 4,
            "commit_recorded_qpc": qpc + 5,
            "exit": {"commit_return_qpc": qpc + 6, "hook_exit_qpc": qpc + 7}}})
    return rows


class CrossThreadTimingTests(unittest.TestCase):
    def setUp(self):
        self.rows = v6_rows(1000)
        self.header = {"schema": "dm-perf-frames-v6", "qpf": 1000,
                       "record_count": len(self.rows), "wrapped": False}

    def test_23_thread_rotation_is_fully_decomposed_including_last_exit(self):
        report = timing.summarize(self.header, self.rows)
        self.assertEqual(report["decomposed_intervals"], 999)
        self.assertEqual(report["decomposed_cross_thread_intervals"], 999)
        self.assertEqual(report["decomposition_coverage_percent"], 100)
        self.assertEqual(report["own_exit_records"], 1000)
        self.assertEqual(report["valid_call_boundary_records"], 1000)
        self.assertFalse(report["acceptance"])
        for item in report["slowest_intervals"]:
            self.assertEqual(item["duration_ms"], item["diagnostic"]["sum_ms"])

    def test_epoch_generation_swapchain_mismatch_never_gets_joined(self):
        for key in ("epoch", "generation", "swapchain_key"):
            rows = copy.deepcopy(self.rows[:2])
            rows[1]["present_call"][key] += 1
            self.assertEqual(timing.decompose(*rows, 1000)["reason"],
                             "epoch_generation_or_swapchain_mismatch")

    def test_v6_long_stall_in_each_of_ten_segments_closes_the_interval(self):
        labels = ("probe_pre_original", "original_present", "probe_post_original",
                  "commit_prepare_and_lock_wait", "commit_locked", "commit_unlock_and_return",
                  "hook_tail", "between_hooks", "next_entry_admission_and_lock_wait",
                  "next_anchor_after_lock")
        for index, label in enumerate(labels):
            with self.subTest(label=label):
                rows = copy.deepcopy(self.rows[:2])
                left, right = rows[0]["present_call"], rows[1]["present_call"]
                points = [(left, "original_begin_qpc"), (left, "original_end_qpc"),
                          (left, "commit_begin_qpc"), (left, "commit_lock_acquired_qpc"),
                          (left, "commit_recorded_qpc"), (left["exit"], "commit_return_qpc"),
                          (left["exit"], "hook_exit_qpc"), (right, "entry_qpc"),
                          (right, "entry_lock_acquired_qpc"), (rows[1], "qpc")]
                for obj, key in points[index:]:
                    obj[key] += 2000
                detail = timing.decompose(*rows, 1000)
                self.assertTrue(detail["available"])
                self.assertEqual(detail["largest_interval"], label)
                self.assertEqual(detail["sum_ms"], 2100)

    def test_overlap_keeps_call_evidence_without_fabricated_partition(self):
        rows = copy.deepcopy(self.rows[:2])
        rows[0]["present_call"]["exit"]["hook_exit_qpc"] = 1095
        report = timing.summarize({**self.header, "record_count": 2}, rows)
        self.assertEqual(report["unavailable_reasons"], {"overlapping_present_calls": 1})
        self.assertEqual(report["valid_call_boundary_records"], 2)

    def test_missing_exit_and_commit_order_are_explicit_not_silently_sorted(self):
        self.rows[0]["present_call"]["exit"] = None
        report = timing.summarize(self.header, self.rows)
        self.assertEqual(report["unavailable_reasons"], {"exit_not_recorded": 1})
        self.rows[5], self.rows[6] = self.rows[6], self.rows[5]
        report = timing.summarize(self.header, self.rows)
        self.assertEqual(report["unavailable_reasons"]["nonincreasing_anchor_order"], 1)

    def test_exact_ack_window_excludes_settle_and_checks_pairing(self):
        ack = {"identity": {"generation": 4}, "ack": {"type": "flushed", "summary": {
            "present_timing": {"qpc_start": 1200, "qpc_end": 1400,
                               "qpc_frequency": 1000, "sample_count": 3}}}}
        report = timing.summarize(self.header, self.rows, ack=ack)
        self.assertEqual(report["record_count"], 3)
        self.assertEqual(report["decomposed_intervals"], 2)
        ack["identity"]["generation"] = 9
        with self.assertRaisesRegex(ValueError, "generation"):
            timing.summarize(self.header, self.rows, ack=ack)
        ack["identity"]["generation"] = 4
        ack["ack"]["summary"]["present_timing"]["sample_count"] = 4
        with self.assertRaisesRegex(ValueError, "fully witnessed"):
            timing.summarize(self.header, self.rows, ack=ack)

    def test_slow_hooks_are_approximate_not_added_or_claimed_as_cause(self):
        slow = ({"schema": "dm-perf-slow-hooks-v1", "qpf": 1000, "epoch": 3,
                 "record_count": 2, "dropped_since_reset": 1}, [
            {"kind": "descriptor_copy", "observed_end_qpc": 1080, "thread_id": 2,
             "duration_ns": 20_000_000, "alignment_uncertainty_ns": 1_000_000},
            {"kind": "execute_prepare", "observed_end_qpc": 1070, "thread_id": 3,
             "duration_ns": 30_000_000, "alignment_uncertainty_ns": 2_000_000}])
        report = timing.summarize(self.header, self.rows, slow_hooks=slow)
        overlap = report["slowest_intervals"][0]["slow_hooks"]
        self.assertEqual(overlap["possible_overlap_count"], 2)
        self.assertEqual(overlap["largest_events"][0]["possible_start_qpc"], 1038)
        self.assertNotIn("total_ms", overlap)
        self.assertEqual(report["slow_hook_evidence"]["dropped_since_reset"], 1)
        slow[0]["epoch"] = 8
        with self.assertRaisesRegex(ValueError, "epoch"):
            timing.summarize(self.header, self.rows, slow_hooks=slow)

    def test_single_row_still_validates_identity_and_has_no_fake_coverage(self):
        report = timing.summarize({**self.header, "record_count": 1}, self.rows[:1])
        self.assertIsNone(report["decomposition_coverage_percent"])
        with self.assertRaises(ValueError):
            timing.summarize({**self.header, "record_count": 1}, [{"seq": 1, "qpc": False}])

    def test_v2_prepare_children_are_preserved_and_validated_not_summed_twice(self):
        detail = dict.fromkeys(timing.PREPARE_TIMES + timing.PREPARE_COUNTS, 0)
        detail.update(total_ns=20_000_000, outer_lock_held_ns=19_000_000,
                      memory_read_ns=12_000_000, validation_ns=5_000_000,
                      descriptor_wait_ns=4_000_000, residual_ns=3_000_000,
                      requests=2, live_requests=2, read_attempts=1, reused_snapshots=1,
                      read_bytes=256, reused_bytes=256, wide_loads=32)
        event = {"kind": "execute_prepare", "observed_end_qpc": 1080, "thread_id": 2,
                 "duration_ns": 20_000_000, "alignment_uncertainty_ns": 100, "execute_prepare": detail}
        slow = ({"schema": "dm-perf-slow-hooks-v2", "qpf": 1000, "epoch": 3,
                 "record_count": 1, "dropped_since_reset": 0}, [event])
        report = timing.summarize(self.header, self.rows, slow_hooks=slow)
        sample = report["slowest_intervals"][0]["slow_hooks"]["largest_events"][0]
        self.assertEqual(sample["prepare_times_ms"]["memory_read"], 12)
        self.assertEqual(sample["prepare_times_ms"]["descriptor_wait"], 4)
        self.assertIn("subsets", sample["prepare_note"])
        self.assertNotIn("memory_query_calls", sample["execute_prepare"])
        for key, bad in [("total_ns", 19_000_000), ("read_bytes", 255),
                         ("descriptor_wait_ns", 6_000_000), ("requests", True)]:
            before = detail[key]
            detail[key] = bad
            with self.assertRaises(ValueError):
                timing.summarize(self.header, self.rows, slow_hooks=slow)
            detail[key] = before

    def test_v3_query_coverage_preserves_hits_and_uncacheable_reads(self):
        detail = dict.fromkeys(timing.PREPARE_TIMES + timing.PREPARE_COUNTS + timing.QUERY_COUNTS, 0)
        detail.update(total_ns=20_000_000, residual_ns=20_000_000,
                      requests=8, live_requests=8, read_attempts=7, reused_snapshots=1,
                      read_rejected=1, read_bytes=1536, wide_loads=192, unflagged_reads=6,
                      memory_query_calls=5, memory_query_rejected=1, query_cache_hits=2,
                      query_cache_ineligible_reads=2, query_cache_inserts=1, query_cache_full_reads=1)
        event = {"kind": "execute_prepare", "observed_end_qpc": 1080, "thread_id": 2,
                 "duration_ns": 20_000_000, "alignment_uncertainty_ns": 100, "execute_prepare": detail}
        slow = ({"schema": "dm-perf-slow-hooks-v3", "qpf": 1000, "epoch": 3,
                 "record_count": 1, "dropped_since_reset": 0}, [event])
        report = timing.summarize(self.header, self.rows, slow_hooks=slow)
        sample = report["slowest_intervals"][0]["slow_hooks"]["largest_events"][0]
        self.assertEqual(sample["execute_prepare"], detail)
        for key, bad in [("memory_query_calls", 6), ("memory_query_rejected", 7),
                         ("query_cache_hits", -1), ("query_cache_hits", True),
                         ("query_cache_ineligible_reads", 4), ("query_cache_inserts", 4),
                         ("query_cache_full_reads", 5), ("unflagged_reads", 5)]:
            with self.subTest(key=key, bad=bad):
                before = detail[key]
                detail[key] = bad
                with self.assertRaises(ValueError):
                    timing.summarize(self.header, self.rows, slow_hooks=slow)
                detail[key] = before
        for key in timing.QUERY_COUNTS:
            before = detail.pop(key)
            with self.assertRaises(ValueError):
                timing.summarize(self.header, self.rows, slow_hooks=slow)
            detail[key] = before

    def test_v3_read_rejected_before_query_is_not_a_query_failure(self):
        detail = dict.fromkeys(timing.PREPARE_TIMES + timing.PREPARE_COUNTS + timing.QUERY_COUNTS, 0)
        detail.update(total_ns=1, residual_ns=1, requests=1, live_requests=1,
                      read_attempts=1, read_rejected=1)
        timing.validate_prepare_detail({"schema": "dm-perf-slow-hooks-v3"},
                                       {"kind": "execute_prepare", "duration_ns": 1, "execute_prepare": detail})


if __name__ == "__main__":
    unittest.main()

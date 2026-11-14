"""Synthetic observations only; no game, GPU, capture files, or retiming."""
import copy
import importlib.util
import itertools
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('replay',Path(__file__).resolve().parents[1]/'Replay-CadenceSelection.py')
replay=importlib.util.module_from_spec(spec);spec.loader.exec_module(replay)


def fixture(first_has_depth=True, second_has_depth=True):
    header={'kind':'header','schema':'capture-cadence-trace/v2','raw_watermark':{'source_width':1920,'source_height':1080},'context':{'qpc_frequency':10000}}
    slot={'run_frame':0,'start_qpc':0,'end_qpc':100,'seal_qpc':104}
    samples=[{'kind':'raw_watermark','source_seq':i,'source_qpc':q,'status':'decoded_low16_only','raw_present_low16':p}
             for i,q,p in [(1,10,7),(2,20,8)]]
    observations=[{'kind':'observe','slot':slot,'source_seq':i,'source_qpc':q,'future':False,'replaced_source_seq':old}
                  for i,q,old in [(1,10,None),(2,20,1)]]
    trace=[header,*samples,*observations,{'kind':'seal','slot':slot,'source_seq':2,'source_qpc':20,'duplicate':False,'superseded':1},
           {'kind':'output_submitted','run_frame':0,'source_seq':2},
           {'kind':'footer','complete':True,'dropped_events':0,'clock_errors':0}]
    depth=[{'present_seq':p,'present_qpc':q} for p,q,keep in [(7,5,first_has_depth),(8,15,second_has_depth)] if keep]
    depth.append({'present_seq':9,'present_qpc':1000})
    return trace,depth


class ReplayTests(unittest.TestCase):
    def test_two_depth_frames_in_one_slot_are_a_capacity_conflict(self):
        result=replay.analyze(*fixture())
        self.assertEqual(result['recorded_matching_depth_identities'],1)
        self.assertEqual(result['known_depth_priority_counterfactual_upper_bound'],1)
        self.assertEqual(result['minimum_distinct_depth_identities_unretained'],1)
        self.assertEqual(result['counterfactual_additional_matches_at_most'],0)

    def test_priority_can_help_only_when_replacement_lacks_depth(self):
        result=replay.analyze(*fixture(second_has_depth=False))
        self.assertEqual(result['counterfactual_additional_matches_at_most'],1)
        self.assertEqual(result['replacement_counts'],{'depth_replaced_by_no_depth':1})

    def test_latest_can_be_the_frame_that_has_depth(self):
        result=replay.analyze(*fixture(first_has_depth=False))
        self.assertEqual(result['counterfactual_additional_matches_at_most'],0)

    def test_matching_does_not_double_count_aliases_or_use_greedy_underbound(self):
        self.assertEqual(replay.maximum_distinct_selection([{1,2},{1}]),2)
        self.assertEqual(replay.maximum_distinct_selection([{1},{1},{1}]),1)
        self.assertEqual(replay.maximum_distinct_selection([set(),{2},set()]),1)

    def test_matching_agrees_with_exhaustive_small_assignments(self):
        subsets=[{i for i in range(3) if mask&(1<<i)} for mask in range(8)]
        for windows in itertools.product(subsets,repeat=3):
            expected=max(len(set(choice)-{None}) for choice in itertools.product(*([None,*w] for w in windows)))
            self.assertEqual(replay.maximum_distinct_selection(windows),expected)

    def test_real_replacement_and_submit_identities_are_required(self):
        for index,key,value in [(4,'replaced_source_seq',None),(5,'source_seq',1),(6,'source_seq',1)]:
            trace,depth=fixture();trace[index][key]=value
            with self.subTest(index=index),self.assertRaises(ValueError):replay.analyze(trace,depth)

    def test_future_pixels_cannot_be_moved_into_a_previous_slot(self):
        trace,depth=fixture();trace[4]['source_qpc']=100
        with self.assertRaises(ValueError):replay.analyze(trace,depth)

    def test_loss_or_missing_footer_precludes_full_replay(self):
        trace,depth=fixture()
        for footer in [{'kind':'footer','complete':False},{'kind':'footer','complete':True,'dropped_events':1},{'kind':'footer','complete':True,'clock_errors':1},{'kind':'observe'}]:
            altered=copy.deepcopy(trace);altered[-1]=footer
            with self.subTest(footer=footer),self.assertRaises(ValueError):replay.analyze(altered,depth)

    def test_low16_alias_and_nonmonotonic_original_rows_are_rejected(self):
        trace,depth=fixture()
        for rows in [depth+[{'present_seq':65543,'present_qpc':2000}],list(reversed(depth))]:
            with self.assertRaises(ValueError):replay.analyze(trace,rows)

    def test_known_pixels_outside_measured_qpc_window_are_not_matched(self):
        trace,depth=fixture();trace[1]['source_qpc']=1000010;trace[2]['source_qpc']=1000020
        trace[3]['source_qpc']=1000010;trace[4]['source_qpc']=1000020
        trace[5]['source_qpc']=1000020
        trace[3]['slot']['start_qpc']=1000000;trace[3]['slot']['end_qpc']=1000100
        result=replay.analyze(trace,depth)
        self.assertEqual(result['recorded_matching_depth_identities'],0)
        self.assertEqual(result['known_depth_priority_counterfactual_upper_bound'],0)


if __name__=='__main__':unittest.main()

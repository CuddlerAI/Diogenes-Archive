"""The coarse event vocabulary keeps only distinctions annotators can make.

Measured on 300 frames labelled independently by Qwen3-VL-235B and Gemini
2.5 Flash: 62.3% agreement on the 9-class vocabulary, 81.7% after merging.
The merged pairs are the ones a single frame cannot settle — most starkly
idle vs movement, since a static image contains no motion information.
"""
from diogenes_vl.schema import (COARSE_EVENT_CLASSES, EVENT_CLASSES,
                                coarse_event)


def test_every_fine_class_maps_somewhere_valid():
    for c in EVENT_CLASSES:
        assert coarse_event(c) in COARSE_EVENT_CLASSES, c


def test_frame_indistinguishable_pairs_are_merged():
    # a single frame shows no motion, so these cannot be separated
    assert coarse_event("idle") == coarse_event("movement")
    # a load screen and a menu are both non-gameplay UI
    assert coarse_event("loading") == coarse_event("menu_ui")
    # one frame rarely shows who died
    assert coarse_event("combat_kill") == coarse_event("combat_death")
    # "moving toward an objective" is both
    assert coarse_event("objective") == coarse_event("movement")


def test_distinctions_worth_keeping_survive():
    assert coarse_event("combat_kill") != coarse_event("movement")
    assert coarse_event("menu_ui") != coarse_event("movement")
    assert coarse_event("dialogue") != coarse_event("movement")
    assert len(set(COARSE_EVENT_CLASSES)) == 4


def test_unknown_class_passes_through_unchanged():
    assert coarse_event("something_new") == "something_new"

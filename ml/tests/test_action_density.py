"""Chunks without action signal must be dropped, and visibly so.

A gaming-500 probe found whole titles at ~0% keyboard activity — gamepad
games and mouse-only strategy. Their frames pair fine pixels with an
all-zero label, so an IDM trained on them learns to predict nothing while
the key metric quietly reports class imbalance as accuracy.
"""
import json

import pytest

from diogenes_phase0.data.dataset import (action_density,
                                          filter_by_action_density,
                                          scan_dataset)
from diogenes_phase0.data.synthetic import generate_dataset


@pytest.fixture(scope="module")
def records(tmp_path_factory):
    root = tmp_path_factory.mktemp("d")
    generate_dataset(root, games=["synth_a"], sessions_per_game=1,
                     chunks_per_session=2, chunk_frames=40, res=48, seed=2)
    return scan_dataset(root)


def _blank_out_keys(record):
    """Rewrite a chunk's actions so no key is ever held."""
    rows = [json.loads(x) for x in open(record.actions_path) if x.strip()]
    for r in rows:
        r["keys"] = []
    with open(record.actions_path, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")


def test_density_reflects_actual_key_presses(records):
    d = action_density(records[0])
    assert d["n"] == 40
    assert 0.0 < d["key_frac"] <= 1.0     # synthetic world holds keys often


def test_silent_chunk_is_dropped_and_named(records, tmp_path_factory):
    import copy
    import shutil
    root = tmp_path_factory.mktemp("d2")
    generate_dataset(root, games=["gamepad_only"], sessions_per_game=1,
                     chunks_per_session=2, chunk_frames=40, res=48, seed=4)
    recs = scan_dataset(root)
    for r in recs:
        _blank_out_keys(r)
    kept, stats = filter_by_action_density(recs, min_key_frac=0.10)
    assert kept == []
    assert stats["dropped"] == 2
    # the dropped title is reported, not silently missing
    assert "gamepad_only" in stats["dropped_games"]
    assert stats["dropped_games"]["gamepad_only"] == 0.0
    _ = copy, shutil


def test_active_chunks_survive(records):
    kept, stats = filter_by_action_density(records, min_key_frac=0.10)
    assert len(kept) == len(records) and stats["dropped"] == 0


def test_threshold_is_respected(records):
    # an impossible bar drops everything
    kept, stats = filter_by_action_density(records, min_key_frac=1.01)
    assert kept == [] and stats["dropped"] == len(records)


def test_too_short_chunk_is_dropped(records):
    kept, _ = filter_by_action_density(records, min_key_frac=0.0,
                                       min_frames=10_000)
    assert kept == []

import json

import pytest

from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_vl.bench import build_bench, load_manifest
from diogenes_vl.schema import validate_observation
from diogenes_vl.teacher import annotate, stub_observation

pytest.importorskip("PIL")


@pytest.fixture(scope="module")
def bench(tmp_path_factory):
    root = tmp_path_factory.mktemp("synth")
    generate_dataset(root, games=["synth_a", "synth_b"], sessions_per_game=2,
                     chunks_per_session=1, chunk_frames=40, res=48, seed=5)
    out = tmp_path_factory.mktemp("bench")
    return build_bench([root], out, per_game=6, frame_stride=10, val_fraction=0.5,
                       skip_first_s=0)  # synthetic sessions have no launcher head


def test_bench_layout(bench):
    rows = load_manifest(bench)
    assert rows and len(rows) <= 12  # <= per_game per game
    games = {r["game"] for r in rows}
    assert games == {"synth_a", "synth_b"}
    for r in rows[:3]:
        assert (bench / r["image"]).exists()
        assert r["split"] in ("val", "test")
    info = json.loads((bench / "bench_info.json").read_text())
    assert info["n_games"] == 2


def test_split_is_session_disjoint(bench):
    by_session = {}
    for r in load_manifest(bench):
        by_session.setdefault(r["session"], set()).add(r["split"])
    assert all(len(s) == 1 for s in by_session.values())


def test_stub_teacher_valid_and_deterministic(bench):
    rows = load_manifest(bench)
    for r in rows:
        obs = stub_observation(r)
        validate_observation(obs)
        assert obs["game"] == r["game"]
    fire = stub_observation({**rows[0], "keys": [1, 87]})
    assert fire["event_class"] == "combat_kill"
    idle = stub_observation({**rows[0], "keys": []})
    assert idle["event_class"] == "idle"


def test_annotate_writes_labels(bench, tmp_path):
    out = tmp_path / "labels.jsonl"
    stats = annotate(bench, "stub", out)
    assert stats["dropped_invalid"] == 0
    rows = [json.loads(x) for x in open(out)]
    assert stats["labeled"] == len(rows) > 0
    assert all("observation" in r and r["teacher"] == "stub" for r in rows)

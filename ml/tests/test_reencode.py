"""Re-encoding may change pixels and nothing else.

This tool exists to measure what the codec costs a trained head. That
measurement is only valid if the labels, the split structure and the frame
count come through untouched — otherwise a metric drop could be relabeling
rather than compression, which is precisely the confusion it is meant to
resolve.
"""
import json

import pytest

from diogenes_phase0.data.dataset import scan_dataset
from diogenes_phase0.data.reencode import reencode_dataset
from diogenes_phase0.data.synthetic import generate_dataset

pytest.importorskip("cv2")


def test_labels_and_structure_survive_reencoding(tmp_path):
    src = generate_dataset(tmp_path / "src", games=["synth_a"],
                           sessions_per_game=1, chunks_per_session=2,
                           chunk_frames=40, res=48, seed=5)
    dst = tmp_path / "dst"
    stats = reencode_dataset(src, dst, codec="mp4")

    src_recs = sorted(scan_dataset(src), key=lambda r: str(r.chunk_dir))
    dst_recs = sorted(scan_dataset(dst), key=lambda r: str(r.chunk_dir))
    assert stats["chunks"] == len(src_recs) == len(dst_recs)

    for a, b in zip(src_recs, dst_recs):
        assert a.n_frames == b.n_frames
        # labels byte-identical — the whole point
        assert (a.chunk_dir / "p0.jsonl").read_bytes() == \
               (b.chunk_dir / "p0.jsonl").read_bytes()
        assert a.meta.game == b.meta.game
        assert a.chunk_dir.relative_to(src) == b.chunk_dir.relative_to(dst)


def test_meta_reports_the_new_codec_not_the_old(tmp_path):
    src = generate_dataset(tmp_path / "src", games=["synth_b"],
                           sessions_per_game=1, chunks_per_session=1,
                           chunk_frames=40, res=48, seed=6)
    dst = tmp_path / "dst"
    reencode_dataset(src, dst, codec="mp4")
    meta = json.loads(next(dst.rglob("meta.json")).read_text())
    assert meta["video"]["file"] == "p0.mp4"
    assert meta["video"]["codec"] in ("h264", "mp4v")
    assert meta["video"]["codec"] != "npz"


def test_frames_still_load_and_shape_matches(tmp_path):
    from diogenes_phase0.data.dataset import _load_frames

    src = generate_dataset(tmp_path / "src", games=["synth_c"],
                           sessions_per_game=1, chunks_per_session=1,
                           chunk_frames=40, res=48, seed=7)
    dst = tmp_path / "dst"
    reencode_dataset(src, dst, codec="mp4")
    a = _load_frames(scan_dataset(src)[0])
    b = _load_frames(scan_dataset(dst)[0])
    assert a.shape == b.shape
    assert b.dtype == a.dtype


def test_limit_probes_a_subset_without_touching_the_rest(tmp_path):
    src = generate_dataset(tmp_path / "src", games=["synth_d"],
                           sessions_per_game=1, chunks_per_session=3,
                           chunk_frames=30, res=32, seed=8)
    dst = tmp_path / "dst"
    stats = reencode_dataset(src, dst, codec="mp4", limit=1)
    assert stats["chunks"] == 1
    assert len(scan_dataset(dst)) == 1

"""The two pieces of D-3 that can be wrong without anything raising.

`replay_events` turns the external model's press/release stream into the
held-key SET our labels are written in, and `KeyStats` reproduces
`evaluate.run_eval`'s definitions per chunk so the aggregate can be
bootstrapped. A bug in either produces a plausible number, which is the worst
kind: doc 26 §2.5 already lost part of a correlation to a one-frame offset that
never raised.

The model itself is not exercised here — `predict.py` runs in its own uv
environment against a 1B checkpoint. What is exercised is everything between
its output and a reported metric.
"""
import json

from diogenes_phase0.ext_idm.metrics import (KeyStats, _states_run_mode,
                                             load_chunk_records, replay_events)
from diogenes_phase0.ext_idm.predict import runs_from_chunks
from diogenes_phase0.keys import SCANNED_VKEYS, keys_to_multihot

W, A, LMB, RMB = 87, 65, 1, 2
FPS = 30.0
STEP = 1e9 / FPS


def kb(ts, vk, kind):
    return {"topic": "keyboard", "timestamp": int(ts), "vk": vk,
            "event_type": kind}


def mouse(ts, dx=0, dy=0, flags=0):
    return {"topic": "mouse/raw", "timestamp": int(ts), "last_x": dx,
            "last_y": dy, "button_flags": flags}


def test_press_then_release_spans_the_right_frames():
    # pressed just after frame 1, released just after frame 3
    evs = [kb(1.5 * STEP, W, "press"), kb(3.5 * STEP, W, "release")]
    keys, _ = replay_events(evs, 6, FPS)
    held = [bool(k[SCANNED_VKEYS.index(W)]) for k in keys]
    assert held == [False, False, True, True, False, False]


def test_frame_owns_the_interval_that_produced_it():
    """A frame's action is the input in (t_prev, t] — d2e.build_rows' rule.

    An event landing exactly on frame 2's timestamp belongs to frame 2, not
    frame 3. Off by one here and every metric shifts a frame while still
    printing a number.
    """
    keys, _ = replay_events([kb(2 * STEP, W, "press")], 4, FPS)
    assert keys[2][SCANNED_VKEYS.index(W)] == 1
    assert keys[1][SCANNED_VKEYS.index(W)] == 0


def test_everything_before_frame_zero_folds_into_frame_zero():
    keys, _ = replay_events([kb(-5 * STEP, W, "press")], 3, FPS)
    assert all(k[SCANNED_VKEYS.index(W)] == 1 for k in keys)


def test_seed_supplies_the_state_no_event_can():
    """A key held since before the chunk has no press event anywhere in it."""
    evs = [kb(2.5 * STEP, W, "release")]
    unseeded, _ = replay_events(evs, 5, FPS)
    seeded, _ = replay_events(evs, 5, FPS, seed_keys=[W])
    i = SCANNED_VKEYS.index(W)
    assert [k[i] for k in unseeded] == [0, 0, 0, 0, 0]
    assert [k[i] for k in seeded] == [1, 1, 1, 0, 0]


def test_shift_moves_events_earlier():
    """Upstream's --time-shift semantics: a positive shift pulls actions back.

    3.2 frames shifted back by 2 lands at 1.2, which belongs to frame 2's
    interval (1, 2] — not frame 1's. The frame the shift lands ON is the one
    that changes, which is the whole reason the shift is swept rather than
    assumed.
    """
    evs = [kb(3.2 * STEP, W, "press")]
    i = SCANNED_VKEYS.index(W)
    late, _ = replay_events(evs, 6, FPS, shift_ns=0)
    early, _ = replay_events(evs, 6, FPS, shift_ns=int(2 * STEP))
    assert [k[i] for k in late] == [0, 0, 0, 0, 1, 1]
    assert [k[i] for k in early] == [0, 0, 1, 1, 1, 1]


def test_button_flags_map_to_mouse_vks():
    evs = [mouse(0.5 * STEP, flags=0x0001),   # LMB down
           mouse(2.5 * STEP, flags=0x0004),   # RMB down
           mouse(3.5 * STEP, flags=0x0002)]   # LMB up
    keys, _ = replay_events(evs, 5, FPS)
    l, r = SCANNED_VKEYS.index(LMB), SCANNED_VKEYS.index(RMB)
    assert [k[l] for k in keys] == [0, 1, 1, 1, 0]
    assert [k[r] for k in keys] == [0, 0, 0, 1, 1]


def test_deltas_accumulate_per_frame():
    evs = [mouse(0.5 * STEP, dx=3, dy=-1), mouse(0.9 * STEP, dx=4, dy=2),
           mouse(2.5 * STEP, dx=-7)]
    _, deltas = replay_events(evs, 4, FPS)
    assert deltas == [(0, 0), (7, 1), (0, 0), (-7, 0)]


def test_keys_outside_the_vocabulary_are_dropped_not_mapped():
    """LShift (0xA0) is captured by D2E and absent from SCANNED_VKEYS."""
    keys, _ = replay_events([kb(0.5 * STEP, 0xA0, "press")], 3, FPS)
    assert all(sum(k) == 0 for k in keys)


def test_unrecognised_event_type_is_ignored_rather_than_guessed():
    keys, _ = replay_events([kb(0.5 * STEP, W, "wiggle")], 3, FPS)
    assert all(sum(k) == 0 for k in keys)


# --- KeyStats -------------------------------------------------------------


def test_always_on_macro_f1_is_the_2p_over_1_plus_p_baseline():
    """The trivial baseline doc 26 quotes — derived, then checked."""
    n, pos = 100, 40
    st = KeyStats()
    on = [1] * len(SCANNED_VKEYS)
    for i in range(n):
        st.add(on, keys_to_multihot([W] if i < pos else []))
    rep = st.report()
    p = pos / n
    assert rep["key_n_seen"] == 1
    assert abs(rep["key_macro_f1"] - 2 * p / (1 + p)) < 1e-12
    assert rep["key_exact"] == 0.0


def test_macro_f1_averages_only_over_keys_the_split_contains():
    st = KeyStats()
    for _ in range(10):
        st.add(keys_to_multihot([W]), keys_to_multihot([W]))
    rep = st.report()
    assert rep["key_n_seen"] == 1              # not 12
    assert rep["key_macro_f1"] == 1.0
    assert rep["key_exact"] == 1.0


def test_exact_match_is_over_the_whole_key_set():
    st = KeyStats()
    st.add(keys_to_multihot([W]), keys_to_multihot([W, A]))
    st.add(keys_to_multihot([W, A]), keys_to_multihot([W, A]))
    rep = st.report()
    assert rep["key_exact"] == 0.5


def test_merge_is_the_same_as_scoring_the_union():
    parts = []
    whole = KeyStats()
    for chunk in range(3):
        st = KeyStats()
        for i in range(7):
            pred = keys_to_multihot([W] if (i + chunk) % 2 else [A])
            true = keys_to_multihot([W] if i % 3 else [A, LMB])
            st.add(pred, true)
            whole.add(pred, true)
        parts.append(st)
    assert KeyStats.merge(parts).report() == whole.report()


def test_restrict_to_makes_two_slices_comparable():
    """Slices of one split must average over the same key set.

    Without it, a slice that happens to contain no digit press divides by a
    smaller denominator and scores higher for a reason that has nothing to do
    with the model — which is exactly the trap the by-chunk-third ablation
    would fall into.
    """
    split_keys = [SCANNED_VKEYS.index(W), SCANNED_VKEYS.index(A)]
    slice_ = KeyStats()
    for _ in range(4):                      # this slice only ever sees W
        slice_.add(keys_to_multihot([W]), keys_to_multihot([W]))
    assert slice_.report()["key_macro_f1"] == 1.0            # 1 key, perfect
    assert slice_.report(restrict_to=split_keys)["key_macro_f1"] == 0.5  # A: F1 0


# ---------------------------------------------------------------------------
# continuous decoding (research/30 D-3-b)
# ---------------------------------------------------------------------------


def _chunk(tmp_path, name, gframe0, n=4, session="s1", game="g", keys0=()):
    d = tmp_path / game / session / name
    d.mkdir(parents=True)
    with open(d / "p0.jsonl", "w") as f:
        for j in range(n):
            f.write(json.dumps({"frame": j, "gframe": gframe0 + j,
                                "keys": list(keys0) if j == 0 else [],
                                "pointerDx": 0, "pointerDy": 0,
                                "pointerBins": [0, 0]}) + "\n")
    (d / "meta.json").write_text(json.dumps({
        "sample_key": name, "game": game, "rights": "third_party",
        "first_party": False, "consent_id": None, "is_trainable": False,
        "is_cutscene": False, "video": {"fps": 30.0},
    }))
    return d


def test_runs_split_where_the_grid_jumps(tmp_path):
    """Chunk numbering is not contiguity — dropped chunks leave a gframe gap."""
    a = _chunk(tmp_path, "c0000", 0)
    b = _chunk(tmp_path, "c0001", 4)      # contiguous with a
    c = _chunk(tmp_path, "c0002", 40)     # a stall was dropped in between
    runs = runs_from_chunks([a, b, c])
    assert [[p.name for p, _, _ in r] for r in runs] == \
        [["c0000", "c0001"], ["c0002"]]


def test_runs_never_span_two_sessions(tmp_path):
    a = _chunk(tmp_path, "c0000", 0, session="s1")
    b = _chunk(tmp_path, "c0000", 4, session="s2")   # same grid, other session
    runs = runs_from_chunks([a, b])
    assert len(runs) == 2


def test_run_mode_carries_state_across_a_chunk_boundary(tmp_path):
    """The whole point: a press in chunk 0 is still held in chunk 1.

    Under the per-chunk protocol the second chunk sees no press event at all and
    reads as 'not pressed'; under continuous decoding it inherits the state.
    """
    a = _chunk(tmp_path, "c0000", 0)
    b = _chunk(tmp_path, "c0001", 4)
    pred = tmp_path / "preds"
    pred.mkdir()
    # W pressed early in chunk 0, never released
    (pred / "run__g__s1__c0000-c0001.json").write_text(json.dumps({
        "run_id": "r", "n_frames_total": 8, "fps": 30.0,
        "members": [{"chunk": str(a), "n_frames": 4, "offset": 0},
                    {"chunk": str(b), "n_frames": 4, "offset": 4}],
        "events": [kb(int(0.5 * 1e9 / 30), 87, "press")],
    }))
    recs = load_chunk_records([a, b])
    states, blobs = _states_run_mode(recs, pred, shift_ms=0, seed_mode="none")
    assert len(blobs) == 1
    w = SCANNED_VKEYS.index(87)
    keys_a, keys_b = states[str(a)][0], states[str(b)][0]
    assert [k[w] for k in keys_a] == [0, 1, 1, 1]
    assert [k[w] for k in keys_b] == [1, 1, 1, 1]   # inherited, no re-seed


def test_run_mode_seeds_once_at_the_run_start_not_per_chunk(tmp_path):
    """seed=gt in run mode is 12 bits per RUN, and it must survive the boundary.

    W is held at the run's first frame and released mid-way through chunk 1.
    Re-seeding per chunk would make chunk 1 start from its own (empty) frame-0
    truth and lose the hold; not seeding at all would never press W.
    """
    a = _chunk(tmp_path, "c0000", 0, keys0=(87,))
    b = _chunk(tmp_path, "c0001", 4)
    pred = tmp_path / "preds"
    pred.mkdir()
    (pred / "run__g__s1__c0000-c0001.json").write_text(json.dumps({
        "run_id": "r", "n_frames_total": 8, "fps": 30.0,
        "members": [{"chunk": str(a), "n_frames": 4, "offset": 0},
                    {"chunk": str(b), "n_frames": 4, "offset": 4}],
        "events": [kb(int(5.5 * 1e9 / 30), 87, "release")],
    }))
    recs = load_chunk_records([a, b])
    states, _ = _states_run_mode(recs, pred, shift_ms=0, seed_mode="gt")
    w = SCANNED_VKEYS.index(87)
    assert [k[w] for k in states[str(a)][0]] == [1, 1, 1, 1]
    assert [k[w] for k in states[str(b)][0]] == [1, 1, 0, 0]   # released at t=6

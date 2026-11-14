"""Event→state replay and the per-chunk metric accumulator.

Split out of `score.py` so it carries no torch import: this is the logic that
can be wrong while still printing a plausible number, so it has to be testable
without a GPU environment (`tests/test_ext_idm_score.py`).

The metric definitions here are `evaluate.run_eval`'s, restated per chunk so
the aggregate can be bootstrapped over chunks. `score.py` checks the two
against each other on every run and refuses to write a report if they diverge.
"""
from __future__ import annotations

import random

import json
from pathlib import Path

from ..contract import ChunkMeta, ChunkRecord, load_actions
from ..data.d2e import _BTN_FLAGS
from ..keys import N_KEYS, SCANNED_VKEYS, keys_to_multihot

VOCAB = set(SCANNED_VKEYS)


class KeyStats:
    """tp/fp/fn per key + exact-set matches, over a set of windows."""

    def __init__(self) -> None:
        self.tp = [0] * N_KEYS
        self.fp = [0] * N_KEYS
        self.fn = [0] * N_KEYS
        self.exact = 0
        self.n = 0

    def add(self, pred: list[int], true: list[int]) -> None:
        self.n += 1
        if pred == true:
            self.exact += 1
        for i in range(N_KEYS):
            p, t = pred[i], true[i]
            if p and t:
                self.tp[i] += 1
            elif p and not t:
                self.fp[i] += 1
            elif t and not p:
                self.fn[i] += 1

    @staticmethod
    def merge(parts: list["KeyStats"]) -> "KeyStats":
        out = KeyStats()
        for p in parts:
            out.n += p.n
            out.exact += p.exact
            for i in range(N_KEYS):
                out.tp[i] += p.tp[i]
                out.fp[i] += p.fp[i]
                out.fn[i] += p.fn[i]
        return out

    def seen_keys(self) -> list[int]:
        return [i for i in range(N_KEYS) if self.tp[i] + self.fn[i] > 0]

    def report(self, restrict_to: list[int] | None = None) -> dict:
        """`restrict_to` fixes the key set the macro average runs over.

        Needed whenever two KeyStats are compared over different window
        subsets: macro F1 divides by the number of keys THAT subset happens to
        contain, so the first third of a chunk and the last third can produce
        different denominators and the difference reads as a model effect.
        Pass the parent split's key set to make the slices comparable.
        """
        if not self.n:
            return {"n": 0}
        f1, support = [], []
        for i in range(N_KEYS):
            denom = 2 * self.tp[i] + self.fp[i] + self.fn[i]
            f1.append(2 * self.tp[i] / denom if denom else 0.0)
            support.append(self.tp[i] + self.fn[i])
        # "seen" = keys with a positive frame in THIS split, matching
        # run_eval's `support > 0`. Averaging over all 12 instead would let a
        # split that never presses key 6 drag every macro F1 down by 1/12.
        seen = ([i for i in range(N_KEYS) if support[i] > 0]
                if restrict_to is None else list(restrict_to))
        # run_eval's diagnostic: the same average over keys with support above
        # 1% of frames. Macro F1 conflates a head that collapsed to the
        # dominant key with a head that learned everything the data can teach
        # while a long tail of near-absent keys sits at F1~0 — both land near
        # 0.3, and only the second is fixed by more data. Reading the pair
        # separates them.
        common = [i for i in range(N_KEYS) if support[i] >= 0.01 * self.n]
        return {
            "n": self.n,
            "key_exact": self.exact / self.n,
            "key_macro_f1": (sum(f1[i] for i in seen) / len(seen)) if seen else None,
            "key_macro_f1_common": ((sum(f1[i] for i in common) / len(common))
                                    if common else None),
            "key_n_seen": len(seen),
            "key_n_common": len(common),
            "key_support": {str(SCANNED_VKEYS[i]): support[i] for i in seen},
            "per_key_f1": {str(SCANNED_VKEYS[i]): round(f1[i], 4) for i in seen},
        }


def bootstrap(per_chunk: list[KeyStats], *, n_boot: int = 1000,
              seed: int = 0) -> dict:
    """Percentile CI over CHUNKS — windows inside a chunk are not independent.

    Resampling windows would treat 58 overlapping views of the same four
    seconds as 58 independent observations and report an interval several times
    too tight.
    """
    if len(per_chunk) < 2:
        return {}
    rng = random.Random(seed)
    f1s, exacts = [], []
    for _ in range(n_boot):
        draw = [per_chunk[rng.randrange(len(per_chunk))] for _ in per_chunk]
        rep = KeyStats.merge(draw).report()
        if rep.get("key_macro_f1") is not None:
            f1s.append(rep["key_macro_f1"])
            exacts.append(rep["key_exact"])
    if not f1s:
        return {}
    f1s.sort()
    exacts.sort()
    lo, hi = int(0.025 * len(f1s)), min(len(f1s) - 1, int(0.975 * len(f1s)))
    return {
        "key_macro_f1_ci95": [round(f1s[lo], 4), round(f1s[hi], 4)],
        "key_exact_ci95": [round(exacts[lo], 4), round(exacts[hi], 4)],
        "n_boot": len(f1s),
    }


def paired_bootstrap(a: list[KeyStats], b: list[KeyStats], *,
                     n_boot: int = 1000, seed: int = 0) -> dict:
    """CI for (a - b) with both systems resampled on the SAME chunks.

    The question D-3 actually asks is "is the external model above the trivial
    line", and that is a difference. Two independent CIs answer it badly:
    every system here is scored on identical windows, so most of the variance
    is shared — which chunks got drawn — and cancels in the difference. Taking
    the same draw for both keeps that cancellation instead of adding the two
    spreads together and calling a real gap inconclusive.
    """
    if len(a) != len(b) or len(a) < 2:
        return {}
    rng = random.Random(seed)
    diffs_f1, diffs_exact = [], []
    for _ in range(n_boot):
        idx = [rng.randrange(len(a)) for _ in a]
        ra = KeyStats.merge([a[i] for i in idx]).report()
        rb = KeyStats.merge([b[i] for i in idx]).report()
        if ra.get("key_macro_f1") is None or rb.get("key_macro_f1") is None:
            continue
        diffs_f1.append(ra["key_macro_f1"] - rb["key_macro_f1"])
        diffs_exact.append(ra["key_exact"] - rb["key_exact"])
    if not diffs_f1:
        return {}
    diffs_f1.sort()
    diffs_exact.sort()
    lo, hi = int(0.025 * len(diffs_f1)), min(len(diffs_f1) - 1,
                                             int(0.975 * len(diffs_f1)))
    point_f1 = (KeyStats.merge(a).report()["key_macro_f1"]
                - KeyStats.merge(b).report()["key_macro_f1"])
    return {
        "delta_key_macro_f1": round(point_f1, 4),
        "delta_key_macro_f1_ci95": [round(diffs_f1[lo], 4), round(diffs_f1[hi], 4)],
        "delta_key_exact_ci95": [round(diffs_exact[lo], 4), round(diffs_exact[hi], 4)],
        # "excludes zero" is the only claim a percentile CI licenses here
        "macro_f1_ci_excludes_zero": bool(diffs_f1[lo] > 0 or diffs_f1[hi] < 0),
        "n_boot": len(diffs_f1),
    }


def _is_press(event_type) -> bool | None:
    """None for anything unrecognised — never guess a transition direction."""
    s = str(event_type or "").strip().lower()
    if s.endswith("press") or s == "down":
        return True
    if s.endswith("release") or s == "up":
        return False
    return None


def replay_events(events: list[dict], n_frames: int, fps: float, *,
                  shift_ns: int = 0, seed_keys: list[int] | None = None
                  ) -> tuple[list[list[int]], list[tuple[int, int]]]:
    """Predicted events -> (per-frame multi-hot key state, per-frame dx/dy).

    Mirrors `d2e.build_rows`: frame j sits at `j / fps` and owns the input in
    `(t_{j-1}, t_j]`, with everything at or before t_0 folded into frame 0.
    Keeping that convention identical to the converter's matters more than it
    looks — doc 26 §2.5 caught a one-frame offset that never raised an error
    and only ate correlation.

    `shift_ns` moves events EARLIER, matching upstream's `--time-shift`: the
    model emits an action after observing its effect, so the shift undoes that.
    """
    step = 1e9 / fps
    evs = sorted(((int(e["timestamp"]) - shift_ns, e) for e in events),
                 key=lambda x: x[0])
    held = set(seed_keys or []) & VOCAB
    i = 0
    keys_per_frame: list[list[int]] = []
    deltas: list[tuple[int, int]] = []
    for j in range(n_frames):
        t = j * step
        dx = dy = 0
        while i < len(evs) and evs[i][0] <= t:
            _, e = evs[i]
            i += 1
            topic = e.get("topic")
            if topic == "keyboard":
                vk = e.get("vk")
                press = _is_press(e.get("event_type"))
                if vk is None or press is None:
                    continue
                if press:
                    held.add(int(vk))
                else:
                    held.discard(int(vk))
            elif topic == "mouse/raw":
                dx += int(e.get("last_x") or 0)
                dy += int(e.get("last_y") or 0)
                flags = int(e.get("button_flags") or 0)
                for bit, vk, down in _BTN_FLAGS:
                    if flags & bit:
                        held.add(vk) if down else held.discard(vk)
        keys_per_frame.append(keys_to_multihot(sorted(held & VOCAB)))
        deltas.append((dx, dy))
    return keys_per_frame, deltas


def centers(n_frames: int, window: int, stride: int) -> list[int]:
    """The exact index WindowDataset builds — do not re-derive it elsewhere."""
    half = window // 2
    return list(range(half, n_frames - half, stride))


def pearson_xy(pred, true) -> dict:
    """Informational only. D-4 removed mouse R^2 from the Phase 0 criteria."""
    out = {}
    for k, idx in (("x", 0), ("y", 1)):
        a = [p[idx] for p in pred]
        b = [t[idx] for t in true]
        if len(a) < 3:
            out[k] = None
            continue
        ma, mb = sum(a) / len(a), sum(b) / len(b)
        va = sum((v - ma) ** 2 for v in a)
        vb = sum((v - mb) ** 2 for v in b)
        if va <= 0 or vb <= 0:
            out[k] = None
            continue
        cov = sum((v - ma) * (w - mb) for v, w in zip(a, b))
        out[k] = round(cov / (va ** 0.5 * vb ** 0.5), 4)
    return out


def load_chunk_records(chunks: list[Path]) -> list[ChunkRecord]:
    out = []
    for c in chunks:
        meta = ChunkMeta.from_json(json.loads((c / "meta.json").read_text()))
        n = sum(1 for line in open(c / "p0.jsonl") if line.strip())
        out.append(ChunkRecord(chunk_dir=c, meta=meta, n_frames=n))
    return out


def _states_chunk_mode(records, pred_dir: Path, shift_ms: float,
                       seed_mode: str) -> tuple[dict, list[dict]]:
    """One independent 4-second episode per chunk (the original protocol)."""
    from .predict import out_name

    states, blobs = {}, []
    for rec in records:
        blob = json.loads((pred_dir / out_name(rec.chunk_dir)).read_text())
        blobs.append(blob)
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        seed = actions[0].keys if seed_mode == "gt" else None
        states[str(rec.chunk_dir)] = replay_events(
            blob["events"], n, blob.get("fps", 30.0),
            shift_ns=int(shift_ms * 1e6), seed_keys=seed)
    return states, blobs


def _states_run_mode(records, pred_dir: Path, shift_ms: float,
                     seed_mode: str) -> tuple[dict, list[dict]]:
    """One episode per contiguous RUN — research/30 D-3-b.

    The replay happens once over the whole run and is then sliced per chunk, so
    a key pressed in chunk 3 is still held in chunk 7 without anyone re-seeding
    it. That is the whole point of the condition: `seed=gt` here means 12 bits
    at the START OF THE RUN (once per ~160 s), not once per 4 s.

    Slicing after the replay rather than replaying per chunk is what keeps the
    time-shift sweep honest — a shift moves events across chunk boundaries, and
    a per-chunk carry-in state baked in at shift=0 would silently stop matching
    the shift being scored.
    """
    wanted = {str(r.chunk_dir) for r in records}
    states, blobs = {}, []
    for path in sorted(pred_dir.glob("run__*.json")):
        blob = json.loads(path.read_text())
        members = blob["members"]
        if not any(m["chunk"] in wanted for m in members):
            continue
        blobs.append(blob)
        first = Path(members[0]["chunk"])
        seed = (load_actions(first / "p0.jsonl")[0].keys
                if seed_mode == "gt" else None)
        keys, delta = replay_events(
            blob["events"], blob["n_frames_total"], blob.get("fps", 30.0),
            shift_ns=int(shift_ms * 1e6), seed_keys=seed)
        for m in members:
            o, n = m["offset"], m["n_frames"]
            states[m["chunk"]] = (keys[o:o + n], delta[o:o + n])
    return states, blobs

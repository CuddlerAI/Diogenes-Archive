"""External IDM baseline — research/26 D-3.

Phase 0 has acceptance gates but no *relative* criterion: the thresholds
(key_exact 0.85, macro F1 0.40) are proxies read off VPT's paper, and doc 26
§2.3 records that the planned Dreamer-4 control was never run. So "0.1885 is
not good enough" has, until now, only ever been a statement about a threshold
nobody validated on this corpus.

`Generalist-IDM-1B` (Apache-2.0, `open-world-agents/Generalist-IDM-1B`) is the
IDM shipped alongside the D2E corpus we already mirror. Scoring it on OUR
evaluation set, with OUR metric, turns the gate numbers into a comparison.

Two halves, deliberately split across two processes because they need
incompatible dependency sets:

  predict.py  runs the upstream inference pipeline (owa-* + transformers>=5)
              over chunk videos and dumps predicted events as JSON.
  score.py    reconstructs per-frame key state from those events, and scores
              it — plus the trivial baselines, plus our own checkpoint — on
              exactly the same windows with `evaluate.run_eval`'s definitions.

**Read `score.py`'s module docstring before reading any number this produces.**
Generalist-IDM-1B was trained on D2E, so both of our splits are in-distribution
for it — including the Rainbow Six "zero-shot" holdout. It is a ceiling
reference, not a peer.
"""

"""Diogenes VL — game-screen recognition model line (the SOTA target).

Implements the training route from
model_research/from_hakkolite/model/16-local-vlm-screen-recognition.md §4.4:
teacher (Qwen3-VL-235B, Apache-2.0) batch-annotates game/screen corpus ->
"screenshot -> structured observation JSON" SFT pairs -> LoRA SFT on a small
Qwen3-VL base -> (later) GRPO refinement. Method validation runs on open data
(gaming-500-hours) before any cohort data is spent.
"""

__version__ = "0.1.0"

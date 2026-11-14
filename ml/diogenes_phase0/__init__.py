"""Diogenes Phase 0 — IDM method-validation infrastructure.

Implements the Phase 0 plan from model_research/research/01-idm-teacher-scaling-dilemma.md §6:
frozen trunk + small (<50M trainable) non-causal IDM head, trained on synthetic /
open data, evaluated against the acceptance gates (key exact-match >=85%,
mouse R^2 >=0.9, per-class F1 on rare actions, zero-shot on held-out games).
"""

__version__ = "0.1.0"

"""Workspace entry point for the strict local CP2077 E-geo v3 gate."""
from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "ml"))

from diogenes_wm.eval.cp2077_egeo_gate import main


if __name__ == "__main__":
    raise SystemExit(main())

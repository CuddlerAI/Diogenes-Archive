"""Workspace entry point for the strict render-camera E-geo gate."""
from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "ml"))

from diogenes_wm.eval.capture_geo_gate import main


if __name__ == "__main__":
    raise SystemExit(main())

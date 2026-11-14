"""Read-only native selected-Present geometry checks; no PR405 or production promotion."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ml"))
from diogenes_wm.eval.cp2077_selected_geometry import main

if __name__ == "__main__":
    raise SystemExit(main())

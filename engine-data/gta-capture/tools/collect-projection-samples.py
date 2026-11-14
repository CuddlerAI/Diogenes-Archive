"""Assemble real, sealed native projection observations; never grant approval."""

import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from gta_capture.calibration_sampling import samples_from_native_frames
from gta_capture.errors import ContractError
from gta_capture.jsonio import write_json_atomic


def _validate_output_paths(frames, out: Path, report: Path) -> None:
    inputs = [path.resolve() for path in frames]
    outputs = [out.resolve(), report.resolve()]

    def same_file(first: Path, second: Path) -> bool:
        return (first == second or
                (first.exists() and second.exists() and first.samefile(second)))

    if same_file(*outputs):
        raise ContractError("sample output and report output must be different files")
    for output in outputs:
        if any(same_file(output, source) for source in inputs):
            raise ContractError("calibration outputs must not overwrite frame-event inputs")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--frames", type=Path, nargs="+", required=True)
    parser.add_argument("--game-build", required=True)
    parser.add_argument("--captured-at", required=True, help="actual native capture time, ISO8601")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    try:
        _validate_output_paths(args.frames, args.out, args.report)
        samples, report = samples_from_native_frames(
            args.frames, game_build=args.game_build, captured_at=args.captured_at)
        write_json_atomic(args.out, samples)
        write_json_atomic(args.report, report)
    except (ContractError, OSError, ValueError) as exc:
        print(f"calibration sampling refused: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Read v2 engineering trace; join actual low16 samples to CFR replacements.

No low16 unwrapping, missing-frame interpolation or modification of the session.
"""

import argparse
import hashlib
import json
from collections import Counter, defaultdict
from pathlib import Path


def summarize(data: bytes) -> dict:
    rows = [json.loads(line) for line in data.splitlines() if line.strip()]
    if not rows or rows[0].get("schema") != "capture-cadence-trace/v2":
        raise ValueError("requires capture-cadence-trace/v2")
    if not rows[0].get("raw_watermark"):
        raise ValueError("trace did not enable raw watermark diagnostics")
    footer = rows[-1] if rows[-1].get("kind") == "footer" else None
    samples = {}
    stages = defaultdict(list)
    replacements = []
    submitted = []
    conflicts = []
    timestamp_rejections = []
    for row in rows[1:]:
        kind = row.get("kind")
        if kind == "raw_watermark":
            seq = row["source_seq"]
            if seq in samples and samples[seq] != row:
                conflicts.append(seq)
            samples[seq] = row
        elif kind == "source":
            stages[row["source_seq"]].append(row["stage"])
        elif kind == "timestamp_rejected":
            timestamp_rejections.append(row)
        elif kind == "observe" and row.get("replaced_source_seq") is not None:
            replacements.append(row)
        elif kind == "output_submitted":
            submitted.append(row)

    def identity(seq):
        sample = samples.get(seq)
        return {
            "source_seq": seq,
            "raw_present_low16": sample.get("raw_present_low16") if sample else None,
            "status": sample["status"] if sample else "not_observed",
            "source_qpc": sample.get("source_qpc") if sample else None,
            "stages": stages[seq],
            "identity_conflict": seq in conflicts,
        }

    joined = [{"run_frame": row["slot"]["run_frame"],
               "replaced": identity(row["replaced_source_seq"]),
               "replacement": identity(row["source_seq"])} for row in replacements]
    return {
        "schema": "dm-cp2077-cadence-watermark-readout/v1",
        "non_trainable": True,
        "trace_sha256": hashlib.sha256(data).hexdigest(),
        "context": rows[0],
        "footer": footer,
        "trace_complete": bool(footer and footer.get("complete") and not conflicts),
        "identity_scope": "Observed low16 only; no full Present inference or pairing acceptance.",
        "raw_sample_count": len(samples),
        "decode_status_counts": dict(Counter(row["status"] for row in samples.values())),
        "identity_conflicts": sorted(set(conflicts)),
        "timestamp_rejections": timestamp_rejections,
        "replacements": joined,
        "replacements_without_decoded_identity": sum(
            item["replaced"]["raw_present_low16"] is None for item in joined),
        "output_submitted_count": len(submitted),
        "unique_submitted_sources": len({r["source_seq"] for r in submitted}),
        "last_output_submitted": submitted[-1] if submitted else None,
        "submitted_identity_samples": [identity(seq) for seq in sorted(
            {r["source_seq"] for r in submitted})],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path,
                        help="new report outside the raw session")
    args = parser.parse_args()
    if args.trace.stat().st_size > 64 * 1024 * 1024:
        raise ValueError("trace exceeds 64 MiB diagnostic bound")
    data = args.trace.read_bytes()
    result = summarize(data)
    if hashlib.sha256(args.trace.read_bytes()).hexdigest() != result["trace_sha256"]:
        raise ValueError("trace changed during analysis")
    with args.output.open("x", encoding="utf-8", newline="\n") as out:
        json.dump(result, out, ensure_ascii=False, indent=2)
        out.write("\n")


if __name__ == "__main__":
    main()

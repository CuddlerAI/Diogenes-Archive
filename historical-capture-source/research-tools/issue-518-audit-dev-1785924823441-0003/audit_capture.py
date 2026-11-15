import json
import sys
from pathlib import Path

import av
import zstandard


BLOCK_PX = 8
TOTAL_BLOCKS = 24


def decompress_frames(segment: Path):
    source = segment / "frames.jsonl.zst"
    with source.open("rb") as compressed:
        with zstandard.ZstdDecompressor().stream_reader(compressed) as reader:
            text = reader.read().decode("utf-8")
    return [line for line in text.splitlines() if line.strip()]


def block_means(frame, scale_x, scale_y):
    plane = frame.planes[0]
    pixels = memoryview(plane)
    stride = plane.line_size
    means = []
    for block in range(TOTAL_BLOCKS):
        center_x = round((block * BLOCK_PX + BLOCK_PX / 2) * scale_x)
        center_y = round((BLOCK_PX / 2) * scale_y)
        total = 0
        for y in range(center_y - 2, center_y + 2):
            row = y * stride
            for x in range(center_x - 2, center_x + 2):
                total += pixels[row + x]
        means.append(total // 16)
    return means


def extract_watermarks(segment: Path, output: Path, frame_start: int, width: int, height: int):
    rows = []
    scale_x = width / 1920
    scale_y = height / 1080
    with av.open(str(segment / "video.mp4")) as container:
        for local_frame, frame in enumerate(container.decode(video=0)):
            rows.append(
                {
                    "block_means": block_means(frame, scale_x, scale_y),
                    "session_frame": frame_start + local_frame,
                }
            )
    with output.open("w", encoding="utf-8", newline="\n") as handle:
        for row in rows:
            handle.write(json.dumps(row, separators=(",", ":")) + "\n")
    return rows


def main():
    session = Path(sys.argv[1])
    audit = Path(sys.argv[2])
    segments = sorted((session / "runs" / "r0000" / "segments").iterdir())
    frame_lines = []
    watermark_rows = []
    summary = []

    for segment in segments:
        meta = json.loads((segment / "meta.json").read_text(encoding="utf-8"))
        lines = decompress_frames(segment)
        frame_lines.extend(lines)
        output = audit / f"watermark-{segment.name}.jsonl"
        rows = extract_watermarks(
            segment,
            output,
            int(meta["session_frame_start"]),
            int(meta["width"]),
            int(meta["height"]),
        )
        summary.append(
            {
                "segment": segment.name,
                "declared_frames": int(meta["n_frames"]),
                "frame_rows": len(lines),
                "decoded_video_frames": len(rows),
                "width": int(meta["width"]),
                "height": int(meta["height"]),
                "fps_num": int(meta["fps_num"]),
                "fps_den": int(meta["fps_den"]),
            }
        )
        watermark_rows.extend(rows)

    (audit / "frames.jsonl").write_text(
        "\n".join(frame_lines) + "\n", encoding="utf-8", newline="\n"
    )
    with (audit / "watermark-samples.jsonl").open(
        "w", encoding="utf-8", newline="\n"
    ) as handle:
        for row in watermark_rows:
            handle.write(json.dumps(row, separators=(",", ":")) + "\n")

    result = {
        "segments": len(segments),
        "frame_rows": len(frame_lines),
        "watermark_rows": len(watermark_rows),
        "all_segment_counts_match": all(
            item["declared_frames"]
            == item["frame_rows"]
            == item["decoded_video_frames"]
            for item in summary
        ),
        "segment_details": summary,
    }
    (audit / "extraction-summary.json").write_text(
        json.dumps(result, indent=2), encoding="utf-8", newline="\n"
    )
    print(json.dumps({key: result[key] for key in result if key != "segment_details"}))


if __name__ == "__main__":
    main()

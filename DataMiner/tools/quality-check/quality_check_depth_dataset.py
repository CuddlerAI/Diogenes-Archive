#!/usr/bin/env python3
"""Quality checks for SFM depth dataset deliveries."""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
import tempfile
import zipfile
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path, PurePosixPath
from typing import Any


EXPECTED_30FPS_ORDER = [
    "time",
    "fps",
    "frame",
    "camera_position",
    "camera_rotation_quaternion",
    "camera_follow_offset",
    "camera_intrinsics",
    "camera_speed",
    "player_position",
    "mouseX",
    "mouseY",
    "mouse_dx",
    "mouse_dy",
    "keyCode",
    "player_rotation_quaternion",
    "player_speed",
    "metric_scale",
]

EXPECTED_15FPS_ORDER = [
    "time",
    "fps",
    "depth_name",
    "frame",
    "camera_position",
    "camera_rotation_quaternion",
    "camera_follow_offset",
    "camera_intrinsics",
    "camera_speed",
    "player_position",
    "mouseX",
    "mouseY",
    "mouse_dx",
    "mouse_dy",
    "keyCode",
    "player_rotation_quaternion",
    "player_speed",
    "metric_scale",
]

OPTIONAL_FIELDS = {"camera_rotation_oula", "player_rotation_oula"}


@dataclass
class CheckResult:
    item: str
    standard: str
    actual: str
    status: str


def pass_result(item: str, standard: str, actual: str) -> CheckResult:
    return CheckResult(item, standard, actual, "通过")


def fail_result(item: str, standard: str, actual: str) -> CheckResult:
    return CheckResult(item, standard, actual, "不通过")


def na_result(item: str, standard: str, actual: str) -> CheckResult:
    return CheckResult(item, standard, actual, "不计问题")


def warn_result(item: str, standard: str, actual: str) -> CheckResult:
    return CheckResult(item, standard, actual, "需人工确认")


def load_json(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def read_video_info(video_path: Path) -> tuple[bool, dict[str, Any], str | None]:
    if not video_path.exists():
        return False, {}, "missing video.mp4"
    try:
        import cv2  # type: ignore
    except Exception as exc:
        return False, {}, f"OpenCV unavailable: {exc}"

    cap = cv2.VideoCapture(str(video_path))
    if not cap.isOpened():
        return False, {}, "OpenCV cannot open video"
    fps = float(cap.get(cv2.CAP_PROP_FPS))
    frames = int(round(cap.get(cv2.CAP_PROP_FRAME_COUNT)))
    width = int(round(cap.get(cv2.CAP_PROP_FRAME_WIDTH)))
    height = int(round(cap.get(cv2.CAP_PROP_FRAME_HEIGHT)))
    cap.release()
    duration = frames / fps if fps else None
    return True, {
        "fps": fps,
        "frames": frames,
        "width": width,
        "height": height,
        "duration": duration,
    }, None


def timestamp_errors(records: list[dict[str, Any]]) -> tuple[float | None, str | None]:
    if not records:
        return None, "empty records"
    fmt = "%Y-%m-%d %H:%M:%S.%f"
    try:
        t0 = datetime.strptime(str(records[0]["time"]), fmt)
        errors = []
        for record in records:
            current = datetime.strptime(str(record["time"]), fmt)
            frame = int(record["frame"])
            delta = (current - t0).total_seconds()
            errors.append(delta - frame / 30.0)
        return max(abs(value) for value in errors), None
    except Exception as exc:
        return None, str(exc)


def keys_match_prefix(keys: list[str], expected: list[str]) -> bool:
    filtered = [key for key in keys if key not in OPTIONAL_FIELDS]
    return filtered[: len(expected)] == expected


def depth_zip_names(zip_path: Path) -> tuple[list[str], str | None]:
    if not zip_path.exists():
        return [], "missing depth.zip"
    try:
        with zipfile.ZipFile(zip_path) as zf:
            names = [
                PurePosixPath(name).name
                for name in zf.namelist()
                if name.lower().endswith(".exr")
            ]
        return names, None
    except Exception as exc:
        return [], str(exc)


def read_depth_sample_info(zip_path: Path, zip_names: list[str]) -> tuple[dict[str, Any], str | None]:
    if not zip_names:
        return {}, "no EXR files"
    sample_name = sorted(zip_names)[0]

    os.environ.setdefault("OPENCV_IO_ENABLE_OPENEXR", "1")
    try:
        import cv2  # type: ignore
    except Exception as exc:
        return {}, f"OpenCV unavailable: {exc}"

    try:
        with zipfile.ZipFile(zip_path) as zf:
            member = next(
                (name for name in zf.namelist() if PurePosixPath(name).name == sample_name),
                None,
            )
            if member is None:
                return {}, f"sample not found in zip: {sample_name}"
            with tempfile.NamedTemporaryFile(suffix=".exr", delete=False) as tmp:
                tmp.write(zf.read(member))
                tmp_path = Path(tmp.name)
        try:
            image = cv2.imread(str(tmp_path), cv2.IMREAD_UNCHANGED)
        finally:
            tmp_path.unlink(missing_ok=True)
    except Exception as exc:
        return {}, f"cannot read sample from depth.zip: {exc}"

    if image is None:
        return {}, f"OpenCV cannot read {sample_name} from depth.zip"
    if len(image.shape) == 2:
        height, width = image.shape
        channels = 1
    else:
        height, width, channels = image.shape
    return {
        "name": sample_name,
        "width": int(width),
        "height": int(height),
        "channels": int(channels),
        "dtype": str(image.dtype),
    }, None


def expected_depth_names(video_frames: int) -> set[str]:
    return {f"depth_{frame}.exr" for frame in range(0, video_frames, 2)}


def expected_depth_stems(video_frames: int) -> set[str]:
    return {f"depth_{frame}" for frame in range(0, video_frames, 2)}


def first_last(values: list[Any], count: int = 3) -> str:
    if not values:
        return "空"
    if len(values) <= count * 2:
        return ", ".join(map(str, values))
    return ", ".join(map(str, values[:count])) + " ... " + ", ".join(map(str, values[-count:]))


def check_dataset(dataset_dir: Path) -> list[CheckResult]:
    results: list[CheckResult] = []

    video_path = dataset_dir / "video.mp4"
    video_ok, video_info, video_error = read_video_info(video_path)
    video_frames = int(video_info.get("frames") or 0)
    video_fps = float(video_info.get("fps") or 0)
    expected_15_count = math.ceil(video_frames / 2) if video_frames else 0

    results.append(
        pass_result("视频文件命名", "video.mp4", "当前为 video.mp4")
        if video_path.exists()
        else fail_result("视频文件命名", "video.mp4", "缺少 video.mp4")
    )
    if video_ok:
        results.append(
            pass_result("视频分辨率", "1920x1080", f"{video_info['width']}x{video_info['height']}")
            if video_info["width"] == 1920 and video_info["height"] == 1080
            else fail_result("视频分辨率", "1920x1080", f"{video_info['width']}x{video_info['height']}")
        )
        results.append(
            pass_result("视频 FPS", "视频帧率 30fps", f"{video_fps:.3f}fps")
            if abs(video_fps - 30.0) < 0.01
            else fail_result("视频 FPS", "视频帧率 30fps", f"{video_fps:.3f}fps")
        )
        expected_frames_text = "60s 视频应为 1800 帧，帧号 0~1799"
        frame_actual = f"{video_frames} 帧，时长 {video_info['duration']:.3f}s"
        results.append(
            pass_result("视频帧数", expected_frames_text, frame_actual)
            if video_frames == 1800
            else fail_result("视频帧数", expected_frames_text, frame_actual)
        )
    else:
        for item in ["视频分辨率", "视频 FPS", "视频帧数"]:
            results.append(fail_result(item, "需要可读取 video.mp4", video_error or "视频不可读"))

    json30_path = dataset_dir / "action_camera_30fps.json"
    results.append(
        pass_result("action_camera_30fps.json 文件命名", "30fps 全帧 JSON 应命名为 action_camera_30fps.json", "当前为 action_camera_30fps.json")
        if json30_path.exists()
        else fail_result("action_camera_30fps.json 文件命名", "30fps 全帧 JSON 应命名为 action_camera_30fps.json", "缺少 action_camera_30fps.json")
    )

    records30: list[dict[str, Any]] = []
    if json30_path.exists():
        try:
            records30 = load_json(json30_path)
        except Exception as exc:
            results.append(fail_result("action_camera_30fps.json 可解析性", "合法 JSON", str(exc)))
    if records30:
        frames30 = [int(record.get("frame", -1)) for record in records30]
        expected_frames = list(range(video_frames)) if video_frames else list(range(1800))
        results.append(
            pass_result("action_camera_30fps.json 数量", "30fps 全帧 JSON，60s 应 1800 条", f"{len(records30)} 条")
            if len(records30) == 1800
            else fail_result("action_camera_30fps.json 数量", "30fps 全帧 JSON，60s 应 1800 条", f"{len(records30)} 条")
        )
        ok_frames = frames30 == expected_frames
        results.append(
            pass_result("action_camera_30fps.json 帧号", "从 0 开始，到 1799，无缺帧", f"{frames30[0]}~{frames30[-1]}，无缺帧")
            if ok_frames
            else fail_result("action_camera_30fps.json 帧号", "从 0 开始，到 1799，无缺帧", f"实际: {first_last(frames30)}")
        )
        keys30 = list(records30[0].keys())
        results.append(
            pass_result("action_camera_30fps.json 字段顺序", "应为 time, fps, frame, ...", ", ".join(keys30[:5]) + ", ...")
            if keys_match_prefix(keys30, EXPECTED_30FPS_ORDER)
            else fail_result("action_camera_30fps.json 字段顺序", "应为 time, fps, frame, ...", ", ".join(keys30[:5]) + ", ...")
        )
    else:
        for item in ["action_camera_30fps.json 数量", "action_camera_30fps.json 帧号", "action_camera_30fps.json 字段顺序"]:
            results.append(fail_result(item, "需要可读取 action_camera_30fps.json", "文件缺失或不可解析"))

    json15_path = dataset_dir / "action_camera_15fps.json"
    fallback_json15_path = dataset_dir / "action_camera.json"
    json15_for_content = json15_path if json15_path.exists() else fallback_json15_path
    if json15_path.exists():
        results.append(pass_result("action_camera_15fps.json 文件命名", "15fps depth 对齐 JSON 应命名为 action_camera_15fps.json", "当前为 action_camera_15fps.json"))
    elif fallback_json15_path.exists():
        results.append(fail_result("action_camera_15fps.json 文件命名", "15fps depth 对齐 JSON 应命名为 action_camera_15fps.json", "当前为 action_camera.json"))
    else:
        results.append(fail_result("action_camera_15fps.json 文件命名", "15fps depth 对齐 JSON 应命名为 action_camera_15fps.json", "缺少 action_camera_15fps.json"))

    records15: list[dict[str, Any]] = []
    if json15_for_content.exists():
        try:
            records15 = load_json(json15_for_content)
        except Exception as exc:
            results.append(fail_result("15fps JSON 可解析性", "合法 JSON", str(exc)))
    if records15:
        frames15 = [int(record.get("frame", -1)) for record in records15]
        expected_even_frames = list(range(0, video_frames or 1800, 2))
        results.append(
            pass_result("15fps JSON 数量", "15fps depth 对齐 JSON，60s 应 900 条", f"{len(records15)} 条")
            if len(records15) == expected_15_count or (not expected_15_count and len(records15) == 900)
            else fail_result("15fps JSON 数量", "15fps depth 对齐 JSON，60s 应 900 条", f"{len(records15)} 条")
        )
        results.append(
            pass_result("15fps JSON 帧号", "depth 对应偶数帧: 0,2,...,1798", "完全匹配")
            if frames15 == expected_even_frames
            else fail_result("15fps JSON 帧号", "depth 对应偶数帧: 0,2,...,1798", f"实际: {first_last(frames15)}")
        )
        keys15 = list(records15[0].keys())
        results.append(
            pass_result("15fps JSON 字段顺序", "应为 time, fps, depth_name, frame, ...", ", ".join(keys15[:6]) + ", ...")
            if keys_match_prefix(keys15, EXPECTED_15FPS_ORDER)
            else fail_result("15fps JSON 字段顺序", "应为 time, fps, depth_name, frame, ...", ", ".join(keys15[:6]) + ", ...")
        )
    else:
        for item in ["15fps JSON 数量", "15fps JSON 帧号", "15fps JSON 字段顺序"]:
            results.append(fail_result(item, "需要可读取 15fps JSON", "文件缺失或不可解析"))

    timestamp_sources = []
    if records30:
        timestamp_sources.append(("30fps", records30))
    if records15:
        timestamp_sources.append(("15fps", records15))
    if timestamp_sources:
        messages = []
        ok = True
        for label, records in timestamp_sources:
            max_error, error = timestamp_errors(records)
            if error is not None or max_error is None:
                ok = False
                messages.append(f"{label}: {error}")
            else:
                messages.append(f"{label}: 最大误差 {max_error:.6f}s")
                ok = ok and max_error <= 0.001
        results.append(
            pass_result("时间戳对齐", "time 与 frame/30 对齐，误差 <= 0.001s", "; ".join(messages))
            if ok
            else fail_result("时间戳对齐", "time 与 frame/30 对齐，误差 <= 0.001s", "; ".join(messages))
        )
    else:
        results.append(fail_result("时间戳对齐", "time 与 frame/30 对齐", "缺少可检查 JSON"))

    fps_values = []
    for records in (records30, records15):
        fps_values.extend(float(record["fps"]) for record in records if "fps" in record)
    if fps_values:
        fps_actual = f"{min(fps_values):.2f}~{max(fps_values):.2f}"
        results.append(
            pass_result("fps 字段", "大于等于 30 即可", fps_actual)
            if min(fps_values) >= 30
            else fail_result("fps 字段", "大于等于 30 即可", fps_actual)
        )
    else:
        results.append(fail_result("fps 字段", "大于等于 30 即可", "缺少 fps 字段"))

    zip_path = dataset_dir / "depth.zip"
    zip_names, zip_error = depth_zip_names(zip_path)
    results.append(
        pass_result("depth 数量", "60s、15fps 应 900 张", f"{len(zip_names)} 张 EXR")
        if not zip_error and len(zip_names) == expected_15_count
        else fail_result("depth 数量", "60s、15fps 应 900 张", zip_error or f"{len(zip_names)} 张 EXR")
    )

    sample_info, sample_error = read_depth_sample_info(zip_path, zip_names)
    if sample_info:
        actual = f"{sample_info['width']}x{sample_info['height']}，{sample_info['channels']} 通道，{sample_info['dtype']}"
        results.append(
            pass_result("depth 格式", "EXR，720p，单通道", actual)
            if sample_info["width"] == 1280 and sample_info["height"] == 720 and sample_info["channels"] == 1
            else fail_result("depth 格式", "EXR，720p，单通道", actual)
        )
    else:
        results.append(warn_result("depth 格式", "EXR，720p，单通道", sample_error or "未能读取样本"))

    if zip_names and video_frames:
        expected_names = expected_depth_names(video_frames)
        actual_names = set(zip_names)
        missing = sorted(expected_names - actual_names, key=lambda name: int(re.search(r"\d+", name).group(0)))
        extra = sorted(actual_names - expected_names)
        actual = "完全匹配" if not missing and not extra else f"缺少 {len(missing)}，多出 {len(extra)}；样例: {first_last(extra[:6] or missing[:6])}"
        results.append(
            pass_result("depth 命名", "新标准: depth_0.exr, depth_2.exr, ..., depth_1798.exr", actual)
            if not missing and not extra
            else fail_result("depth 命名", "新标准: depth_0.exr, depth_2.exr, ..., depth_1798.exr", actual)
        )
    else:
        results.append(fail_result("depth 命名", "新标准: depth_0.exr, depth_2.exr, ..., depth_1798.exr", "缺少 depth.zip 或视频帧数"))

    if records15 and video_frames:
        actual_stems = {str(record.get("depth_name", "")) for record in records15}
        expected_stems = expected_depth_stems(video_frames)
        ok = actual_stems == expected_stems
        sample = first_last(sorted(actual_stems)[:6])
        results.append(
            pass_result("15fps JSON 的 depth_name", "应与新 depth 命名一致: depth_0, depth_2, ..., depth_1798", "完全匹配")
            if ok
            else fail_result("15fps JSON 的 depth_name", "应与新 depth 命名一致: depth_0, depth_2, ..., depth_1798", f"不匹配；当前样例: {sample}")
        )
    else:
        results.append(fail_result("15fps JSON 的 depth_name", "应与新 depth 命名一致", "缺少可检查 15fps JSON 或视频帧数"))

    has_camera_oula = any("camera_rotation_oula" in record for record in records30 + records15)
    has_player_oula = any("player_rotation_oula" in record for record in records30 + records15)
    results.append(pass_result("多出 camera_rotation_oula", "可有可无", "存在" if has_camera_oula else "不存在"))
    results.append(pass_result("多出 player_rotation_oula", "可有可无", "存在" if has_player_oula else "不存在"))

    results.append(na_result("地图比例/边界", "不做特别要求", "未作为硬性验收项"))
    results.append(na_result("坐标系/单位声明", "不做特别要求", "未作为硬性验收项"))

    return results


def markdown_table(results: list[CheckResult]) -> str:
    lines = [
        "| 检查项 | 标准/口径 | 当前数据情况 | 是否通过 |",
        "|---|---|---|---|",
    ]
    for result in results:
        lines.append(
            "| "
            + " | ".join(
                str(value).replace("\n", " ").replace("|", "\\|")
                for value in [result.item, result.standard, result.actual, result.status]
            )
            + " |"
        )
    return "\n".join(lines)


def final_conclusion(results: list[CheckResult]) -> str:
    hard_failures = [result for result in results if result.status == "不通过"]
    if hard_failures:
        names = "、".join(result.item for result in hard_failures)
        return f"最终结论：暂不通过。硬性不通过项：{names}"
    return "最终结论：通过。"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="检查游戏 depth 数据交付目录，并输出逐项验收表格。"
    )
    parser.add_argument(
        "dataset_dir",
        type=Path,
        help="数据目录，例如 /Users/example/Desktop/zhangx/废弃/2026_08_01_Hogwarts Legacy_2",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="输出 JSON，默认输出 Markdown 表格。",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    dataset_dir = args.dataset_dir.expanduser().resolve()
    if not dataset_dir.exists() or not dataset_dir.is_dir():
        print(f"数据目录不存在或不是目录: {dataset_dir}", file=sys.stderr)
        return 2

    results = check_dataset(dataset_dir)
    if args.json:
        payload = {
            "dataset_dir": str(dataset_dir),
            "results": [asdict(result) for result in results],
            "conclusion": final_conclusion(results),
        }
        print(json.dumps(payload, ensure_ascii=False, indent=2))
    else:
        print(markdown_table(results))
        print()
        print(final_conclusion(results))

    return 1 if any(result.status == "不通过" for result in results) else 0


if __name__ == "__main__":
    raise SystemExit(main())

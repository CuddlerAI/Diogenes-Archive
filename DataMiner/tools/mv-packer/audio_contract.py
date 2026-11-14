"""Validation helpers for optional public FLAC chunk members."""

from __future__ import annotations

import hashlib
import json
import pathlib


BASE_FILES = ("p0.mp4", "p0.jsonl", "meta.json")
AUDIO_FORMAT = {
    "source": "process_loopback",
    "sample_rate": 48_000,
    "channels": 2,
    "bits": 16,
    "sample_format": "pcm_s16le",
}


def _sha256(raw: bytes) -> str:
    return f"sha256:{hashlib.sha256(raw).hexdigest()}"


def chunk_members(
    chunk_dir: pathlib.Path, key: str, audio_expected: bool
) -> tuple[list[tuple[str, bytes]], dict, dict | None]:
    """Return deterministic tar members after enforcing converter audio metadata."""
    raw = {name: (chunk_dir / name).read_bytes() for name in BASE_FILES}
    meta = json.loads(raw["meta.json"].decode("utf-8"))
    audio = meta.get("audio")
    if (audio is not None) != audio_expected:
        raise ValueError(f"{chunk_dir}: chunk/session audio generation mismatch")

    expected_files = set(BASE_FILES)
    members = [
        (f"{key}.p0.mp4", raw["p0.mp4"]),
        (f"{key}.p0.jsonl", raw["p0.jsonl"]),
        (f"{key}.meta.json", raw["meta.json"]),
    ]
    if audio is not None:
        for field, expected in AUDIO_FORMAT.items():
            if audio.get(field) != expected:
                raise ValueError(f"{chunk_dir}: audio {field} is not canonical")
        state = audio.get("state")
        if state == "ready":
            if not (
                audio.get("present") is True
                and audio.get("file") == "p0.flac"
                and audio.get("codec") == "flac"
                and isinstance(audio.get("frames"), int)
                and audio["frames"] > 0
                and audio.get("failure_codes") == []
            ):
                raise ValueError(f"{chunk_dir}: ready audio metadata is incomplete")
            flac = (chunk_dir / "p0.flac").read_bytes()
            if audio.get("sha256") != _sha256(flac):
                raise ValueError(f"{chunk_dir}: p0.flac SHA-256 mismatch")
            expected_files.add("p0.flac")
            # Keep the historical first three members stable; audio is literally
            # the optional fourth member for v2-aware readers.
            members.append((f"{key}.p0.flac", flac))
        elif state in {"degraded", "absent"}:
            if not (
                audio.get("present") is False
                and all(audio.get(field) is None for field in ("file", "codec", "frames", "sha256"))
                and isinstance(audio.get("failure_codes"), list)
                and audio["failure_codes"]
            ):
                raise ValueError(f"{chunk_dir}: unavailable audio metadata is incomplete")
        else:
            raise ValueError(f"{chunk_dir}: unsupported audio state {state!r}")

    actual_files = {path.name for path in chunk_dir.iterdir() if path.is_file()}
    if actual_files != expected_files:
        raise ValueError(
            f"{chunk_dir}: chunk members {sorted(actual_files)} do not match metadata"
        )
    return members, meta, audio


def validate_session_audio(summary: dict | None, chunks: list[dict | None]) -> None:
    """Close the session summary over every chunk before publication."""
    if summary is None:
        if any(audio is not None for audio in chunks):
            raise ValueError("legacy session unexpectedly contains chunk audio metadata")
        return
    if any(audio is None for audio in chunks):
        raise ValueError("audio session contains a legacy chunk")
    for field, expected in {**AUDIO_FORMAT, "codec": "flac"}.items():
        if summary.get(field) != expected:
            raise ValueError(f"session audio {field} is not canonical")
    counts = {
        state: sum(audio["state"] == state for audio in chunks if audio is not None)
        for state in ("ready", "degraded", "absent")
    }
    if (
        summary.get("ready_chunks") != counts["ready"]
        or summary.get("degraded_chunks") != counts["degraded"]
        or summary.get("absent_chunks") != counts["absent"]
        or summary.get("all_publishable")
        != (counts["degraded"] == 0 and counts["absent"] == 0)
    ):
        raise ValueError("session audio summary does not close over chunk states")

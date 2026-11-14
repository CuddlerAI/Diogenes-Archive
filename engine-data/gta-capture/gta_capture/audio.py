"""Validate the normal-speed WAV sub-pass emitted by RockstarEditorPlus."""

from __future__ import annotations

import math
import os
import struct
import tempfile
import wave
from pathlib import Path

from .errors import ContractError
from .jsonio import load_json, sha256_file


_WINDOW_SECONDS = 0.25
_ACTIVE_PEAK_FRACTION = 1e-4
_MIN_ACTIVE_WINDOW_FRACTION = 0.25
_MIN_ACTIVE_FRAME_FRACTION = 0.05
_MAX_CLIPPED_SAMPLE_FRACTION = 0.001


def _read_riff_wave_layout(source: Path) -> dict:
    """Read the small subset of RIFF/WAVE needed for an auditable conversion."""
    size = source.stat().st_size
    with source.open("rb") as stream:
        header = stream.read(12)
        if (len(header) != 12 or header[:4] != b"RIFF"
                or header[8:] != b"WAVE"):
            raise ContractError("sealed audio is not a RIFF/WAVE file")
        declared_size = struct.unpack_from("<I", header, 4)[0] + 8
        if declared_size != size:
            raise ContractError("sealed WAV RIFF size does not match the file size")

        fmt = None
        data_offset = None
        data_size = None
        while stream.tell() < size:
            chunk_header = stream.read(8)
            if len(chunk_header) != 8:
                raise ContractError("sealed WAV contains a truncated chunk header")
            chunk_id = chunk_header[:4]
            chunk_size = struct.unpack_from("<I", chunk_header, 4)[0]
            chunk_offset = stream.tell()
            padded_size = chunk_size + (chunk_size & 1)
            if chunk_offset + padded_size > size:
                raise ContractError("sealed WAV contains a truncated chunk")
            if chunk_id == b"fmt ":
                if fmt is not None or chunk_size < 16:
                    raise ContractError("sealed WAV has an invalid fmt chunk")
                raw_fmt = stream.read(chunk_size)
                fmt = struct.unpack_from("<HHIIHH", raw_fmt)
            elif chunk_id == b"data":
                if data_offset is not None:
                    raise ContractError("sealed WAV contains multiple data chunks")
                data_offset = chunk_offset
                data_size = chunk_size
                stream.seek(chunk_size, os.SEEK_CUR)
            else:
                stream.seek(chunk_size, os.SEEK_CUR)
            if chunk_size & 1:
                stream.seek(1, os.SEEK_CUR)

    if fmt is None or data_offset is None or data_size is None:
        raise ContractError("sealed WAV is missing fmt or data")
    format_tag, channels, rate, byte_rate, block_align, bits = fmt
    return {
        "format_tag": format_tag,
        "channels": channels,
        "sample_rate_hz": rate,
        "byte_rate": byte_rate,
        "block_align": block_align,
        "bits_per_sample": bits,
        "data_offset": data_offset,
        "data_size": data_size,
    }


def normalize_replus_wav(path) -> dict:
    """Atomically normalize RE+'s float32 WAV to the PCM s16le contract.

    RE+ records its normal-speed process-loopback pass as WAVE_FORMAT_IEEE_FLOAT.
    The delivery contract remains PCM s16le/48 kHz/stereo, so conversion happens
    once at the seal boundary while the untouched lane-1 candidate remains as
    provenance. Unknown layouts and non-finite samples fail closed.
    """
    source = Path(path).resolve()
    if not source.is_file():
        raise ContractError(f"audio WAV is missing: {source}")
    layout = _read_riff_wave_layout(source)
    source_sha256 = sha256_file(source)
    source_bytes = source.stat().st_size
    common = {
        "source_format_tag": layout["format_tag"],
        "source_bits_per_sample": layout["bits_per_sample"],
        "source_audio_bytes": source_bytes,
        "source_audio_sha256": source_sha256,
    }
    if (layout["format_tag"] == 1 and layout["channels"] == 2
            and layout["sample_rate_hz"] == 48000
            and layout["bits_per_sample"] == 16
            and layout["block_align"] == 4
            and layout["byte_rate"] == 192000):
        return {
            "schema": "dm-gta-audio-normalization/1",
            "converted": False,
            "method": "already-pcm-s16le",
            **common,
            "audio_bytes": source_bytes,
            "audio_sha256": source_sha256,
        }
    if not (layout["format_tag"] == 3 and layout["channels"] == 2
            and layout["sample_rate_hz"] == 48000
            and layout["bits_per_sample"] == 32
            and layout["block_align"] == 8
            and layout["byte_rate"] == 384000
            and layout["data_size"] % 8 == 0):
        raise ContractError(
            "RE+ WAV must be IEEE float32 or PCM s16le, 48 kHz, stereo")

    fd, temporary_name = tempfile.mkstemp(
        prefix=source.name + ".", suffix=".pcm-s16le.partial",
        dir=str(source.parent))
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "wb") as output, source.open("rb") as input_stream:
            output_data_size = layout["data_size"] // 2
            output.write(b"RIFF")
            output.write(struct.pack("<I", 36 + output_data_size))
            output.write(b"WAVEfmt ")
            output.write(struct.pack("<IHHIIHH", 16, 1, 2, 48000,
                                     192000, 4, 16))
            output.write(b"data")
            output.write(struct.pack("<I", output_data_size))
            input_stream.seek(layout["data_offset"])
            remaining = layout["data_size"]
            while remaining:
                raw = input_stream.read(min(remaining, 64 * 1024))
                if not raw or len(raw) % 4:
                    raise ContractError("RE+ float WAV contains truncated samples")
                remaining -= len(raw)
                pcm = bytearray()
                for (sample,) in struct.iter_unpack("<f", raw):
                    if not math.isfinite(sample):
                        raise ContractError("RE+ float WAV contains non-finite samples")
                    clamped = max(-1.0, min(1.0, float(sample)))
                    pcm.extend(struct.pack("<h", int(round(clamped * 32767.0))))
                output.write(pcm)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, source)
    except BaseException:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise

    return {
        "schema": "dm-gta-audio-normalization/1",
        "converted": True,
        "method": "ieee-float32-le-to-pcm-s16le",
        **common,
        "audio_bytes": source.stat().st_size,
        "audio_sha256": sha256_file(source),
    }


def radio_off_evidence_valid(item, *, expected_replay_sha256: str | None = None,
                             expected_duration_s: float | None = None) -> bool:
    """Verify a separate, immutable native radio-state readback artifact."""
    if not isinstance(item, dict):
        return False
    value = item.get("path")
    expected_sha = item.get("sha256")
    if not isinstance(value, str) or not isinstance(expected_sha, str) or len(expected_sha) != 64:
        return False
    path = Path(os.path.expandvars(os.path.expanduser(value)))
    if not path.is_absolute() or not path.is_file() or sha256_file(path) != expected_sha.casefold():
        return False
    try:
        evidence = load_json(path)
    except (OSError, UnicodeError, ValueError):
        return False
    return (
        isinstance(evidence, dict)
        and evidence.get("schema") == "dm-gta-radio-state-evidence/1"
        and evidence.get("method") == "gta-native-radio-readback"
        and (expected_replay_sha256 is None
             or evidence.get("replay_sha256") == expected_replay_sha256)
        and evidence.get("coverage_start") == "scenario_applied"
        and evidence.get("coverage_end") == "audio_sealed"
        and (expected_duration_s is None
             or (isinstance(evidence.get("expected_replay_duration_s"), (int, float))
                 and math.isclose(float(evidence["expected_replay_duration_s"]),
                                  float(expected_duration_s), rel_tol=0.0, abs_tol=1e-6)))
        and evidence.get("all_samples_off") is True
        and isinstance(evidence.get("sample_count"), int)
        and not isinstance(evidence.get("sample_count"), bool)
        and evidence.get("sample_count", 0) >= max(
            2, math.ceil(float(expected_duration_s or 0.0) * 30.0))
    )


def audio_rights_evidence_valid(item, *, expected_replay_sha256: str | None,
                                expected_audio_sha256: str | None,
                                expected_use: str) -> bool:
    """Verify a separately sealed human/legal approval bound to this audio artifact."""
    if not isinstance(item, dict):
        return False
    value = item.get("path")
    expected_sha = item.get("sha256")
    if not isinstance(value, str) or not isinstance(expected_sha, str) or len(expected_sha) != 64:
        return False
    path = Path(os.path.expandvars(os.path.expanduser(value)))
    if not path.is_absolute() or not path.is_file() or sha256_file(path) != expected_sha.casefold():
        return False
    try:
        evidence = load_json(path)
    except (OSError, UnicodeError, ValueError, ContractError):
        return False
    approved_by = evidence.get("approved_by") if isinstance(evidence, dict) else None
    approved_at = evidence.get("approved_at") if isinstance(evidence, dict) else None
    return (
        isinstance(evidence, dict)
        and evidence.get("schema") == "dm-gta-audio-rights-evidence/1"
        and evidence.get("approval_status") == "approved"
        and evidence.get("allowed_use") == expected_use
        and evidence.get("replay_sha256") == expected_replay_sha256
        and evidence.get("audio_sha256") == expected_audio_sha256
        and evidence.get("radio_off_required") is True
        and isinstance(approved_by, str) and bool(approved_by.strip())
        and not any(marker in approved_by.casefold()
                    for marker in ("pending", "unknown", "todo"))
        and isinstance(approved_at, str) and approved_at.endswith("Z")
        and len(approved_at) >= 20
    )


def _sample_value(raw: bytes, offset: int, width: int) -> int:
    # validate_wav pins the contract to PCM s16le before sampling.
    if width != 2:
        raise ContractError(f"unsupported WAV sample width: {width} bytes")
    return int.from_bytes(raw[offset:offset + width], "little", signed=True)


def validate_wav(path, *, expected_duration_s: float) -> dict:
    source = Path(path).resolve()
    if not source.is_file():
        raise ContractError(f"audio WAV is missing: {source}")
    if not math.isfinite(expected_duration_s) or expected_duration_s <= 0:
        raise ContractError("expected audio duration must be positive")
    try:
        with wave.open(str(source), "rb") as wav:
            channels = wav.getnchannels()
            rate = wav.getframerate()
            width = wav.getsampwidth()
            frames = wav.getnframes()
            compression = wav.getcomptype()
            if channels <= 0 or rate <= 0 or frames <= 0 or compression != "NONE":
                raise ContractError("WAV must contain uncompressed PCM frames")
            if channels != 2 or rate != 48000 or width != 2:
                raise ContractError(
                    "sealed WAV must be PCM s16le, 48 kHz, stereo")
            total_samples = 0
            sum_squares = 0.0
            frames_read = 0
            window_frames = max(1, int(round(rate * _WINDOW_SECONDS)))
            window_sum_squares = 0.0
            window_samples = 0
            window_frame_count = 0
            active_windows = 0
            measured_windows = 0
            active_frames = 0
            clipped_samples = 0
            current_silent_frames = 0
            max_silent_frames = 0
            full_scale = float((1 << (width * 8 - 1)) - 1)
            active_floor = full_scale * _ACTIVE_PEAK_FRACTION

            def finish_window() -> None:
                nonlocal active_windows, measured_windows
                nonlocal window_sum_squares, window_samples, window_frame_count
                if window_frame_count == 0:
                    return
                window_rms = math.sqrt(window_sum_squares / max(1, window_samples)) / full_scale
                measured_windows += 1
                if window_rms >= _ACTIVE_PEAK_FRACTION:
                    active_windows += 1
                window_sum_squares = 0.0
                window_samples = 0
                window_frame_count = 0

            while True:
                raw = wav.readframes(4096)
                if not raw:
                    break
                stride = width * channels
                if len(raw) % stride:
                    raise ContractError("WAV contains a truncated PCM frame")
                for frame_offset in range(0, len(raw), stride):
                    frame_peak = 0.0
                    for channel in range(channels):
                        offset = frame_offset + channel * width
                        value = _sample_value(raw, offset, width)
                        squared = float(value) * float(value)
                        sum_squares += squared
                        window_sum_squares += squared
                        total_samples += 1
                        window_samples += 1
                        frame_peak = max(frame_peak, abs(float(value)))
                        if abs(value) >= full_scale * 0.999:
                            clipped_samples += 1
                    frames_read += 1
                    window_frame_count += 1
                    if frame_peak >= active_floor:
                        active_frames += 1
                        current_silent_frames = 0
                    else:
                        current_silent_frames += 1
                        max_silent_frames = max(max_silent_frames, current_silent_frames)
                    if window_frame_count >= window_frames:
                        finish_window()
            finish_window()
    except (wave.Error, EOFError, OSError) as exc:
        raise ContractError(f"cannot decode sealed WAV: {exc}") from exc

    duration_s = frames / float(rate)
    tolerance_s = max(0.1, 1.0 / 30.0 + 1.0 / rate)
    duration_integrity = (frames_read == frames
                          and abs(duration_s - expected_duration_s) <= tolerance_s)
    rms = math.sqrt(sum_squares / max(1, total_samples)) / full_scale
    active_window_fraction = active_windows / max(1, measured_windows)
    active_frame_fraction = active_frames / max(1, frames_read)
    active_coverage = (active_window_fraction >= _MIN_ACTIVE_WINDOW_FRACTION
                       and active_frame_fraction >= _MIN_ACTIVE_FRAME_FRACTION)
    max_silence_s = max_silent_frames / float(rate)
    max_allowed_silence_s = min(2.0, max(0.5, expected_duration_s * 0.25))
    max_silence_ok = max_silence_s <= max_allowed_silence_s
    clipped_sample_fraction = clipped_samples / max(1, total_samples)
    clipping_ok = clipped_sample_fraction <= _MAX_CLIPPED_SAMPLE_FRACTION
    non_silent = rms > 1e-5 and active_coverage
    continuous = duration_integrity and active_coverage and max_silence_ok and clipping_ok
    return {
        "schema": "dm-gta-audio-report/2",
        "source": "replus-normal-speed-loopback-subpass",
        "encoding": "pcm_s16le",
        "audio_path": str(source),
        "audio_bytes": source.stat().st_size,
        "audio_sha256": sha256_file(source),
        "duration_expected_s": expected_duration_s,
        "duration_actual_s": duration_s,
        "duration_tolerance_s": tolerance_s,
        "sample_rate_hz": rate,
        "channels": channels,
        "sample_width_bytes": width,
        "pcm_frames_declared": frames,
        "pcm_frames_read": frames_read,
        "normalized_rms": rms,
        "window_duration_s": _WINDOW_SECONDS,
        "active_window_count": active_windows,
        "measured_window_count": measured_windows,
        "active_window_fraction": active_window_fraction,
        "minimum_active_window_fraction": _MIN_ACTIVE_WINDOW_FRACTION,
        "active_frame_fraction": active_frame_fraction,
        "minimum_active_frame_fraction": _MIN_ACTIVE_FRAME_FRACTION,
        "active_coverage": active_coverage,
        "max_contiguous_silence_s": max_silence_s,
        "max_allowed_contiguous_silence_s": max_allowed_silence_s,
        "max_silence_ok": max_silence_ok,
        "clipped_sample_fraction": clipped_sample_fraction,
        "maximum_clipped_sample_fraction": _MAX_CLIPPED_SAMPLE_FRACTION,
        "clipping_ok": clipping_ok,
        "duration_integrity": duration_integrity,
        "radio_off": False,
        "radio_off_evidence": None,
        "continuous": continuous,
        "non_silent": non_silent,
        "rights_checked": False,
        "rights_evidence": None,
        "mock": False,
    }

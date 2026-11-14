"""Bounded, read-only camera v2/v3 reader. v3 resolves lossless field dictionaries.

The returned observations are an in-memory full view, never a replacement RAW file.
The physical v3 schema remains visible. No frame, matrix or admission decision is changed.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import stat

ARCHIVE_BYTES = 256 * 1024 * 1024
REPORT_BYTES = 320 * 1024 * 1024
EXPANDED_REPORT_BYTES = 512 * 1024 * 1024  # v3 only; physical file bounds stay unchanged.
TAIL_BYTES = 64 * 1024 * 1024
ROW_BYTES = 1024 * 1024
BLOCK_BYTES = 8 * 1024 * 1024
EXPANDED_BLOCK_BYTES = 4 * 1024 * 1024
V2 = "dm-cp2077-render-camera-observation/v2"
V3 = "dm-cp2077-render-camera-observation/v3"
BLOCK_SCHEMA = "dm-cp2077-camera-draw-block/v1"


def require(ok, message):
    if not ok:
        raise ValueError(message)


def integer(value):
    return type(value) is int and value >= 0


def invalid_constant(value):
    raise ValueError(f"non-JSON numeric constant: {value}")


def loads(value):
    return json.loads(value, parse_constant=invalid_constant)


def regular(path: Path, limit: int) -> int:
    s = path.lstat()
    require(stat.S_ISREG(s.st_mode) and not path.is_symlink()
            and not (getattr(s, "st_file_attributes", 0) & 0x400), "camera evidence is not a regular file")
    require(s.st_size <= limit, "camera evidence exceeds byte bound")
    return s.st_size


def raw_members(text: str, array=False):
    """Read validated JSON while retaining each value's exact spelling, including -0 and f64."""
    text = text.strip()
    decoder = json.JSONDecoder(parse_constant=invalid_constant)
    start, end = ("[", "]") if array else ("{", "}")
    require(text.startswith(start) and text.endswith(end), "invalid camera dictionary container")
    at, result = 1, []
    while True:
        while text[at].isspace():
            at += 1
        if text[at] == end:
            break
        key = None
        if not array:
            key, at = decoder.raw_decode(text, at)
            require(type(key) is str, "invalid camera dictionary key")
            while text[at].isspace():
                at += 1
            require(text[at] == ":", "missing camera dictionary colon")
            at += 1
            while text[at].isspace():
                at += 1
        begin = at
        _, at = decoder.raw_decode(text, at)
        result.append((key, text[begin:at]))
        while text[at].isspace():
            at += 1
        if text[at] == end:
            break
        require(text[at] == ",", "invalid camera dictionary separator")
        at += 1
        while text[at].isspace():
            at += 1
        require(text[at] != end, "trailing comma in camera dictionary")
    require(at == len(text) - 1, "trailing camera dictionary content")
    return result


def decode_block(line: bytes) -> list[bytes]:
    require(len(line) <= BLOCK_BYTES, "camera block exceeds encoded bound")
    text = line.decode("utf-8").rstrip("\n")
    pairs = raw_members(text)
    fields = dict(pairs)
    require(len(fields) == len(pairs) and set(fields) == {"schema", "keys", "values", "rows", "expanded_bytes"},
            "invalid camera block fields")
    require(loads(fields["schema"]) == BLOCK_SCHEMA, "unknown camera block schema")
    keys, rows, expanded = (loads(fields[k]) for k in ("keys", "rows", "expanded_bytes"))
    values = [v.encode("utf-8") for _, v in raw_members(fields["values"], array=True)]
    require(isinstance(keys, list) and all(type(k) is str for k in keys)
            and len(keys) == len(set(keys)) and len(keys) <= 64 * 128
            and len(values) <= 64 * 128 and isinstance(rows, list) and 0 < len(rows) <= 64
            and integer(expanded) and expanded <= EXPANDED_BLOCK_BYTES, "camera block bounds/keys invalid")
    keys = [json.dumps(k, ensure_ascii=False, separators=(",", ":")).encode("utf-8") for k in keys]
    output, used_keys, used_values, size = [], set(), set(), 0
    for refs in rows:
        require(isinstance(refs, list) and 0 < len(refs) <= 128, "camera draw field count invalid")
        parts, seen, row_size = [], set(), 2
        for pair in refs:
            require(isinstance(pair, list) and len(pair) == 2 and all(integer(i) for i in pair),
                    "invalid camera dictionary reference")
            ki, vi = pair
            require(ki < len(keys) and vi < len(values) and ki not in seen, "missing/duplicate camera field reference")
            row_size += len(keys[ki]) + 1 + len(values[vi]) + bool(parts)
            require(row_size + 1 <= ROW_BYTES and size + row_size + 1 <= EXPANDED_BLOCK_BYTES,
                    "camera dictionary expansion exceeds bound")
            seen.add(ki); used_keys.add(ki); used_values.add(vi)
            parts.append(keys[ki] + b":" + values[vi])
        raw = b"{" + b",".join(parts) + b"}\n"
        size += len(raw)
        output.append(raw)
    require(size == expanded and len(used_keys) == len(keys) and len(used_values) == len(values),
            "camera block expansion count/unused dictionary entry")
    return output


def read_camera_report(path: Path) -> dict:
    path = Path(path)
    report_bytes = regular(path, REPORT_BYTES)
    data = path.read_bytes()
    require(len(data) == report_bytes, "camera report changed during read")
    report = loads(data)
    compact = report.get("schema") == V3
    require(report.get("schema") in (V2, V3), "unsupported camera report schema")
    archive = report.get("observation_archive")
    if archive is None:
        require(not compact and report_bytes <= TAIL_BYTES, "unarchived camera report exceeds legacy bound")
        return report
    require(archive.get("schema") == ("dm-cp2077-camera-archive/v2" if compact else "dm-cp2077-camera-archive/v1")
            and archive.get("non_trainable") is True and archive.get("durable") is True
            and archive.get("final_observations_include_archive") is (not compact)
            and archive.get("file") == "render-camera-draws.jsonl"
            and archive.get("active_draw_capacity") == 4096 and archive.get("disk_budget_bytes") == ARCHIVE_BYTES,
            "invalid camera archive declaration")
    count, size = archive.get("archived_draws"), archive.get("bytes")
    require(integer(count) and integer(size) and size <= ARCHIVE_BYTES, "invalid camera archive counts")
    if compact:
        require(archive.get("encoding") == BLOCK_SCHEMA and report_bytes <= TAIL_BYTES
                and integer(archive.get("expanded_bytes"))
                and archive["expanded_bytes"] <= EXPANDED_REPORT_BYTES - report_bytes, "camera expansion exceeds bound")
    obs = report["observation"]
    active = obs["local_archive"]
    require(active.get("schema") == "dm-cp2077-camera-active-window/v1" and active.get("frozen") is True
            and active.get("enqueue_failures") == 0 and active.get("enqueued_draws") == count,
            "camera archive is not frozen and complete")
    tail = obs["observations"]
    require(isinstance(tail, list) and (len(tail) <= 4096 if compact else count <= len(tail) <= count + 4096),
            "camera archive/tail count mismatch")
    archive_path = path.with_name(archive["file"])
    require(regular(archive_path, ARCHIVE_BYTES) == size, "camera archive size mismatch")
    encoded_hash, expanded_hash = hashlib.sha256(), hashlib.sha256()
    consumed = expanded_bytes = blocks = seen = 0
    restored = []
    expanded_bound = EXPANDED_REPORT_BYTES if compact else REPORT_BYTES
    with archive_path.open("rb") as source:
        bound = BLOCK_BYTES if compact else ROW_BYTES
        while line := source.readline(bound + 1):
            consumed += len(line)
            require(len(line) <= bound and line.endswith(b"\n") and consumed <= ARCHIVE_BYTES,
                    "camera archive truncated/oversized")
            encoded_hash.update(line)
            raw_rows = decode_block(line) if compact else [line]
            blocks += int(compact)
            for raw in raw_rows:
                seen += 1; expanded_bytes += len(raw)
                require(seen <= count and expanded_bytes <= expanded_bound, "camera archive count/expansion exceeds bound")
                expanded_hash.update(raw)
                row = loads(raw)
                require(isinstance(row, dict), "camera draw is not an object")
                if compact:
                    restored.append(row)
                else:
                    require(row == tail[seen - 1], "camera archive row differs from final report")
    require(seen == count and consumed == size and archive["sha256"] == "sha256:" + encoded_hash.hexdigest(),
            "camera archive SHA/count mismatch")
    if compact:
        require(archive.get("blocks") == blocks and archive["expanded_bytes"] == expanded_bytes
                and expanded_bytes <= EXPANDED_REPORT_BYTES - report_bytes
                and archive.get("expanded_sha256") == "sha256:" + expanded_hash.hexdigest(),
                "camera archive expanded SHA/count mismatch")
        obs["observations"] = restored + tail
    require(path.read_bytes() == data, "camera report changed during archive read")
    return report

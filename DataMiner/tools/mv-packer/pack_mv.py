#!/usr/bin/env python
"""Pack converter output into the mv-gameplay dataset layout.

Byte-for-byte compatible with v2026.07, which was produced by a packer that no
longer exists; the format was recovered from the published artifacts (see
MV_PACKAGING_SPEC.md) and every constant here traces to something confirmed
there. Two of them are why this writes tar by hand instead of using `tarfile`:
`devmajor`/`devminor` are all-NUL rather than octal zero, and the archive ends
with exactly 1024 zero bytes rather than being padded to a 10240-byte record.

Usage: pack_mv.py <converted-root> <out-root> <version>

Input is one `dataminer-data-demo convert --output` tree per session:

  <converted-root>/<session_id>/metadata.jsonl        -> converter-metadata.jsonl
                              /_dataset_stats.json    -> converter-_dataset_stats.json
                              /<game_slug>/<session_id>/metadata.json
                                                       /framepairs.jsonl
                                                       /cNNNN/{p0.mp4,p0.jsonl,meta.json[,p0.flac]}

`samples.jsonl` is not converter output — it records where this packer placed
each chunk, so it is synthesized here.
"""
import datetime
import hashlib
import json
import math
import pathlib
import sys

import pyarrow as pa
import pyarrow.parquet as pq

from audio_contract import chunk_members, validate_session_audio

SRC = pathlib.Path(sys.argv[1])
OUT = pathlib.Path(sys.argv[2])
VERSION = sys.argv[3] if len(sys.argv) > 3 else "v2026.08"

SHARD_CAP = 1 << 30  # 1 GiB; the recovered split threshold
TEST_SESSIONS = 2  # sorted by id ascending, the last two go to test
EXCLUDED = [
    {"session_id": "dev-1785039658512-0000", "reason": "CAPTURE_QUEUE_DROPPED",
     "converter_rejected": True},
    {"session_id": "dev-1785045787023-0005", "reason": "INPUT_NOT_NEUTRAL",
     "converter_rejected": True},
]


# ---- tar ------------------------------------------------------------------

def tar_header(name: str, size: int) -> bytes:
    """One ustar header. Everything host-derived is zeroed so the archive is a
    pure function of its contents: mtime 0, uid/gid 0, uname/gname empty."""
    raw = name.encode()
    if len(raw) > 100:
        raise ValueError(f"name too long for ustar without a prefix: {name}")
    h = bytearray(512)
    h[0:len(raw)] = raw
    h[100:108] = b"0000644\0"
    h[108:116] = b"0000000\0"
    h[116:124] = b"0000000\0"
    h[124:136] = f"{size:011o}\0".encode()
    h[136:148] = b"00000000000\0"
    h[148:156] = b" " * 8  # checksum field is spaces while summing
    h[156] = 0x30  # typeflag '0'
    h[257:263] = b"ustar\0"
    h[263:265] = b"00"
    # uname/gname/devmajor/devminor/prefix stay NUL — see module docstring.
    h[148:156] = f"{sum(h):06o}\0 ".encode()
    return bytes(h)


def member_cost(size: int) -> int:
    return 512 + math.ceil(size / 512) * 512


class Shard:
    def __init__(self, path: pathlib.Path):
        self.path = path
        self.fh = path.open("wb")
        self.digest = hashlib.sha256()
        self.bytes = 0
        self.entries = 0

    def add(self, name: str, data: bytes) -> None:
        pad = b"\0" * (-len(data) % 512)
        for blob in (tar_header(name, len(data)), data, pad):
            self.fh.write(blob)
            self.digest.update(blob)
            self.bytes += len(blob)
        self.entries += 1

    def close(self) -> dict:
        trailer = b"\0" * 1024  # exactly two blocks, no record padding
        self.fh.write(trailer)
        self.digest.update(trailer)
        self.bytes += len(trailer)
        self.fh.close()
        return {"name": self.path.name, "bytes": self.bytes,
                "sha256": self.digest.hexdigest(), "entry_count": self.entries}


# ---- json / jsonl style ---------------------------------------------------

def write_json(path: pathlib.Path, obj, trailing_newline: bool = True) -> None:
    text = json.dumps(obj, indent=2, ensure_ascii=False)
    path.write_text(text + ("\n" if trailing_newline else ""), encoding="utf-8")


def write_jsonl(path: pathlib.Path, rows) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as fh:
        for row in rows:
            fh.write(json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n")


def read_jsonl(path: pathlib.Path):
    with path.open(encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


# ---- pack -----------------------------------------------------------------

sessions = sorted(p.name for p in SRC.iterdir() if p.is_dir())
split_of = {sid: ("test" if i >= len(sessions) - TEST_SESSIONS else "train")
            for i, sid in enumerate(sessions)}

manifest_sessions, all_samples, all_pairs, audio_sessions = [], [], [], []
audio_mode = None

for sid in sessions:
    sdir = SRC / sid
    split = split_of[sid]
    game_slug = next(p.name for p in sdir.iterdir() if p.is_dir())
    inner = sdir / game_slug / sid
    chunk_dirs = sorted(p for p in inner.iterdir() if p.is_dir() and p.name.startswith("c"))
    metadata = json.loads((inner / "metadata.json").read_text(encoding="utf-8"))
    session_audio = metadata.get("audio")
    session_audio_mode = session_audio is not None
    if audio_mode is None:
        audio_mode = session_audio_mode
    elif audio_mode != session_audio_mode:
        raise ValueError(
            "one mv-gameplay version may not mix audio and legacy sessions; "
            "publish them under separate dataset configs/versions"
        )
    out_dir = OUT / VERSION / split / sid
    out_dir.mkdir(parents=True, exist_ok=True)

    shards, shard = [], None
    per_chunk_shard, per_chunk_bytes, per_chunk_meta, per_chunk_audio = {}, {}, {}, {}
    for chunk_dir in chunk_dirs:
        key = f"{sid}_{chunk_dir.name}"
        members, chunk_meta, chunk_audio = chunk_members(
            chunk_dir, key, session_audio is not None)
        # `bytes` in the index is the whole chunk, all three members — confirmed
        # against the reference release's own numbers.
        per_chunk_bytes[chunk_dir.name] = sum(len(data) for _, data in members)
        per_chunk_meta[chunk_dir.name] = chunk_meta
        per_chunk_audio[chunk_dir.name] = chunk_audio
        cost = sum(member_cost(len(data)) for _, data in members)
        # Look-ahead: roll before exceeding the cap, never after — no shard in
        # the reference release is larger than the cap.
        if shard is not None and shard.bytes + cost + 1024 > SHARD_CAP:
            shards.append(shard.close())
            shard = None
        if shard is None:
            shard = Shard(out_dir / f"shard-{len(shards):03d}.tar")
        for name, data in members:
            shard.add(name, data)
        per_chunk_shard[chunk_dir.name] = shard.path.name
    if shard is not None:
        shards.append(shard.close())

    stats = json.loads((sdir / "_dataset_stats.json").read_text(encoding="utf-8"))
    validate_session_audio(session_audio, list(per_chunk_audio.values()))
    if session_audio is not None:
        audio_sessions.append({"session_id": sid, **session_audio})
    manifest_session = {
        "session_id": sid,
        "split": split,
        "chunk_count": len(chunk_dirs),
        "shards": shards,
        "seconds": round(stats.get("total_hours", 0) * 3600),
    }
    if session_audio is not None:
        manifest_session["audio"] = session_audio
    manifest_sessions.append(manifest_session)

    # Per-session index, verbatim copies of what the converter emitted plus the
    # shard placement this packer just decided.
    idx = OUT / VERSION / "index" / "sessions" / sid
    idx.mkdir(parents=True, exist_ok=True)
    write_json(idx / "metadata.json", metadata)
    # Carried through byte-for-byte rather than re-serialized: it is the
    # converter's own record, and the published pair is identical content.
    (idx / "converter-metadata.jsonl").write_bytes((sdir / "metadata.jsonl").read_bytes())
    write_json(idx / "converter-_dataset_stats.json", stats)
    session_index = {
        "schema": "dm-mv-session-index-2" if session_audio is not None else "dm-mv-session-index-1",
        "session_id": sid,
        "game_slug": game_slug,
        "split": split,
        "is_trainable": False,
        "method_validation_only": True,
        "chunk_count": len(chunk_dirs),
        "shards": shards,
    }
    if session_audio is not None:
        session_index["audio"] = session_audio
    write_json(idx / "session-index.json", session_index)

    samples = []
    for chunk_dir in chunk_dirs:
        audio = per_chunk_audio[chunk_dir.name]
        sample = {
            "session_id": sid,
            "chunk": chunk_dir.name,
            "split": split,
            "shard": per_chunk_shard[chunk_dir.name],
            "game_slug": game_slug,
            "bytes": per_chunk_bytes[chunk_dir.name],
            "is_trainable": False,
            "method_validation_only": True,
            "meta": per_chunk_meta[chunk_dir.name],
        }
        if session_audio is not None:
            sample.update({
                "audio_present": audio["present"],
                "audio_state": audio["state"],
                "audio_codec": audio.get("codec"),
                "audio_sha256": audio.get("sha256"),
            })
        samples.append(sample)
    write_jsonl(idx / "samples.jsonl", samples)
    all_samples.extend(samples)

    pairs = read_jsonl(inner / "framepairs.jsonl")
    write_jsonl(idx / "framepairs.jsonl", pairs)
    all_pairs.extend({**p, "session_id": sid} for p in pairs)

# ---- index parquet --------------------------------------------------------
# `meta` and `actionT` are objects in the jsonl and JSON strings in the parquet;
# the reference used json.dumps defaults there, spaces and all.
index_dir = OUT / VERSION / "index"
sample_rows = [{**r, "meta": json.dumps(r["meta"], ensure_ascii=False)} for r in all_samples]
sample_schema = [
    ("session_id", pa.string()), ("chunk", pa.string()), ("split", pa.string()),
    ("shard", pa.string()), ("game_slug", pa.string()), ("bytes", pa.int64()),
    ("is_trainable", pa.bool_()), ("method_validation_only", pa.bool_()),
    ("meta", pa.string()),
]
if audio_sessions:
    sample_schema.extend([
        ("audio_present", pa.bool_()), ("audio_state", pa.string()),
        ("audio_codec", pa.string()), ("audio_sha256", pa.string()),
    ])
    for row in sample_rows:
        row.setdefault("audio_present", False)
        row.setdefault("audio_state", None)
        row.setdefault("audio_codec", None)
        row.setdefault("audio_sha256", None)
pq.write_table(
    pa.Table.from_pylist(sample_rows, schema=pa.schema(sample_schema)),
    index_dir / "samples.parquet", compression="snappy")

pair_rows = [{**p, "actionT": json.dumps(p["actionT"], ensure_ascii=False)} for p in all_pairs]
pq.write_table(
    pa.Table.from_pylist(pair_rows, schema=pa.schema([
        ("clipId", pa.string()), ("consentId", pa.null()), ("frameT", pa.int64()),
        ("gapH", pa.int64()), ("chunkT", pa.int64()), ("chunkTH", pa.int64()),
        ("actionT", pa.string()), ("session_id", pa.string()),
    ])),
    index_dir / "framepairs.parquet", compression="snappy")

# index/shards.json is the only file in the tree without a trailing newline.
write_json(index_dir / "shards.json",
           [{"session_id": s["session_id"], "split": s["split"], **shard}
            for s in manifest_sessions for shard in s["shards"]],
           trailing_newline=False)


# ---- datasheet ------------------------------------------------------------
# v2026.07 registered a DATASHEET.md in index_files (produced by the since-lost
# packer); this restores the file and its registration (docs/148 A2). The
# aggregation section mirrors the converter card — normative sources are
# docs/02 §3-§4 and docs/58 §1.4; docs/52 is the byte-level tar spec.

def datasheet(sessions, audio: bool) -> str:
    total_chunks = sum(s["chunk_count"] for s in sessions)
    total_shards = sum(len(s["shards"]) for s in sessions)
    member_order = "`p0.mp4` + `p0.jsonl` + `meta.json`" + (" + `p0.flac` (audio state=ready only)" if audio else "")
    return f"""# mv-gameplay {VERSION}

Method-validation WebDataset packaging of DataMiner dm-2 chunk trees.
`is_trainable=false` for every sample; method validation only.

## Layout

- One `~1 GiB` look-ahead-rolled `shard-NNN.tar` series per session under
  `{{split}}/{{session_id}}/`; a shard never crosses a session (revocation unit).
- Tar members are flat basenames grouped by sample key `{{session_id}}_cNNNN`
  (rocket-science key convention): {member_order}.
- Every chunk is one native 4-second capture GOP, remuxed without re-encoding;
  each `p0.mp4` starts with an IDR sample and decodes independently.
- Indexes: `index/shards.json` (shard registry), `index/samples.parquet`
  (one row per chunk), `index/framepairs.parquet`; per-session copies under
  `index/sessions/<session_id>/`. Byte-level spec: docs/52.
- Totals: {len(sessions)} sessions / {total_chunks} chunks / {total_shards} shards.

## Action aggregation & downsampling

How every `p0.jsonl` row was aggregated (normative: docs/02 §3-§4; docs/58 §1.4):

- **window**: each input event lands in exactly one frame window by
  `qpc ∈ [output_qpc_start, output_qpc_end)` (half-open).
- **`keys` / `buttons`**: the sets still **held at window end**, sorted ascending —
  not "pressed during the window". A fast press-and-release inside one window
  appears only in `events`.
- **`pointerDx/Dy`**: raw relative counts **summed over the window, then clamped
  once to ±200** — unsuitable as regression targets (the clamp hits exactly the
  fast-flick frames); regress on the unclamped `pointerRawDx/Dy` where present,
  else re-derive by summing `events` mouse deltas per window.
- **`pointerBins`**: mu-law bins of the clamped sums, binned exactly once
  (scheme `dm1-v2`: mu=8, ref=200, max_bin=10). Bin schemes are generational
  (`dm1-v2` vs `legacy`) and **not interchangeable**: cross-corpus consumers drop
  stored bins and re-derive from raw deltas.
- **`duplicate`**: a CFR pacer fact recorded at capture time; never inferred
  from pixels.
- **Downsampling to a lower fps**: build target windows on time; **sum** pointer
  displacement per window (sampling narrows the delta distribution); key/button
  channels become the in-window **coverage ratio in [0,1]** (OR would stretch a
  tap to the whole window — binarize with `>0` if needed); never re-clamp
  already-clamped values; cut video and labels with the same window plan;
  upsampling is not defined for this data.
"""


(OUT / VERSION / "DATASHEET.md").write_text(
    datasheet(manifest_sessions, bool(audio_sessions)), encoding="utf-8")


def digest(path: pathlib.Path) -> dict:
    raw = path.read_bytes()
    return {"sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}


manifest = {
    "schema": "dm-mv-manifest-2" if audio_sessions else "dm-mv-manifest-1",
    "dataset": "mv-gameplay",
    "version": VERSION,
    "is_trainable": False,
    "method_validation_only": True,
    # Packing time, not recording time — the recordings carry no wall clock of
    # their own (that is what `t_utc_est_us` in each frame estimates).
    "created_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "source": {
        "origin": "internal",
        "note": "source details withheld (desensitized); full provenance kept internally",
    },
    "converter": {
        "tool": "internal byte-deterministic converter (non-trainable debug profile)",
        "determinism_check": True,
    },
    "split_rule": "completed sessions sorted by id ascending; last 2 -> test; "
                  "split by session (no chunk-level mixing)",
    "sessions": manifest_sessions,
    "excluded_sessions": EXCLUDED,
    "totals": {
        "sessions": len(manifest_sessions),
        "chunks": sum(s["chunk_count"] for s in manifest_sessions),
        "shards": sum(len(s["shards"]) for s in manifest_sessions),
        "shard_bytes": sum(sh["bytes"] for s in manifest_sessions for sh in s["shards"]),
    },
    "index_files": {
        "index/shards.json": digest(index_dir / "shards.json"),
        "index/samples.parquet": digest(index_dir / "samples.parquet"),
        "index/framepairs.parquet": digest(index_dir / "framepairs.parquet"),
        # Restored registration (docs/148 A2): v2026.07 carried it, the packer
        # rewrite dropped it together with the generator.
        "DATASHEET.md": digest(OUT / VERSION / "DATASHEET.md"),
    },
}
if audio_sessions:
    manifest["dataset_config"] = {
        "schema": "dm-mv-dataset-config-2",
        "chunk_member_order": ["p0.mp4", "p0.jsonl", "meta.json", "p0.flac?"],
        "audio_optional_per_chunk": True,
    }
    manifest["audio"] = {
        "source": "process_loopback",
        "codec": "flac",
        "sample_rate": 48_000,
        "channels": 2,
        "bits": 16,
        "sample_format": "pcm_s16le",
        "sessions": len(audio_sessions),
        "ready_chunks": sum(item["ready_chunks"] for item in audio_sessions),
        "degraded_chunks": sum(item["degraded_chunks"] for item in audio_sessions),
        "absent_chunks": sum(item["absent_chunks"] for item in audio_sessions),
        "all_publishable": all(item["all_publishable"] for item in audio_sessions),
    }
write_json(OUT / VERSION / "MANIFEST.json", manifest)
print(json.dumps(manifest["totals"], indent=2))

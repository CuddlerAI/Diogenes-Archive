# DataMiner dataset normalizer

Offline-only normalization for exact `60/1` CFR RAW timing. Version 1 supports
only `60/1 -> 30/1`, `20/1`, or `12/1` and always emits a non-trainable,
non-certified artifact. It does not change the capture path or the native
`data-demo-converter` remux path.

The shipped specs under `specs/` freeze the QPC grid, source-frame selector,
RAW-event projection, pointer accumulation, trim, duplicate bookkeeping,
FramePair deferral, and offline encoder implementation. Unknown or changed
policy strings fail closed. The exact spec bytes are copied into every output
and bound by SHA-256 in `metadata.json` and `MANIFEST.json`.

## Request contract

`dm-normalization-request-1` is deliberately independent of catalog schema v1
or v2. A future Forge/session adapter can produce it without teaching this
crate catalog delivery rules. Paths are relative to the request file unless
absolute.

```json
{
  "schema_version": "dm-normalization-request-1",
  "session_id": "session-001",
  "source_fps": { "fps_num": 60, "fps_den": 1 },
  "source_width": 1920,
  "source_height": 1080,
  "mapping_path": "mapping.json",
  "runs": [
    {
      "run_id": "r0000",
      "qpc_anchor": 1000000,
      "qpc_frequency": 10000000,
      "segments": [
        {
          "timing_path": "runs/r0000/segments/00000/timing.jsonl.zst",
          "events_path": "runs/r0000/segments/00000/events.jsonl.zst",
          "video_path": "runs/r0000/segments/00000/video.mp4"
        }
      ]
    }
  ]
}
```

Each timing file contains the serialized `capture_core::projection::FrameTiming`
shape. Segment-local `frame` restarts at zero; `session_frame`, QPC windows, and
source identity remain monotonic across segments in one run. Pause/resume starts
a new run and a new QPC anchor.

## Commands

```powershell
cargo run -p dataset-normalizer --bin dataminer-dataset-normalizer -- plan `
  --request request.json --spec specs/fps60-to-training30-v1.json

cargo run -p dataset-normalizer --bin dataminer-dataset-normalizer -- normalize `
  --request request.json --spec specs/fps60-to-training30-v1.json `
  --output normalized --ffmpeg C:\tools\ffmpeg.exe `
  --ffmpeg-sha256 sha256:<exact-binary-hash>

cargo run -p dataset-normalizer --bin dataminer-dataset-normalizer -- verify `
  --dataset normalized
```

The worker refuses an FFmpeg executable whose bytes do not match the caller's
exact SHA-256. It decodes offline, selects the representative source CFR slots
from the frame-plan, resets PTS to the target CFR, and encodes H.264 High with a
four-second closed GOP, no B-frames, scene-cut keyframes disabled, and one
encoder thread. `verify` independently reads the MP4 sample table and H.264 IDR
NAL units, then closes frame count, exact fps, duration, PTS, dimensions, GOP,
first IDR, labels, plan, metadata, and manifest hashes.

The source MP4 sample table is also checked independently: every sample must
have contiguous PTS and the exact `60/1` duration declared by the request and
spec. A matching frame count is not enough; 59.94, 30fps, and VFR media fail
closed.

The real FFmpeg worker has an explicit short integration test. It is ignored by
default because the repository does not distribute or silently select an
encoder binary; the caller must pin both path and exact bytes:

```powershell
$env:DATAMINER_TEST_FFMPEG = 'C:\tools\ffmpeg.exe'
$env:DATAMINER_TEST_FFMPEG_SHA256 = 'sha256:<64-lowercase-hex>'
cargo test -p dataset-normalizer `
  artifact::tests::pinned_ffmpeg_worker_closes_all_targets_and_is_deterministic `
  -- --ignored --exact --nocapture
```

That test generates a 61-frame exact-60/1 H.264 fixture, normalizes it twice to
30/20/12fps, and independently verifies frame/label/plan counts, tail trim,
PTS, GOP/IDR, and repeat-run artifact hashes.

No FramePair file is emitted. Duplicate observations are preserved and marked,
but owner review is still pending; those two facts are why v1 cannot claim
trainability or production 60fps admission.

import { describe, expect, it } from "vitest";

import {
  completeUploadIntentSchema,
  createSessionSchemaFor,
  putClipSchemaFor,
  putRunSchemaFor,
  requestClipPurgeSchema,
} from "./contracts.js";
import { environmentProfile, primaryFamily, storagePolicyForSessionSchema } from "./environment.js";

// This file exercises the production admission envelope; the staging-bound
// convenience exports are covered transitively (same schema factory).
const production = primaryFamily(environmentProfile("production"));
const createSessionSchema = createSessionSchemaFor(production);
const putClipSchema = putClipSchemaFor(production);
// 家族只剩一个：consented first-party 那条随 docs/57 §3.3 退役。
const putRunSchema = putRunSchemaFor([production.preflightResult]);

const digest = `sha256:${"a".repeat(64)}`;

function validSessionDeclaration() {
  const profile = (id: string) => ({ id, version: "1", contentHash: digest });
  return {
    localSessionId: "dev-1784791023860-0000",
    sessionSchemaVersion: "dm-local-dev-session-2",
    startedAt: "2026-07-23T00:00:00.000Z",
    localState: "completed",
    classification: {
      sourceFamily: "dm-local-dev",
      contractVersion: "dm-1",
      artifactSchemaVersion: "dm-local-dev-recording-1",
      environment: "production",
      storageNamespace: "raw",
      nonTrainable: true,
      isTrainable: false,
      productionCertified: false,
    },
    clientVersion: "0.1.0",
    catalog: {
      id: "dataminer-dev-synthetic",
      version: "1",
      mode: "dev_non_trainable",
      contentHash: digest,
    },
    game: {
      id: "synthetic-window",
      version: "1",
      contentHash: digest,
      executableSha256: digest,
    },
    deviceProfile: profile("synthetic-device"),
    recordingProfile: {
      ...profile("synthetic-1080p60"),
      width: 1920,
      height: 1080,
      fpsNum: 60,
      fpsDen: 1,
      captureEngine: "native_wgc_window_v1",
    },
    mapping: profile("synthetic-mapping"),
  };
}



function validClipDeclaration() {
  const artifact = () => ({ byteSize: 128, sha256: digest });
  return {
    classification: {
      sourceFamily: "dm-local-dev",
      contractVersion: "dm-1",
      artifactSchemaVersion: "dm-local-dev-recording-1",
      environment: "production",
      storageNamespace: "raw",
      nonTrainable: true,
      isTrainable: false,
      productionCertified: false,
    },
    localArtifactState: "ready",
    sessionFrameStart: 0,
    nFrames: 60,
    qpcStart: 10,
    qpcEnd: 20,
    eventCount: 3,
    health: {
      duplicateFrames: 0,
      sourceSuperseded: 0,
      queueDrops: 0,
      inputOverflow: 0,
    },
    artifacts: {
      video: artifact(),
      frames: artifact(),
      events: artifact(),
      meta: artifact(),
      manifest: artifact(),
    },
  };
}

function validAudioClipDeclaration(state: "ready" | "degraded" | "absent" = "ready") {
  const input = validClipDeclaration();
  const present = state !== "absent";
  const health: {
    discontinuities: Array<{ start_frame: number; frames: number }>;
    silence_filled_spans: Array<{ start_frame: number; frames: number }>;
    queue_overflows: number;
    timestamp_errors: number;
    max_qpc_drift_us: number;
  } = {
    discontinuities: [],
    silence_filled_spans: [],
    queue_overflows: 0,
    timestamp_errors: 0,
    max_qpc_drift_us: 0,
  };
  return {
    ...input,
    classification: {
      ...input.classification,
      artifactSchemaVersion: "dm-local-dev-recording-2",
    },
    audio: {
      state,
      source: "process_loopback",
      ...(present ? { path: "audio.pcm.zst", sha256: digest, bytes: 128 } : {}),
      format: {
        sample_rate: 48_000,
        channels: 2,
        bits: 16,
        sample_format: "pcm_s16le",
        channel_mask: 3,
        requested_format: "pcm_s16le/48000/2",
        system_src: true,
      },
      qpc_frequency: 10_000_000,
      ...(present ? { first_sample_qpc: 10, frames: 48_000 } : {}),
      ...(state === "ready"
        ? {}
        : {
            failure_code:
              state === "degraded" ? "AUDIO_BOUNDED_GAP_FILLED" : "AUDIO_INIT_FAILED",
          }),
      health,
    },
    artifacts: {
      ...input.artifacts,
      ...(present ? { audio: { byteSize: 128, sha256: digest } } : {}),
    },
  };
}

const depthFormats = ["dm-depth-variant-r-zip-1", "dm-depth-raw-zip-2"] as const;

function withReadyDepth<T extends ReturnType<typeof validClipDeclaration>>(
  input: T = validClipDeclaration() as T,
  format: typeof depthFormats[number] = "dm-depth-variant-r-zip-1",
) {
  return {
    ...input,
    depth: {
      state: "ready" as const,
      source: "depth_probe" as const,
      path: "depth.zip" as const,
      sha256: digest,
      bytes: 128,
      format,
    },
    artifacts: {
      ...input.artifacts,
      depth: { byteSize: 128, sha256: digest },
    },
  };
}

describe("production non-trainable clip contract", () => {
  it("accepts only the fixed non-trainable classification", () => {
    const input = validClipDeclaration();
    expect(putClipSchema.safeParse(input).success).toBe(true);

    const trainable = {
      ...input,
      classification: {
        ...input.classification,
        nonTrainable: false,
        isTrainable: true,
        productionCertified: true,
      },
    };
    expect(putClipSchema.safeParse(trainable).success).toBe(false);
  });

  it("rejects a declaration missing any member of the five-artifact bundle", () => {
    const input = validClipDeclaration();
    const { manifest: _manifest, ...incompleteArtifacts } = input.artifacts;

    expect(
      putClipSchema.safeParse({ ...input, artifacts: incompleteArtifacts }).success,
    ).toBe(false);
  });

  it("rejects a naked hexadecimal digest without the sha256 prefix", () => {
    const input = validClipDeclaration();
    input.artifacts.video.sha256 = "b".repeat(64);

    expect(putClipSchema.safeParse(input).success).toBe(false);
  });

  it("accepts recording-v2 with a matching optional audio sidecar or explicit absence", () => {
    expect(putClipSchema.safeParse(validAudioClipDeclaration("ready")).success).toBe(true);
    expect(putClipSchema.safeParse(validAudioClipDeclaration("absent")).success).toBe(true);

    const mismatched = validAudioClipDeclaration("ready");
    delete (mismatched.artifacts as { audio?: unknown }).audio;
    expect(putClipSchema.safeParse(mismatched).success).toBe(false);
  });

  it("keeps the server audio health gate aligned with the RAW contract", () => {
    const degraded = validAudioClipDeclaration("degraded");
    degraded.audio.health.discontinuities = [{ start_frame: 10_000, frames: 240 }];
    degraded.audio.health.silence_filled_spans = [{ start_frame: 10_000, frames: 240 }];
    expect(putClipSchema.safeParse(degraded).success).toBe(true);

    const tooLarge = structuredClone(degraded);
    tooLarge.audio.health.silence_filled_spans[0]!.frames = 4_801;
    expect(putClipSchema.safeParse(tooLarge).success).toBe(false);

    const outsideSegment = structuredClone(degraded);
    outsideSegment.audio.health.silence_filled_spans[0] = {
      start_frame: 47_900,
      frames: 240,
    };
    expect(putClipSchema.safeParse(outsideSegment).success).toBe(false);

    const overflowed = structuredClone(degraded);
    overflowed.audio.health.queue_overflows = 1;
    expect(putClipSchema.safeParse(overflowed).success).toBe(false);
  });

  it.each(depthFormats)("accepts %s only when depth metadata and the artifact match exactly", (format) => {
    const ready = withReadyDepth(validClipDeclaration(), format);
    expect(putClipSchema.safeParse(ready).success).toBe(true);

    const withAudio = withReadyDepth(validAudioClipDeclaration("ready"), format);
    expect(putClipSchema.safeParse(withAudio).success).toBe(true);

    const missingObject = structuredClone(ready);
    delete (missingObject.artifacts as { depth?: unknown }).depth;
    expect(putClipSchema.safeParse(missingObject).success).toBe(false);

    const { depth: _depth, ...missingMetadata } = structuredClone(ready);
    expect(putClipSchema.safeParse(missingMetadata).success).toBe(false);

    const mismatchedHash = structuredClone(ready);
    mismatchedHash.depth.sha256 = `sha256:${"b".repeat(64)}`;
    expect(putClipSchema.safeParse(mismatchedHash).success).toBe(false);

    const mismatchedBytes = structuredClone(ready);
    mismatchedBytes.depth.bytes += 1;
    expect(putClipSchema.safeParse(mismatchedBytes).success).toBe(false);

    expect(
      putClipSchema.safeParse({
        ...validClipDeclaration(),
        depth: { state: "absent" },
      }).success,
    ).toBe(true);
  });

  it.each(depthFormats)("preserves the %s declaration on the neutral ingest lane", (format) => {
    const family = storagePolicyForSessionSchema(environmentProfile("production"), "dm-session-ingest-2");
    const input = {
      ...withReadyDepth(validClipDeclaration(), format),
      classification: family.admission,
    };
    const parsed = putClipSchemaFor(family).parse(input);
    expect(parsed.depth).toEqual(input.depth);
    expect(parsed.classification).toEqual(family.admission);
  });

  it("refuses unknown depth formats instead of treating them as legacy or raw v2", () => {
    const ready = withReadyDepth();
    for (const format of ["dm-depth-raw-zip-3", "dm-depth-variant-r-zip-2", "", undefined]) {
      expect(putClipSchema.safeParse({
        ...ready,
        depth: { ...ready.depth, format },
      }).success).toBe(false);
    }
  });

  it.each(depthFormats)("keeps %s source, path, hash, size and metadata fields strict", (format) => {
    const ready = withReadyDepth(validClipDeclaration(), format);
    for (const patch of [
      { source: "estimated_depth" },
      { path: "../depth.zip" },
      { sha256: "a".repeat(64) },
      { bytes: 0 },
      { bytes: 1.5 },
      { bytes: 5 * 1024 ** 4 + 1 },
      { metricDepthVerified: true },
    ]) {
      const depth = { ...ready.depth, ...patch };
      expect(putClipSchema.safeParse({
        ...ready,
        depth,
        // Match the duplicate artifact declaration so these cases exercise the
        // value/identity bounds, not merely the metadata/artifact equality gate.
        artifacts: {
          ...ready.artifacts,
          depth: { byteSize: depth.bytes, sha256: depth.sha256 },
        },
      }).success).toBe(false);
    }
  });

  it("requires completion evidence for all five artifacts", () => {
    const part = { parts: [{ partNumber: 1, etag: "etag-1" }] };
    const complete = {
      artifacts: {
        video: part,
        frames: part,
        events: part,
        meta: part,
        manifest: part,
      },
    };
    expect(completeUploadIntentSchema.safeParse(complete).success).toBe(true);
    expect(
      completeUploadIntentSchema.safeParse({
        artifacts: { ...complete.artifacts, depth: part },
      }).success,
    ).toBe(true);

    const { manifest: _manifest, ...incomplete } = complete.artifacts;
    expect(
      completeUploadIntentSchema.safeParse({ artifacts: incomplete }).success,
    ).toBe(false);
  });

  it("accepts only the fixed device deletion reason", () => {
    expect(
      requestClipPurgeSchema.safeParse({ reason: "device_principal_delete" }).success,
    ).toBe(true);
    expect(requestClipPurgeSchema.safeParse({ reason: "because I said so" }).success).toBe(false);
  });
});

describe("T14 session v2 contract", () => {
  it("requires all six exact-byte catalog identities without accepting their local files", () => {
    const input = validSessionDeclaration();
    expect(createSessionSchema.safeParse(input).success).toBe(true);

    expect(
      createSessionSchema.safeParse({ ...input, sessionSchemaVersion: "dm-local-dev-session-1" }).success,
    ).toBe(false);
    const { catalog: _catalog, ...missingCatalog } = input;
    expect(createSessionSchema.safeParse(missingCatalog).success).toBe(false);
    expect(
      createSessionSchema.safeParse({
        ...input,
        deviceProfile: { ...input.deviceProfile, snapshot: { injected: true } },
      }).success,
    ).toBe(false);
    expect(
      createSessionSchema.safeParse({
        ...input,
        mapping: { ...input.mapping, snapshot: { injected: true } },
      }).success,
    ).toBe(false);
  });

  it("accepts the audio session/recording schema pair and rejects mixed generations", () => {
    const input = validSessionDeclaration();
    const audio = {
      ...input,
      sessionSchemaVersion: "dm-local-dev-session-3",
      classification: {
        ...input.classification,
        artifactSchemaVersion: "dm-local-dev-recording-2",
      },
    };
    expect(createSessionSchema.safeParse(audio).success).toBe(true);
    expect(
      createSessionSchema.safeParse({
        ...audio,
        classification: input.classification,
      }).success,
    ).toBe(false);
  });

  it("never accepts a certified local catalog claim in the non-trainable path", () => {
    const input = validSessionDeclaration();
    expect(
      createSessionSchema.safeParse({ ...input, catalog: { ...input.catalog, mode: "certified" } }).success,
    ).toBe(false);
  });

  it("rejects name-only executable evidence from the SHA-256 field", () => {
    const input = validSessionDeclaration();
    expect(createSessionSchema.safeParse(input).success).toBe(true);
    expect(
      createSessionSchema.safeParse({
        ...input,
        game: { ...input.game, executableSha256: "name-only:cod.exe" },
      }).success,
    ).toBe(false);
  });

  // docs/55：指纹变可空后，"缺席"必须携带原因，且二者严格互斥。少了互斥，
  // 一个漏报指纹的客户端就能安静地登记一条既无指纹也无解释的会话。
  it("accepts a missing fingerprint only when a reason code explains it", () => {
    const input = validSessionDeclaration();
    const withGame = (game: Record<string, unknown>) =>
      createSessionSchema.safeParse({ ...input, game: { ...input.game, ...game } }).success;

    expect(withGame({ executableSha256: undefined, executableSha256MissingReason: "open_process_denied:5" })).toBe(true);
    // 两边都缺 = 漏报；两边都在 = 自相矛盾。
    expect(withGame({ executableSha256: undefined })).toBe(false);
    expect(withGame({ executableSha256MissingReason: "open_process_denied:5" })).toBe(false);
  });

  // 自由文本是有意的（负责人裁决 2026-07-28）：标注员是内部员工，排查"指纹为
  // 什么没测到"时完整路径与系统错误原文远比错误码有用。上限与隐私无关，只防
  // 失控的错误链把列撑爆。
  it("accepts a full error chain as the reason, bounded in length", () => {
    const input = validSessionDeclaration();
    const withReason = (executableSha256MissingReason: string) =>
      createSessionSchema.safeParse({
        ...input,
        game: { ...input.game, executableSha256: undefined, executableSha256MissingReason },
      }).success;

    expect(
      withReason(
        "open_process_denied: OpenProcess(9284) failed: Access is denied. (os error 5)",
      ),
    ).toBe(true);
    expect(
      withReason(
        "executable_read_failed: failed to open C:\\Users\\Example\\Games\\cod.exe (os error 32)",
      ),
    ).toBe(true);
    expect(withReason("")).toBe(false);
    expect(withReason("   ")).toBe(false);
    expect(withReason("x".repeat(2000))).toBe(true);
    expect(withReason("x".repeat(2001))).toBe(false);
  });

  it("enforces the 30 fps floor the client and docs/03 already promise", () => {
    // docs/50 P-5a：服务端此前只挡 >=1fps，弱于客户端三处 >=30 的硬门。
    const input = validSessionDeclaration();
    const withFps = (fpsNum: number, fpsDen: number) => ({
      ...input,
      recordingProfile: { ...input.recordingProfile, fpsNum, fpsDen },
    });
    expect(createSessionSchema.safeParse(withFps(30, 1)).success).toBe(true);
    expect(createSessionSchema.safeParse(withFps(60, 1)).success).toBe(true);
    expect(createSessionSchema.safeParse(withFps(60_000, 1_001)).success).toBe(true);
    expect(createSessionSchema.safeParse(withFps(29, 1)).success).toBe(false);
    expect(createSessionSchema.safeParse(withFps(1, 1)).success).toBe(false);
  });
});

describe("run DTO and SQL boundary", () => {
  const validRun = {
    sequence: 0,
    localState: "completed",
    startedAt: "2026-07-23T00:00:00.000Z",
    endedAt: "2026-07-23T00:00:01.000Z",
    qpcFrequency: 10_000_000,
    sessionFrameStart: 0,
    preflight: {
      schemaVersion: "dm-preflight-evidence-1",
      id: "preflight-abc123",
      result: "dev_non_trainable_pass",
      evidenceHash: digest,
    },
    capture: {
      width: 1920,
      height: 1080,
      fpsNum: 60,
      fpsDen: 1,
      captureEngine: "native_wgc_window_v1",
      encoderBackend: "media-foundation-hardware-h264",
    },
  };

  it("accepts a cleanly paused local run as a closed upload source", () => {
    expect(putRunSchema.safeParse({ ...validRun, localState: "paused" }).success).toBe(true);
  });

  // docs/67 §3.2：run 结果三字段可选——老客户端（≤0.6.1）根本不发，strict 对象
  // 要求它们就等于把已发布客户端的 run PUT 全部打回。
  it("takes the run outcome when the client has it and stays valid when it does not", () => {
    expect(
      putRunSchema.safeParse({
        ...validRun,
        frameCount: 20_400,
        stopReason: "capture_fault",
        errorCode: "CAPTURE_QUEUE_DROPPED",
      }).success,
    ).toBe(true);
    expect(putRunSchema.safeParse(validRun).success).toBe(true);
    // 空串不是「没有」，它会在库里变成一条查不出所以然的假记录。
    expect(putRunSchema.safeParse({ ...validRun, stopReason: "   " }).success).toBe(false);
    expect(putRunSchema.safeParse({ ...validRun, frameCount: -1 }).success).toBe(false);
    expect(putRunSchema.safeParse({ ...validRun, errorCode: "E".repeat(121) }).success).toBe(false);
  });

  it("rejects values that PostgreSQL would reject instead of turning them into a 500", () => {
    expect(putRunSchema.safeParse({ ...validRun, sequence: 2_147_483_648 }).success).toBe(false);
    expect(
      putRunSchema.safeParse({ ...validRun, endedAt: "2026-07-22T23:59:59.000Z" }).success,
    ).toBe(false);
    expect(
      putRunSchema.safeParse({ ...validRun, capture: { ...validRun.capture, fpsNum: 1, fpsDen: 2 } }).success,
    ).toBe(false);
    // docs/50 P-5a：run 侧同款 30fps 下限
    expect(
      putRunSchema.safeParse({ ...validRun, capture: { ...validRun.capture, fpsNum: 29, fpsDen: 1 } }).success,
    ).toBe(false);
    expect(
      putRunSchema.safeParse({ ...validRun, capture: { ...validRun.capture, fpsNum: 30, fpsDen: 1 } }).success,
    ).toBe(true);
    expect(
      putRunSchema.safeParse({ ...validRun, capture: { ...validRun.capture, encoderBackend: "   " } }).success,
    ).toBe(false);
    expect(
      putRunSchema.safeParse({
        ...validRun,
        preflight: { ...validRun.preflight, result: "certified_pass" },
      }).success,
    ).toBe(false);
  });
});

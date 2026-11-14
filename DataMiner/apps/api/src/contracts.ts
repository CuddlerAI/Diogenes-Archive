import { z } from "zod";

import { environmentProfile, primaryFamily, type AdmissionFamily } from "./environment.js";

import { SHA256_PATTERN } from "./checksum.js";

export const requiredArtifactKinds = ["video", "frames", "events", "meta", "manifest"] as const;
export const artifactKinds = [...requiredArtifactKinds, "audio", "depth"] as const;
export const artifactKindSchema = z.enum(artifactKinds);
export type ArtifactKind = z.infer<typeof artifactKindSchema>;

const INT32_MAX = 2_147_483_647;
const safeNonNegativeInteger = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);
const positiveInt32 = z.number().int().min(1).max(INT32_MAX);

const identifierSchema = z
  .string()
  .min(1)
  .max(160)
  .regex(/^[A-Za-z0-9][A-Za-z0-9._:-]*$/, "must be a stable local identifier");
const sha256Schema = z.string().regex(SHA256_PATTERN);
const instantSchema = z.iso.datetime({ offset: true });
const versionSchema = z.string().trim().min(1).max(120);
// 为什么是自由文本而不是机器码（docs/55 §3.1，负责人裁决 2026-07-28）：这一列
// 只在客户端观察失败时出现，排查"指纹为什么没测到"时，完整镜像路径与系统错误
// 原文远比一个错误码有用；标注员都是内部员工，这里不做最小化。
// 上限与隐私无关——这条值来自错误链拼接，失控的嵌套 context 能撑到任意长度。
// 与迁移 0019 的 CHECK 同源。
const executableMissingReasonSchema = z.string().trim().min(1).max(2000);

// The admission envelope is the admitted family turned into a schema, so a
// client that declares one lane or environment can never be admitted by
// another.
export function classificationSchemaFor(family: AdmissionFamily) {
  const declared = family.admission;
  const artifactSchemaVersion = family.audio
    ? z.enum([family.artifactSchemaVersion, family.audio.artifactSchemaVersion])
    : z.literal(family.artifactSchemaVersion);
  const common = {
    sourceFamily: z.literal(declared.sourceFamily),
    contractVersion: z.literal(declared.contractVersion),
    artifactSchemaVersion,
    environment: z.literal(declared.environment),
    storageNamespace: z.literal(declared.storageNamespace),
  };
  // 中性契约（issue #855 §B）不再重复携带训练声明。三个布尔要么整体在（legacy），
  // 要么整体不在（中性）；`.strict()` 让"不在"变成"带了就拒"，而不是"带了也不看"。
  // 半个信封是不存在的状态：`environment.ts` 的 admission 要么三个都填、要么一个都不填。
  if (declared.nonTrainable === undefined) {
    return z.object(common).strict();
  }
  return z
    .object({
      ...common,
      nonTrainable: z.literal(declared.nonTrainable),
      isTrainable: z.literal(declared.isTrainable as boolean),
      productionCertified: z.literal(declared.productionCertified as boolean),
    })
    .strict();
}

const snapshotReferenceSchema = z
  .object({
    id: identifierSchema,
    version: versionSchema,
    contentHash: sha256Schema,
  })
  .strict();

/**
 * 会话声明的精确 Catalog 身份。
 *
 * legacy 车道额外携带 `mode`（`dev_non_trainable`）——那正是 issue #855 §A.4 要拆掉
 * 的"配置 / 环境 / 训练三件事粘在一起"的词。中性车道只留 id + version + contentHash：
 * **provenance 一个字节没少，车道词没有了**。去环境化去的是车道词，不是身份。
 */
function catalogIdentitySchemaFor(family: AdmissionFamily) {
  if (family.catalogMode === null) return snapshotReferenceSchema;
  return z
    .object({
      id: identifierSchema,
      version: versionSchema,
      mode: z.literal(family.catalogMode),
      contentHash: sha256Schema,
    })
    .strict();
}

function preflightReferenceSchemaFor(preflightResults: readonly string[]) {
  return z
    .object({
      schemaVersion: z.literal("dm-preflight-evidence-1"),
      id: identifierSchema,
      // The run body carries no classification block, so at the schema level any
      // admitted family's preflight vocabulary is acceptable; the exact pairing
      // with the parent session's catalog_mode is enforced by the migration-0009
      // trigger inside the write's own transaction.
      result: z.enum(preflightResults as [string, ...string[]]),
      evidenceHash: sha256Schema,
    })
    .strict();
}

export function createSessionSchemaFor(family: AdmissionFamily) {
  const sessionSchemaVersion = family.audio
    ? z.enum([family.sessionSchemaVersion, family.audio.sessionSchemaVersion])
    : z.literal(family.sessionSchemaVersion);
  return z
  .object({
    localSessionId: identifierSchema,
    sessionSchemaVersion,
    startedAt: instantSchema,
    localState: z.enum(["paused", "completed", "faulted", "corrupt"]),
    classification: classificationSchemaFor(family),
    clientVersion: z.string().trim().min(1).max(80),
    catalog: catalogIdentitySchemaFor(family),
    // Which consent the client believes it is recording under (docs/108 D4).
    //
    // Optional on every family now, where it used to exist only on the lane that
    // required a grant — a lane that no longer has one. The server derives the
    // authoritative consent from the device's annotator and the capture start
    // time and always wins; this value is cross-checked against that derivation
    // and a disagreement is *recorded*, not refused. It measures how stale the
    // client's view of its own authorization is, which is a diagnostic, and
    // diagnostics must not be wired to admission (docs/100 §2.1).
    declaredConsentId: z.string().uuid().optional(),
    // 身份观察（docs/55）：指纹是**观测结果**而非准入前提。客户端拿得到就报，
    // 拿不到（受保护进程拒绝 OpenProcess 等）就报一条可读的失败说明，采集不中断。
    // 二者严格互斥——两边都缺是漏报，两边都在是自相矛盾，都拒。资格判定不在这里：
    // 无指纹的会话照常入库，只是 0018 的审批 join 匹配不上，永远拿不到训练资格。
    game: z
      .object({
        id: identifierSchema,
        version: versionSchema,
        contentHash: sha256Schema,
        executableSha256: sha256Schema.nullish(),
        executableSha256MissingReason: executableMissingReasonSchema.nullish(),
      })
      .strict()
      .refine(
        (game) =>
          (game.executableSha256 == null) !== (game.executableSha256MissingReason == null),
        {
          error:
            "exactly one of executableSha256 / executableSha256MissingReason must be present",
          path: ["executableSha256"],
        },
      ),
    // 视角标定声明（docs/46 §5.2）：evidence-only——clip_trainability 用它做
    // "存在性与自洽门"（view_scale_unverified）；谎报 pass 由光流 QA 与
    // quality gate 捕获；服务端永不据此派生 classification。
    viewScaleCalibration: z
      .object({
        sha256: sha256Schema,
        verdict: z.enum(["pass", "fail"]),
        deviationPct: z.number().finite().nullable(),
        pollingHzP50: z.number().finite().nullable(),
      })
      .strict()
      .optional(),
    deviceProfile: snapshotReferenceSchema,
    // capture + settings 两本册子并成一本（docs/57 §3.1）。引擎几何仍然在这里
    // 逐字段申报——它们是服务端唯一能对账的采集事实；游戏侧配置（FOV、灵敏度、
    // view_scale）只住在快照里，控制面不解释它们。
    recordingProfile: z
      .object({
        id: identifierSchema,
        version: versionSchema,
        contentHash: sha256Schema,
        width: positiveInt32,
        height: positiveInt32,
        fpsNum: positiveInt32,
        fpsDen: positiveInt32,
        captureEngine: z.literal("native_wgc_window_v1"),
      })
      .strict(),
    mapping: snapshotReferenceSchema,
  })
  .strict()
  .superRefine((value, context) => {
    const audioPair = family.audio;
    const usesAudioSession = audioPair !== undefined && value.sessionSchemaVersion === audioPair.sessionSchemaVersion;
    const usesAudioArtifacts = audioPair !== undefined && value.classification.artifactSchemaVersion === audioPair.artifactSchemaVersion;
    if (usesAudioSession !== usesAudioArtifacts) {
      context.addIssue({
        code: "custom",
        path: ["sessionSchemaVersion"],
        message: "session and artifact schema generations must match",
      });
    }
    if (value.recordingProfile.fpsNum < 30 * value.recordingProfile.fpsDen) {
      context.addIssue({
        code: "custom",
        path: ["recordingProfile", "fpsNum"],
        message: "frame rate must be at least 30 fps (fpsNum >= 30 * fpsDen)",
      });
    }
  });
}

// The staging instances stay exported: they are the shape most call sites and
// tests still mean, and `AppConfig` carries the admitted families for the rest.
export const createSessionSchema = createSessionSchemaFor(
  primaryFamily(environmentProfile("staging")),
);
/**
 * 两代摄取契约的公共形状。
 *
 * 从 staging 家族推断出来的类型把两处**按家族而变**的位置钉死成了 legacy 的取值：
 * `sessionSchemaVersion` 的字面量并集，和 `catalog.mode`。中性车道两处都不同
 * （issue #855 §B），所以这里把它们放宽到"契约允许的形状"，而不是"legacy 恰好长
 * 成的样子"——放宽的是**类型**，运行时取值域仍由每个家族自己的 zod schema 钉死。
 */
export type CreateSessionInput = Omit<
  z.infer<typeof createSessionSchema>,
  "catalog" | "sessionSchemaVersion"
> & {
  sessionSchemaVersion: string;
  /** `mode` 只在 legacy 车道存在；中性车道的 Catalog 身份不再自称车道。 */
  catalog: { id: string; version: string; contentHash: string; mode?: string };
};

export function putRunSchemaFor(preflightResults: readonly string[]) {
  return z
  .object({
    sequence: z.number().int().min(0).max(INT32_MAX),
    localState: z.enum(["paused", "completed", "faulted"]),
    startedAt: instantSchema,
    endedAt: instantSchema,
    qpcFrequency: z.number().int().min(1).max(Number.MAX_SAFE_INTEGER),
    sessionFrameStart: safeNonNegativeInteger,
    // Run outcome facts (docs/67 §3.2). Optional because clients ≤0.6.1 do not
    // send them and a strict object would reject their whole run PUT; the
    // columns have been nullable since 0001 and stayed empty for exactly that
    // reason. A run that ends without them is indistinguishable from an old
    // client, which is why the server fills them once and never overwrites.
    frameCount: safeNonNegativeInteger.optional(),
    stopReason: z.string().trim().min(1).max(120).optional(),
    errorCode: z.string().trim().min(1).max(120).optional(),
    preflight: preflightReferenceSchemaFor(preflightResults),
    capture: z
      .object({
        width: positiveInt32,
        height: positiveInt32,
        fpsNum: positiveInt32,
        fpsDen: positiveInt32,
        captureEngine: z.literal("native_wgc_window_v1"),
        encoderBackend: z.string().trim().min(1).max(120),
      })
      .strict(),
  })
  .strict()
  .superRefine((value, context) => {
    if (Date.parse(value.endedAt) < Date.parse(value.startedAt)) {
      context.addIssue({
        code: "custom",
        path: ["endedAt"],
        message: "endedAt must be greater than or equal to startedAt",
      });
    }
    if (value.capture.fpsNum < 30 * value.capture.fpsDen) {
      context.addIssue({
        code: "custom",
        path: ["capture", "fpsNum"],
        message: "frame rate must be at least 30 fps (fpsNum >= 30 * fpsDen)",
      });
    }
  });
}

export const putRunSchema = putRunSchemaFor([
  primaryFamily(environmentProfile("staging")).preflightResult,
]);
export type PutRunInput = z.infer<typeof putRunSchema>;

export const artifactDeclarationSchema = z
  .object({
    byteSize: z.number().int().positive().max(5 * 1024 ** 4),
    sha256: sha256Schema,
  })
  .strict();
export type ArtifactDeclaration = z.infer<typeof artifactDeclarationSchema>;

const audioSpanSchema = z
  .object({
    start_frame: safeNonNegativeInteger,
    frames: z.number().int().positive().max(Number.MAX_SAFE_INTEGER),
  })
  .strict();

export const segmentAudioMetadataSchema = z
  .object({
    state: z.enum(["ready", "degraded", "absent"]),
    source: z.literal("process_loopback"),
    path: z.literal("audio.pcm.zst").optional(),
    sha256: sha256Schema.optional(),
    bytes: z.number().int().positive().max(5 * 1024 ** 4).optional(),
    format: z
      .object({
        sample_rate: z.literal(48_000),
        channels: z.literal(2),
        bits: z.literal(16),
        sample_format: z.literal("pcm_s16le"),
        channel_mask: z.literal(3),
        requested_format: z.literal("pcm_s16le/48000/2"),
        system_src: z.literal(true),
      })
      .strict(),
    qpc_frequency: z.number().int().positive().max(Number.MAX_SAFE_INTEGER),
    first_sample_qpc: z.number().int().min(Number.MIN_SAFE_INTEGER).max(Number.MAX_SAFE_INTEGER).optional(),
    frames: z.number().int().positive().max(Number.MAX_SAFE_INTEGER).optional(),
    failure_code: z.string().regex(/^AUDIO_[A-Z0-9_]{1,96}$/).optional(),
    health: z
      .object({
        discontinuities: z.array(audioSpanSchema).max(10_000),
        silence_filled_spans: z.array(audioSpanSchema).max(10_000),
        queue_overflows: safeNonNegativeInteger,
        timestamp_errors: safeNonNegativeInteger,
        max_qpc_drift_us: safeNonNegativeInteger,
      })
      .strict(),
  })
  .strict()
  .superRefine((audio, context) => {
    const present = audio.state !== "absent";
    const presentFields =
      audio.path !== undefined &&
      audio.sha256 !== undefined &&
      audio.bytes !== undefined &&
      audio.first_sample_qpc !== undefined &&
      audio.frames !== undefined;
    if (present !== presentFields) {
      context.addIssue({
        code: "custom",
        path: ["path"],
        message: "audio artifact fields must be present exactly for ready/degraded audio",
      });
    }
    if ((audio.state === "ready") !== (audio.failure_code === undefined)) {
      context.addIssue({
        code: "custom",
        path: ["failure_code"],
        message: "ready audio has no failure code; degraded/absent audio requires one",
      });
    }
    if (
      audio.state === "ready" &&
      (audio.health.discontinuities.length !== 0 ||
        audio.health.silence_filled_spans.length !== 0 ||
        audio.health.queue_overflows !== 0 ||
        audio.health.timestamp_errors !== 0)
    ) {
      context.addIssue({ code: "custom", path: ["health"], message: "ready audio health must be pristine" });
    }
    if (present && audio.frames !== undefined) {
      for (const [field, spans] of [
        ["discontinuities", audio.health.discontinuities],
        ["silence_filled_spans", audio.health.silence_filled_spans],
      ] as const) {
        for (const [index, span] of spans.entries()) {
          if (span.start_frame > audio.frames - span.frames) {
            context.addIssue({
              code: "custom",
              path: ["health", field, index],
              message: "audio health span exceeds the segment sample range",
            });
          }
        }
      }
    }
    if (audio.state === "degraded" && audio.frames !== undefined) {
      if (audio.health.queue_overflows !== 0 || audio.health.timestamp_errors !== 0) {
        context.addIssue({
          code: "custom",
          path: ["health"],
          message: "audio queue overflow or timestamp error must be absent, not degraded",
        });
      }
      const silenceFrames = audio.health.silence_filled_spans.reduce(
        (sum, span) => sum + span.frames,
        0,
      );
      const maxByPercent = Math.floor(audio.frames / 100);
      if (
        !Number.isSafeInteger(silenceFrames) ||
        silenceFrames === 0 ||
        silenceFrames > 4_800 ||
        silenceFrames > maxByPercent
      ) {
        context.addIssue({
          code: "custom",
          path: ["health", "silence_filled_spans"],
          message: "degraded audio must contain only a bounded silence fill",
        });
      }
    }
  });
export type SegmentAudioMetadata = z.infer<typeof segmentAudioMetadataSchema>;

export const segmentDepthMetadataSchema = z.discriminatedUnion("state", [
  z
    .object({
      state: z.literal("ready"),
      source: z.literal("depth_probe"),
      path: z.literal("depth.zip"),
      sha256: sha256Schema,
      bytes: z.number().int().positive().max(5 * 1024 ** 4),
      // v2 carries raw depth plus reconstruction provenance; this format declaration
      // does not claim metric depth or change the object's size/SHA-256 verification.
      format: z.enum(["dm-depth-variant-r-zip-1", "dm-depth-raw-zip-2"]),
    })
    .strict(),
  z.object({ state: z.literal("absent") }).strict(),
]);
export type SegmentDepthMetadata = z.infer<typeof segmentDepthMetadataSchema>;

const jsonArtifactDeclarationSchema = artifactDeclarationSchema.extend({
  byteSize: z.number().int().positive().max(2 * 1024 * 1024),
});

const artifactObjectSchema = z
  .object({
    video: artifactDeclarationSchema,
    frames: artifactDeclarationSchema,
    events: artifactDeclarationSchema,
    meta: jsonArtifactDeclarationSchema,
    manifest: jsonArtifactDeclarationSchema,
    audio: artifactDeclarationSchema.optional(),
    depth: artifactDeclarationSchema.optional(),
  })
  .strict();

export function putClipSchemaFor(family: AdmissionFamily) {
  return z
  .object({
    classification: classificationSchemaFor(family),
    localArtifactState: z.literal("ready"),
    sessionFrameStart: safeNonNegativeInteger,
    nFrames: z.number().int().min(1).max(Number.MAX_SAFE_INTEGER),
    qpcStart: z.number().int().min(Number.MIN_SAFE_INTEGER).max(Number.MAX_SAFE_INTEGER),
    qpcEnd: z.number().int().min(Number.MIN_SAFE_INTEGER).max(Number.MAX_SAFE_INTEGER),
    eventCount: safeNonNegativeInteger,
    health: z
      .object({
        duplicateFrames: safeNonNegativeInteger,
        sourceSuperseded: safeNonNegativeInteger,
        queueDrops: safeNonNegativeInteger,
        inputOverflow: safeNonNegativeInteger,
      })
      .strict(),
    audio: segmentAudioMetadataSchema.optional(),
    depth: segmentDepthMetadataSchema.optional(),
    artifacts: artifactObjectSchema,
  })
  .strict()
  .superRefine((value, context) => {
    if (value.qpcEnd < value.qpcStart) {
      context.addIssue({
        code: "custom",
        path: ["qpcEnd"],
        message: "qpcEnd must be greater than or equal to qpcStart",
      });
    }
    // 音频世代按家族数据判定，不硬编码词表：certified 的音频代（docs/63 §3）
    // 与 dev 的走同一条规则。
    //
    // 中性车道（`per_clip`）没有音频世代可判：`dm-session-ingest-2` 是唯一的 session
    // schema（issue #855 §B），"这一段有没有音频"是 clip 自己的事实。世代配对在这里
    // 缺席**不等于**没有闸——录制档案有没有配置音频由服务端快照裁决，见
    // `putClip` 的 `clip_audio_not_in_profile`。
    if (family.audioDeclaration === "session_schema") {
      const audioVersion =
        family.audio !== undefined &&
        value.classification.artifactSchemaVersion === family.audio.artifactSchemaVersion;
      if (audioVersion !== (value.audio !== undefined)) {
        context.addIssue({
          code: "custom",
          path: ["audio"],
          message:
            "the audio artifact generation requires audio metadata and the base generation forbids it",
        });
      }
    }
    const audioHasArtifact = value.audio !== undefined && value.audio.state !== "absent";
    if (audioHasArtifact !== (value.artifacts.audio !== undefined)) {
      context.addIssue({
        code: "custom",
        path: ["artifacts", "audio"],
        message: "audio artifact declaration must match the audio metadata state",
      });
    }
    if (
      value.audio !== undefined &&
      value.artifacts.audio !== undefined &&
      (value.audio.sha256 !== value.artifacts.audio.sha256 ||
        value.audio.bytes !== value.artifacts.audio.byteSize)
    ) {
      context.addIssue({
        code: "custom",
        path: ["artifacts", "audio"],
        message: "audio artifact declaration must match audio metadata bytes and SHA-256",
      });
    }
    const depthHasArtifact = value.depth?.state === "ready";
    if (depthHasArtifact !== (value.artifacts.depth !== undefined)) {
      context.addIssue({
        code: "custom",
        path: ["artifacts", "depth"],
        message: "depth artifact declaration must match the depth metadata state",
      });
    }
    if (
      value.depth?.state === "ready" &&
      value.artifacts.depth !== undefined &&
      (value.depth.sha256 !== value.artifacts.depth.sha256 ||
        value.depth.bytes !== value.artifacts.depth.byteSize)
    ) {
      context.addIssue({
        code: "custom",
        path: ["artifacts", "depth"],
        message: "depth artifact declaration must match depth metadata bytes and SHA-256",
      });
    }
  });
}

export const putClipSchema = putClipSchemaFor(primaryFamily(environmentProfile("staging")));
/** 同 `CreateSessionInput`：中性车道的 classification 信封不携带三个训练布尔。 */
export type PutClipInput = Omit<z.infer<typeof putClipSchema>, "classification"> & {
  classification: {
    sourceFamily: string;
    contractVersion: string;
    artifactSchemaVersion: string;
    environment: string;
    storageNamespace: string;
    nonTrainable?: boolean;
    isTrainable?: boolean;
    productionCertified?: boolean;
  };
};

export const createUploadIntentSchema = z
  .object({
    clipRevision: positiveInt32,
    transferMode: z.literal("server_owned_r2_multipart"),
  })
  .strict();
export type CreateUploadIntentInput = z.infer<typeof createUploadIntentSchema>;

export const signPartsSchema = z
  .object({
    parts: z
      .array(
        z
          .object({
            artifactType: artifactKindSchema,
            partNumber: z.number().int().min(1).max(10_000),
            byteSize: z.number().int().positive().max(5 * 1024 ** 3),
            contentMd5: z
              .string()
              .regex(/^[A-Za-z0-9+/]{22}==$/, "must be a base64 MD5 digest"),
          })
          .strict(),
      )
      .min(1)
      .max(100),
  })
  .strict()
  .superRefine((value, context) => {
    const identities = value.parts.map((part) => `${part.artifactType}:${part.partNumber}`);
    if (new Set(identities).size !== identities.length) {
      context.addIssue({ code: "custom", path: ["parts"], message: "artifact part identities must be unique" });
    }
  });
export type SignPartsInput = z.infer<typeof signPartsSchema>;

const completedPartsSchema = z
  .object({
    parts: z
      .array(
        z
          .object({
            partNumber: z.number().int().min(1).max(10_000),
            etag: z.string().min(1).max(256),
          })
          .strict(),
      )
      .min(1)
      .max(10_000),
  })
  .strict()
  .superRefine((value, context) => {
    const partNumbers = value.parts.map((part) => part.partNumber);
    if (new Set(partNumbers).size !== partNumbers.length) {
      context.addIssue({ code: "custom", path: ["parts"], message: "part numbers must be unique" });
    }
    for (let index = 0; index < partNumbers.length; index += 1) {
      if (partNumbers[index] !== index + 1) {
        context.addIssue({
          code: "custom",
          path: ["parts", index, "partNumber"],
          message: "parts must be sorted and contiguous starting at 1",
        });
        break;
      }
    }
  });

export const completeUploadIntentSchema = z
  .object({
    artifacts: z
      .object({
        video: completedPartsSchema,
        frames: completedPartsSchema,
        events: completedPartsSchema,
        meta: completedPartsSchema,
        manifest: completedPartsSchema,
        audio: completedPartsSchema.optional(),
        depth: completedPartsSchema.optional(),
      })
      .strict(),
  })
  .strict();
export type CompleteUploadIntentInput = z.infer<typeof completeUploadIntentSchema>;

export const requestClipPurgeSchema = z
  .object({
    reason: z.literal("device_principal_delete"),
  })
  .strict();
export type RequestClipPurgeInput = z.infer<typeof requestClipPurgeSchema>;

// docs/47 · session-root artifacts (6 snapshots + markers.jsonl). Separate,
// versioned contract; not the clip five-piece set (doc 14 §1). Small JSON/JSONL,
// so a single presigned PUT rather than multipart.
export const sessionArtifactKinds = [
  "session",
  "catalog",
  "device_profile",
  "recording_profile",
  "game_profile",
  "mapping",
  "markers",
  "input_calibration",
] as const;
/**
 * 非可训练车道必须一次性申报齐的六件快照（docs/47）。
 * 断代前是七件：capture + settings 两本册子合并成 recording 之后少一件
 * （docs/57 §3.1）。客户端的 SessionArtifactKind::REQUIRED 同步收到 6。
 */
export const requiredSessionArtifactKinds = [
  "session",
  "catalog",
  "device_profile",
  "recording_profile",
  "game_profile",
  "mapping",
] as const;
export const sessionArtifactKindSchema = z.enum(sessionArtifactKinds);
export type SessionArtifactKind = z.infer<typeof sessionArtifactKindSchema>;

const sessionArtifactDeclarationSchema = z
  .object({
    byteSize: z.number().int().positive().max(16 * 1024 * 1024),
    sha256: sha256Schema,
  })
  .strict();

export const putSessionArtifactsSchema = z
  .object({
    // 每种都在 schema 层可选：集合形状按车道在服务层校验（docs/46 §5.2）——
    // 非可训练车道要求六件快照齐（capture + settings 合并后从七件降到六件，
    // docs/57 §3.1）。单 kind 例外只剩 `input_calibration` 那一条，原来的
    // first-party 车道随 §3.3 退役。
    artifacts: z
      .object({
        session: sessionArtifactDeclarationSchema.optional(),
        catalog: sessionArtifactDeclarationSchema.optional(),
        device_profile: sessionArtifactDeclarationSchema.optional(),
        recording_profile: sessionArtifactDeclarationSchema.optional(),
        game_profile: sessionArtifactDeclarationSchema.optional(),
        mapping: sessionArtifactDeclarationSchema.optional(),
        // markers.jsonl is optional: the client only produces it when a session
        // has manual markers (T12).
        markers: sessionArtifactDeclarationSchema.optional(),
        // 视角标定证据（docs/46 §2.2）：仅当会话引用了标定时产生。
        input_calibration: sessionArtifactDeclarationSchema.optional(),
      })
      .strict(),
  })
  .strict();
export type PutSessionArtifactsInput = z.infer<typeof putSessionArtifactsSchema>;

export const signSessionArtifactSchema = z
  .object({
    contentMd5: z.string().regex(/^[A-Za-z0-9+/]{22}==$/, "must be a base64 MD5 digest"),
  })
  .strict();
export type SignSessionArtifactInput = z.infer<typeof signSessionArtifactSchema>;

export const sessionArtifactPathParamsSchema = z.object({
  sessionId: z.string().uuid(),
  kind: sessionArtifactKindSchema,
});

export const sealSessionSchema = z
  .object({
    endedAt: instantSchema,
    localFinalState: z.enum(["completed", "faulted", "corrupt"]),
    stopReason: z.string().min(1).max(160).optional(),
  })
  .strict();
export type SealSessionInput = z.infer<typeof sealSessionSchema>;


export const sessionPathParamsSchema = z.object({ sessionId: z.string().uuid() });
export const runPathParamsSchema = sessionPathParamsSchema.extend({ localRunId: identifierSchema });
export const clipDeclarationPathParamsSchema = runPathParamsSchema.extend({
  segmentIndex: z.coerce.number().int().min(0).max(INT32_MAX),
});
export const clipPathParamsSchema = z.object({ clipId: z.string().uuid() });
export const uploadIntentPathParamsSchema = z.object({ uploadIntentId: z.string().uuid() });

export const idempotencyKeySchema = z
  .string()
  .min(8)
  .max(160)
  .regex(/^[\x21-\x7E]+$/, "must contain visible ASCII characters only");

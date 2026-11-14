import { z } from "zod";

/**
 * 镜像事件契约。
 *
 * 两条不可协商的规则：
 *  1. 每个 object 都是 `.strict()`——未知字段**拒绝**，不是忽略。悄悄丢掉一个字段
 *     会让客户端以为自己报上去了；宁可 400。
 *  2. 只允许脱敏投影字段。禁止出现 HWND/PID、窗口标题、本地绝对路径、原始按键/鼠标
 *     事件、annotator alias、凭据。契约层是这条边界的最后一道闸，不能指望调用方自觉。
 */

export const MAX_EVENTS_PER_BATCH = 500;

/** 与客户端 journal 的实体一一对应；heartbeat 是唯一没有本地实体的合成事件。 */
export const mirrorEntities = [
  "session",
  "run",
  "segment",
  "marker",
  "preflight",
  "fault",
  "heartbeat",
] as const;

/**
 * 与 Cloud SQL `devices.id` 同形：uuid，服务端生成。
 * 固定小写以对齐仓库里 `sha256:<64 lowercase hex>` 的口径，也挡住机器名、
 * hostname 之类可识别标识被当成 device id 混进镜像。
 */
const deviceIdSchema = z
  .string()
  .regex(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
    "device id must be a lowercase uuid issued by the console (devices.id)",
  );

/** 本地 id 由客户端生成，只允许保守字符集，避免把路径片段混进来。 */
const localIdSchema = z.string().min(1).max(128).regex(/^[A-Za-z0-9_.:-]+$/);

const epochMillisSchema = z.int().min(0).max(4_102_444_800_000);

const sessionPayloadSchema = z
  .object({
    localSessionId: localIdSchema,
    state: z.enum(["starting", "recording", "paused", "stopping", "completed", "faulted"]),
    gameSlug: z.string().min(1).max(64).regex(/^[a-z0-9][a-z0-9_-]*$/).optional(),
    startedAt: epochMillisSchema.optional(),
    endedAt: epochMillisSchema.optional(),
    runCount: z.int().min(0).max(100_000),
    segmentCount: z.int().min(0).max(10_000_000),
    readySegmentCount: z.int().min(0).max(10_000_000),
  })
  .strict();

const segmentPayloadSchema = z
  .object({
    localSessionId: localIdSchema,
    localRunId: localIdSchema,
    segmentIndex: z.int().min(0).max(10_000_000),
    localState: z.enum(["active", "finalizing", "ready", "aborted", "failed", "corrupt", "quarantined", "deleted"]),
    nFrames: z.int().min(0).max(100_000_000).optional(),
    totalBytes: z.int().min(0).max(1_099_511_627_776).optional(),
    /**
     * 客户端自己的上传队列状态。它**不是**远端事实——远端是否真的 stored/verified
     * 只能问 Cloud SQL。命名刻意加 client 前缀，避免有人拿它当 transfer_state 用。
     */
    clientUploadState: z.enum(["unknown", "pending", "uploading", "registered", "failed"]),
  })
  .strict();

const heartbeatPayloadSchema = z
  .object({
    clientVersion: z.string().min(1).max(64),
    recordingState: z.enum(["idle", "recording", "paused", "faulted"]),
    diskFreeBytes: z.int().min(0).max(1_125_899_906_842_624).optional(),
    pendingUploadCount: z.int().min(0).max(10_000_000).optional(),
  })
  .strict();

/**
 * 其余实体当前只进事件日志，不做当前态投影。用一个受限的通用形状收下，
 * 而不是接受任意 JSON——`details_id` 是客户端诊断的稳定引用，不是自由文本。
 */
const auditPayloadSchema = z
  .object({
    localSessionId: localIdSchema.optional(),
    localRunId: localIdSchema.optional(),
    code: z.string().min(1).max(64).regex(/^[A-Z][A-Z0-9_]*$/),
    detailsId: z.string().min(1).max(128).regex(/^[A-Za-z0-9_.:-]+$/).optional(),
  })
  .strict();

const eventBase = {
  seq: z.int().min(1).max(Number.MAX_SAFE_INTEGER),
  entityId: localIdSchema,
  observedAt: epochMillisSchema,
};

export const mirrorEventSchema = z.discriminatedUnion("entity", [
  z.object({ ...eventBase, entity: z.literal("session"), payload: sessionPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("segment"), payload: segmentPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("heartbeat"), payload: heartbeatPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("run"), payload: auditPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("marker"), payload: auditPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("preflight"), payload: auditPayloadSchema }).strict(),
  z.object({ ...eventBase, entity: z.literal("fault"), payload: auditPayloadSchema }).strict(),
]);

export const ingestEventsSchema = z
  .object({
    deviceId: deviceIdSchema,
    events: z
      .array(mirrorEventSchema)
      .min(1)
      .max(MAX_EVENTS_PER_BATCH)
      .refine(
        (events) => events.every((event, index) => index === 0 || event.seq > events[index - 1]!.seq),
        { message: "events must be ordered by strictly increasing seq" },
      ),
  })
  .strict();

export const deviceParamsSchema = z.object({ deviceId: deviceIdSchema }).strict();

export type MirrorEvent = z.infer<typeof mirrorEventSchema>;
export type IngestEventsInput = z.infer<typeof ingestEventsSchema>;
export type MirrorEntity = (typeof mirrorEntities)[number];

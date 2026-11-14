import { z } from "zod";

const sha256Hex = z.string().regex(/^[0-9a-f]{64}$/, "a lowercase sha256 hex digest is required");
const isoDate = z.iso.datetime({ offset: true });
const jsonObject = z.record(z.string(), z.unknown());

export const idempotencyKeySchema = z
  .string()
  .trim()
  .min(8)
  .max(120)
  .regex(/^[A-Za-z0-9_.:-]+$/, "an idempotency key may only contain url-safe characters");

/**
 * 目录渠道。单值，且**点名拒绝**退役的那个值。
 *
 * certified 采集车道随 docs/100 从录制侧删除（PR #578），发布侧随 issue #829
 * 收口（docs/100 §11）。退成 `z.literal("dev")` 之后一个 `certified` 会落进
 * 泛泛的「不是 dev」——这里给它一条指名道姓的消息，照 docs/100 §10.1 第 2 条
 * 的先例：一个仍然发 certified 的调用方 pin 的是那把已退役的 KMS 锚，静默当
 * dev 收下只会让它死在一句 signature verification failed 上，排障方向全错。
 *
 * 消息落在 ZodError → 400 `invalid_request` 的 `details.issues[].message` 里。
 */
export const catalogChannelSchema = z.literal("dev", {
  error: (issue) =>
    issue.input === "certified"
      ? "the certified catalog channel was retired (docs/100); dev is the only channel"
      : "unknown catalog channel; dev is the only channel",
});

export const publishCatalogReleaseSchema = z.object({
  channel: catalogChannelSchema,
  // 每游戏一份 bundle（docs/57 §3.3）之后，目录 id 必须由调用方给出——
  // 断代前它由渠道派生成两个「舰队」常量，现在那两个常量不存在了。
  catalogId: z.string().trim().min(1).max(200),
  catalogVersion: z.string().trim().min(1).max(120),
  // 四本册子各至少一行（docs/57 §3.1）。
  trustedCatalogVersionIds: z.array(z.string().uuid()).min(4).max(10_000),
});

export const createAnnotatorSchema = z.object({
  alias: z.string().trim().min(1).max(80),
  externalSubjectHash: sha256Hex.optional(),
});

export const updateAnnotatorSchema = z
  .object({
    alias: z.string().trim().min(1).max(80).optional(),
    status: z.enum(["active", "suspended"]).optional(),
  })
  .refine((value) => value.alias !== undefined || value.status !== undefined, {
    message: "at least one of alias or status must be provided",
  });

// allowsTrainableUse is accepted in the shape but refused by the service in this
// stage. Dropping the field instead would make a trainable consent look merely
// unsupported rather than deliberately blocked.
export const createConsentSchema = z.object({
  annotatorId: z.string().uuid(),
  policyVersion: z.string().trim().min(1).max(64),
  purpose: z.string().trim().min(1).max(200),
  termsHash: sha256Hex,
  acceptedAt: isoDate,
  expiresAt: isoDate.optional(),
  retentionDeadline: isoDate.optional(),
  allowsTrainableUse: z.boolean().default(false),
});

export const revokeSchema = z.object({
  reason: z.string().trim().min(3).max(280),
});

export const createDeviceSchema = z.object({
  clientDeviceId: z.string().trim().min(1).max(120),
  annotatorId: z.string().uuid().optional(),
  // Legacy operator payload compatibility only; the repository deliberately
  // does not persist this as session authorization.
  certifiedProfileId: z.string().trim().min(1).max(200).optional(),
  windowsBuild: z.string().trim().min(1).max(64),
  gpuPciId: z.string().trim().min(1).max(120),
  gpuDriver: z.string().trim().min(1).max(64),
  clientVersion: z.string().trim().min(1).max(64),
  displaySnapshot: jsonObject,
  inputDevicesSnapshot: z.union([jsonObject, z.array(z.unknown())]),
});

export const updateDeviceSchema = z
  .object({
    status: z.enum(["pending", "active", "suspended", "retired"]).optional(),
    annotatorId: z.union([z.string().uuid(), z.literal("none")]).optional(),
  })
  .refine((value) => value.status !== undefined || value.annotatorId !== undefined, {
    message: "at least one of status or annotatorId must be provided",
  });

export const clipQualitySchema = z.object({
  result: z.enum(["passed", "failed"]),
  note: z.string().trim().max(280).optional(),
});

/**
 * session 级 QA 判定（issue #855 PR 5）。
 *
 * 形状与 `clipQualitySchema` 逐字相同——这是**有意**的，不是可以合并的重复：
 * 判定的内容（合格/不合格 + 备注）在两个粒度上本来就一样，变的只是判定落到
 * 几条 clip 上。两份 schema 分开写，是为了将来任一粒度要加字段时不会连坐另一
 * 个；合成一份反而会让"逐 clip 例外入口"和"整场入口"被迫共享演化。
 */
export const sessionQualitySchema = z.object({
  result: z.enum(["passed", "failed"]),
  note: z.string().trim().max(280).optional(),
});

// A review verdict over a bucket subject (docs/33 §5). The subject key is a
// directory prefix ending "/" — the reviewable unit, not a single object — and
// stays a plain relative path whatever the caller sends.
export const reviewVerdictSchema = z.object({
  source: z.string().trim().min(1).max(32),
  subjectKind: z.enum(["raw_segment", "dataset_chunk", "session"]),
  subjectKey: z
    .string()
    .trim()
    .min(1)
    .max(1024)
    .refine((value) => value.endsWith("/"), { message: "subject keys are directory prefixes" })
    .refine(
      (value) => !value.startsWith("/") && !value.includes("..") && !value.includes("\\"),
      { message: "subject keys must be plain relative paths" },
    ),
  result: z.enum(["passed", "failed"]),
  note: z.string().trim().max(280).optional(),
});

export const reviewVerdictQuerySchema = z.object({
  source: z.string().trim().min(1).max(32),
  subjectKey: z.string().trim().min(1).max(1024),
});

export const clipPurgeSchema = z.object({
  reason: z.string().trim().min(3).max(280),
});

export const catalogQuerySchema = z.object({
  kind: z
    .enum(["device_profile", "capture_profile", "game_profile", "settings_profile", "game_mapping"])
    .optional(),
  gameId: z.string().trim().min(1).max(120).optional(),
});

export const catalogReleaseQuerySchema = z.object({
  channel: catalogChannelSchema.optional(),
});

export type CreateAnnotatorInput = z.infer<typeof createAnnotatorSchema>;
export type UpdateAnnotatorInput = z.infer<typeof updateAnnotatorSchema>;
export type CreateConsentInput = z.infer<typeof createConsentSchema>;
export type CreateDeviceInput = z.infer<typeof createDeviceSchema>;
export type UpdateDeviceInput = z.infer<typeof updateDeviceSchema>;
export type ClipQualityInput = z.infer<typeof clipQualitySchema>;
export type SessionQualityInput = z.infer<typeof sessionQualitySchema>;
export type ReviewVerdictInput = z.infer<typeof reviewVerdictSchema>;

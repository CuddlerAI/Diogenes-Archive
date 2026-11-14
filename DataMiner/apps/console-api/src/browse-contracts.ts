import { z } from "zod";

import { defaultPageSize, maxPageSize } from "./pagination.js";

const cursorSchema = z.string().max(512).optional();
const pageSizeSchema = z.coerce.number().int().min(1).max(maxPageSize).default(defaultPageSize);
const isoDateSchema = z.iso.datetime({ offset: true }).optional();

// "none" is a first-class filter value: M2a data has no annotator, so the
// browsers must be able to ask for exactly the rows nobody is attached to
// instead of pretending the dimension does not exist.
export const unattached = "none";
const annotatorFilterSchema = z.union([z.literal(unattached), z.string().uuid()]).optional();

export const paginationSchema = z.object({
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

/**
 * 时间口径（docs/67 §3.3）。`recorded` 是录制墙钟，`registered` 是服务端登记
 * 时间。默认 recorded：问「这个人这天干了什么」时想要的是录制那天，而上传可能
 * 发生在几小时甚至几天之后（弱网重传、断网补传）。
 */
export const timeBasisSchema = z.enum(["recorded", "registered"]).optional();
export type TimeBasis = "recorded" | "registered";
/** 缺省口径只在这一处定下，其余调用点原样把 undefined 传下去。 */
export const defaultTimeBasis: TimeBasis = "recorded";

export const annotatorListQuerySchema = z.object({
  status: z.enum(["active", "suspended", "tombstoned"]).optional(),
  query: z.string().trim().min(1).max(120).optional(),
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

export const deviceListQuerySchema = z.object({
  status: z.enum(["pending", "active", "suspended", "retired"]).optional(),
  annotatorId: annotatorFilterSchema,
  clientVersion: z.string().trim().min(1).max(64).optional(),
  query: z.string().trim().min(1).max(120).optional(),
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

export const sessionListQuerySchema = z.object({
  gameId: z.string().trim().min(1).max(120).optional(),
  deviceId: z.string().uuid().optional(),
  annotatorId: annotatorFilterSchema,
  status: z.enum(["registered", "capturing", "sealed", "completed", "rejected"]).optional(),
  catalogMode: z.string().trim().min(1).max(64).optional(),
  from: isoDateSchema,
  to: isoDateSchema,
  timeBasis: timeBasisSchema,
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

export const clipListQuerySchema = z.object({
  gameId: z.string().trim().min(1).max(120).optional(),
  deviceId: z.string().uuid().optional(),
  annotatorId: annotatorFilterSchema,
  sessionId: z.string().uuid().optional(),
  ingestState: z.enum(["pending", "uploading", "stored", "retry_wait", "failed"]).optional(),
  verificationState: z
    .enum(["unverified", "queued", "verifying", "verified", "rejected"])
    .optional(),
  qualityState: z.enum(["pending", "passed", "failed"]).optional(),
  retentionState: z
    .enum(["retained", "purge_requested", "purging", "purged", "purge_failed"])
    .optional(),
  /**
   * 只看健康异常的片段（docs/67 §3.4；判定式见 docs/146——硬故障位非零或
   * 补帧占比越线，`sourceSuperseded` 是诊断列不参与判定）。逐值列举而不是
   * `z.coerce.boolean()`——后者把任何非空串都当真，`unhealthyOnly=false`
   * 会筛出与本意相反的结果。
   */
  unhealthyOnly: z
    .enum(["true", "false", "1", "0"])
    .transform((value) => value === "true" || value === "1")
    .optional(),
  from: isoDateSchema,
  to: isoDateSchema,
  timeBasis: timeBasisSchema,
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

export const clientEventQuerySchema = z.object({
  deviceId: z.string().uuid().optional(),
  annotatorId: z.string().uuid().optional(),
  sessionId: z.string().uuid().optional(),
  severity: z.enum(["debug", "info", "warn", "error"]).optional(),
  kind: z.string().trim().min(1).max(120).optional(),
  from: isoDateSchema,
  to: isoDateSchema,
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

export const auditQuerySchema = z.object({
  actorType: z.enum(["service", "annotator", "device", "operator", "worker"]).optional(),
  actorId: z.string().trim().min(1).max(200).optional(),
  entityType: z.string().trim().min(1).max(64).optional(),
  entityId: z.string().trim().min(1).max(200).optional(),
  action: z.string().trim().min(1).max(120).optional(),
  from: isoDateSchema,
  to: isoDateSchema,
  cursor: cursorSchema,
  pageSize: pageSizeSchema,
});

/**
 * 天界时区（docs/97 §7.2）。「每天」是运营日报的天：团队与采集员都在东八区，
 * UTC 天界（北京 08:00）会把凌晨录制归到前一天。缺省只在这一处定下；
 * 它是代码常量而不是 env——`--set-env-vars` 整组替换的失败类不再给新目标。
 */
export const defaultStatsTimeZone = "Asia/Shanghai";

/** IANA 时区名。合法性用 Intl 试构造判定，非法名 400 而不是把错误留给 SQL。 */
const timeZoneSchema = z
  .string()
  .trim()
  .min(1)
  .max(64)
  .refine((tz) => {
    try {
      new Intl.DateTimeFormat("en-US", { timeZone: tz });
      return true;
    } catch {
      return false;
    }
  }, "unknown IANA time zone")
  .default(defaultStatsTimeZone);

// docs/97 P2：贡献面与总览面同一天界缺省——同一个人的同一天，两页必须是同一天。
export const contributionQuerySchema = z.object({
  days: z.coerce.number().int().min(1).max(180).default(30),
  timeBasis: timeBasisSchema,
  tz: timeZoneSchema,
});

/** 维度拆分（docs/97 §7.8）。缺省不拆；拆分序列是稀疏的，空天由整体轴补齐。 */
const groupBySchema = z.enum(["game", "annotator"]).optional();

export const statsDailyQuerySchema = z.object({
  days: z.coerce.number().int().min(1).max(180).default(30),
  timeBasis: timeBasisSchema,
  tz: timeZoneSchema,
  groupBy: groupBySchema,
});

/**
 * 产出报表（docs/145）。回答「谁、哪个游戏、哪段时间、交了多少」。
 *
 * 与 `statsDaily` 的分工：那个是总览页的固定滑窗快照，只拆一层；这个收显式区间、
 * 可叠两层分组、可对分组值筛选。两者共用同一套指标口径与天界时区，底部合计逐桶
 * 必须对得上（`browse-output-report.test.ts` 的对账用例钉的就是这条）。
 */

/** YYYY-MM-DD 日历日。「哪一天」由 `tz` 决定，所以这里只收日期，不收时刻。 */
const calendarDaySchema = z
  .string()
  .trim()
  .regex(/^\d{4}-\d{2}-\d{2}$/, "expected a YYYY-MM-DD calendar day")
  .refine((value) => {
    const parsed = new Date(`${value}T00:00:00Z`);
    return !Number.isNaN(parsed.getTime()) && parsed.toISOString().slice(0, 10) === value;
  }, "not a real calendar day");

/** 可分组的属性。第一刀只有两条——加第三条时这里加一项、仓储那两个分支各加一支。 */
export const outputDimensions = ["annotator", "game"] as const;
export type OutputDimension = (typeof outputDimensions)[number];

/**
 * 区间上限，按粒度分开。
 *
 * 按日一个季度、按月一年：不是保守，是列数本身的可读上限——按日 92 列已经要横向
 * 滚三屏，再长界面先失效、库才轮到吃力。上限落在这里而不是靠行数截断，是为了
 * 不出现「截断了但没说」的表；口径外的请求直接 400，并告诉调用方该切哪个粒度。
 */
export const maxOutputSpanDays: Record<"day" | "month", number> = { day: 92, month: 366 };

const oneDayMs = 86_400_000;
const spanDays = (from: string, to: string): number =>
  Math.round((Date.parse(`${to}T00:00:00Z`) - Date.parse(`${from}T00:00:00Z`)) / oneDayMs) + 1;

/** 逗号分隔的有序分组列表：顺序即列顺序，同一属性不能出现两次。 */
const outputGroupBySchema = z
  .string()
  .trim()
  .default("annotator")
  .transform((value) =>
    value
      .split(",")
      .map((part) => part.trim())
      .filter((part) => part.length > 0),
  )
  .pipe(
    z
      .array(z.enum(outputDimensions))
      .min(1)
      .max(outputDimensions.length)
      .refine((dims) => new Set(dims).size === dims.length, "a property can only be grouped once"),
  );

/** 可重复的查询参数：`?gameId=a&gameId=b` 到手是数组，只给一个时是字符串。 */
const repeatable = <T extends z.ZodType<string>>(item: T) =>
  z.preprocess(
    (value) => (value === undefined ? [] : Array.isArray(value) ? value : [value]),
    z.array(item).max(64),
  );

export const statsOutputQuerySchema = z
  .object({
    from: calendarDaySchema,
    to: calendarDaySchema,
    bucket: z.enum(["day", "month"]).default("day"),
    groupBy: outputGroupBySchema,
    // `none` 是一等筛选值，与 clips / sessions 列表同款：匿名会话必须问得出来。
    annotatorId: repeatable(z.union([z.literal(unattached), z.string().uuid()])),
    gameId: repeatable(z.string().trim().min(1).max(120)),
    timeBasis: timeBasisSchema,
    tz: timeZoneSchema,
  })
  .refine((query) => query.from <= query.to, {
    message: "from must not be after to",
    path: ["from"],
  })
  .refine((query) => spanDays(query.from, query.to) <= maxOutputSpanDays[query.bucket], {
    message: "the range is longer than this bucket allows; switch to a coarser bucket",
    path: ["to"],
  });

export type StatsOutputQuery = z.infer<typeof statsOutputQuerySchema>;

export const idParamsSchema = z.object({ id: z.string().uuid() });

export type AnnotatorListQuery = z.infer<typeof annotatorListQuerySchema>;
export type DeviceListQuery = z.infer<typeof deviceListQuerySchema>;
export type SessionListQuery = z.infer<typeof sessionListQuerySchema>;
export type ClipListQuery = z.infer<typeof clipListQuerySchema>;
export type AuditQuery = z.infer<typeof auditQuerySchema>;
export type ClientEventListQuery = z.infer<typeof clientEventQuerySchema>;
export type StatsDailyQuery = z.infer<typeof statsDailyQuerySchema>;
export type DailyGroupBy = "game" | "annotator";

import type {
  AnnotatorListQuery,
  AuditQuery,
  ClipListQuery,
  DeviceListQuery,
  ClientEventListQuery,
  OutputDimension,
  SessionListQuery,
  StatsOutputQuery,
  TimeBasis,
} from "./browse-contracts.js";
import type { Page } from "./pagination.js";

/**
 * 逐段健康计数（docs/67 §3.4）。四个数都是客户端在封段时点的事实，随 clip 一起
 * 入库；此前只在 `meta` 的原始 JSON 里，界面上没有任何一处读它。
 */
export type ClipHealth = {
  duplicateFrames: number;
  sourceSuperseded: number;
  queueDrops: number;
  inputOverflow: number;
};

export type ContributionTotals = {
  sessionCount: number;
  clipCount: number;
  storedClipCount: number;
  verifiedClipCount: number;
  rejectedClipCount: number;
  /** 片段时长之和（n_frames / fps）。配额是按小时给的，计数答不了这个问题。 */
  capturedSeconds: number;
  /** 健康异常片段数：硬故障位非零或补帧占比越线（docs/146 的判定式）。 */
  unhealthyClipCount: number;
  storedBytes: number;
  firstActivityAt: string | null;
  lastActivityAt: string | null;
};

export type ContributionDay = {
  day: string;
  sessionCount: number;
  clipCount: number;
  verifiedClipCount: number;
  capturedSeconds: number;
  unhealthyClipCount: number;
  storedBytes: number;
};

export type Contribution = {
  /**
   * 分桶依据（docs/67 §3.3）。`recorded` = 录制墙钟（run 起始，缺失时退回登记
   * 时间），`registered` = 服务端登记时间。默认 `recorded`：一次跨夜录制、一次
   * 弱网重传，按登记时间会把工作量记到错误的那一天。
   */
  timeBasis: "recorded" | "registered";
  /** 天界时区（docs/97 P2）。与总览的每日进线同一缺省，跨页同一天必须是同一天。 */
  timeZone: string;
  totals: ContributionTotals;
  daily: ContributionDay[];
};

export type AnnotatorSummary = {
  id: string;
  alias: string | null;
  status: string;
  deviceCount: number;
  activeConsentCount: number;
  activeGrantCount: number;
  sessionCount: number;
  clipCount: number;
  verifiedClipCount: number;
  storedBytes: number;
  lastActivityAt: string | null;
  createdAt: string;
};

export type ConsentSummary = {
  id: string;
  policyVersion: string;
  purpose: string;
  status: string;
  allowsTrainableUse: boolean;
  /** 这份文书授权的范围（docs/108 §3.2）。 */
  scopes: string[];
  acceptedAt: string | null;
  expiresAt: string | null;
  revokedAt: string | null;
  /** 被后一份 consent 换掉的时刻。**不是**撤回（docs/108 D5）。 */
  supersededAt: string | null;
  retentionState: string;
};

export type GrantSummary = {
  id: string;
  gameId: string;
  deviceId: string;
  status: string;
  allowsTrainable: boolean;
  validFrom: string;
  validUntil: string;
  issuedBy: string;
  revokedAt: string | null;
};

export type AnnotatorDetail = AnnotatorSummary & {
  externalSubjectHash: string | null;
  tombstonedAt: string | null;
  consents: ConsentSummary[];
  grants: GrantSummary[];
  devices: DeviceSummary[];
  /**
   * How this annotator entered the system: the invite it redeemed and when, plus
   * the self-service credential's own timestamps. Null when the annotator was
   * hand-created by an operator (11 §6.2) rather than signing up — the two entry
   * paths coexist, and the console should show which one this was.
   */
  identity: AnnotatorIdentity | null;
  /**
   * Contact facts (docs/34 §5.4). Null when no profile row exists — profiles
   * are optional by design, so a missing one is a normal state, not an error.
   * Detail-only: list rows never carry contact data.
   */
  profile: AnnotatorProfile | null;
};

export type AnnotatorProfile = {
  displayName: string | null;
  phone: string | null;
  imKind: string | null;
  imHandle: string | null;
  region: string | null;
  updatedAt: string;
};

export type AnnotatorIdentity = {
  email: string;
  credentialStatus: "pending_verification" | "active" | "suspended";
  registeredAt: string;
  emailVerifiedAt: string | null;
  lastLoginAt: string | null;
  inviteCode: string | null;
  invitedBy: string | null;
  redeemedAt: string | null;
};

export type DeviceSummary = {
  id: string;
  clientDeviceId: string;
  annotatorId: string | null;
  annotatorAlias: string | null;
  status: string;
  windowsBuild: string;
  gpuPciId: string;
  gpuDriver: string;
  clientVersion: string;
  lastSeenAt: string | null;
  sessionCount: number;
  clipCount: number;
  verifiedClipCount: number;
  storedBytes: number;
  createdAt: string;
};

export type DeviceDetail = DeviceSummary & {
  credentialFingerprint: string;
  displaySnapshot: unknown;
  inputDevicesSnapshot: unknown;
  retiredAt: string | null;
  /** 旧登记流程留下的历史快照；新设备为 null，不参与 session admission。 */
  certifiedProfileId: string | null;
};

export type SessionSummary = {
  id: string;
  clientSessionId: string;
  gameId: string;
  gameTitle: string;
  deviceId: string;
  clientDeviceId: string;
  annotatorId: string | null;
  annotatorAlias: string | null;
  status: string;
  serverDataClassification: string;
  schemaVersion: string;
  catalogMode: string | null;
  startedAt: string;
  endedAt: string | null;
  runCount: number;
  clipCount: number;
  storedClipCount: number;
  verifiedClipCount: number;
  storedBytes: number;
  createdAt: string;
};

export type CaptureRunSummary = {
  id: string;
  runSeq: number;
  status: string;
  encoderBackend: string;
  width: number;
  height: number;
  fpsNum: number;
  fpsDen: number;
  frameCount: number | null;
  readyClipCount: number | null;
  startedAt: string | null;
  endedAt: string | null;
  stopReason: string | null;
  errorCode: string | null;
  preflightSchemaVersion: string | null;
  preflightId: string | null;
  preflightResult: string | null;
  preflightEvidenceSha256: string | null;
  /**
   * 这个 run 全部片段的健康计数合计（docs/67 §3.2）。**不是** `capture_runs.health_json`
   * ——那一列从第一天起就没有任何写入者。逐段计数是同一批事实里唯一真实上报过
   * 的，合计是它诚实的 run 级投影。
   */
  health: ClipHealth;
};

/**
 * 会话级可训练性汇总（issue #855 PR 5）。
 *
 * `clip_trainability` 是只读派生视图，console 不复算也不缓存；这里只是把它按
 * 会话数了一遍，回答界面上此前答不了的那个问题——**这一场卡在哪几道闸上**。
 * 逐 clip 详情继续给出该 clip 的完整 blocker 列表，两者口径一致。
 */
export type SessionTrainability = {
  /** 此刻回看可训练的片段数。视图是现在时的，consent 撤回会改写这个数。 */
  trainableClipCount: number;
  blockedClipCount: number;
  /** 已通过传输验证、仍在等 QA 裁决的片段数——session 级判定会落到的正是这些。 */
  awaitingQualityReviewClipCount: number;
  /** 该会话里出现过的 blocker 及各自命中的片段数，按命中数降序。 */
  blockers: { blocker: string; clipCount: number }[];
};

export type SessionDetail = SessionSummary & {
  captureEngine: string;
  nativeWidth: number;
  nativeHeight: number;
  fpsNum: number;
  fpsDen: number;
  clientVersion: string;
  gameVersion: string;
  // docs/55：指纹是观测结果，客户端拿不到时为 null，改由 missing reason 说明
  // 原因。这两个字段合起来正是「没采到」与「采到了但没批准」的区分方式——
  // 后者由 clip_trainability 的 executable_unapproved blocker 单独表达。
  gameExecutableSha256: string | null;
  gameExecutableSha256MissingReason: string | null;
  manifestSha256: string;
  catalogId: string | null;
  catalogVersion: string | null;
  catalogManifestSha256: string | null;
  gameProfileSha256: string | null;
  stopReason: string | null;
  rejectedReason: string | null;
  sealedAt: string | null;
  consentId: string | null;
  captureGrantId: string | null;
  /**
   * 这次会话实际声明的 device profile 版本。设备行只钉谱系（57 §2.1），所以
   * 「哪一版」这个问题只有会话行答得了：同一台机器在驱动地板抬高前后录的两次，
   * 在这里就是两个不同的版本行。
   */
  deviceProfile: {
    versionId: string;
    version: string;
    status: string;
    approvedForTrainable: boolean;
  } | null;
  /** 见 `SessionTrainability`。会话没有片段时全为 0、blockers 为空。 */
  trainability: SessionTrainability;
  runs: CaptureRunSummary[];
};

export type ClipSummary = {
  id: string;
  sessionId: string;
  gameId: string;
  deviceId: string;
  annotatorId: string | null;
  runSeq: number;
  segmentSeq: number;
  nFrames: number;
  durationSeconds: number | null;
  eventCount: number;
  ingestState: string;
  verificationState: string;
  qualityState: string;
  retentionState: string;
  sizeBytes: number;
  retentionDeadline: string;
  isTrainable: boolean;
  blockerCount: number;
  health: ClipHealth;
  /** 录制墙钟（所属 run 的起始时间），与 `createdAt` 的登记时间区分开。 */
  recordedAt: string | null;
  createdAt: string;
};

export type ArtifactSummary = {
  kind: string;
  bucketName: string;
  objectKey: string;
  expectedSha256: string;
  expectedSizeBytes: number;
  storageState: string;
  verificationState: string;
  storedAt: string | null;
  verifiedAt: string | null;
  lastErrorCode: string | null;
};

export type VerificationAttempt = {
  id: string;
  artifactKind: string;
  status: string;
  attempts: number;
  lastErrorCode: string | null;
  availableAt: string | null;
  updatedAt: string;
};

export type ClipDetail = ClipSummary & {
  revision: number;
  sessionFrameStart: number;
  qpcStart: string;
  qpcEnd: string;
  manifestSha256: string;
  rejectedReason: string | null;
  storedAt: string | null;
  verifiedAt: string | null;
  qualityReviewedAt: string | null;
  qualityReviewedBy: string | null;
  purgeRequestedAt: string | null;
  purgedAt: string | null;
  blockers: string[];
  meta: unknown;
  manifest: unknown;
  artifacts: ArtifactSummary[];
  verificationJobs: VerificationAttempt[];
};

export type GameSummary = {
  id: string;
  title: string;
  contentKind: string;
  allowedForCapture: boolean;
  approvedForTrainable: boolean;
  sessionCount: number;
  clipCount: number;
};

export type AuditEntry = {
  id: string;
  occurredAt: string;
  requestId: string | null;
  actorType: string;
  actorId: string | null;
  action: string;
  entityType: string;
  entityId: string;
  details: unknown;
};

export type PipelineOverview = {
  devices: { total: number; active: number; seenLast24h: number; seenLast7d: number };
  sessions: { total: number; last24h: number };
  clips: {
    total: number;
    pending: number;
    uploading: number;
    stored: number;
    retryWait: number;
    failed: number;
    verified: number;
    rejected: number;
    purgeActive: number;
  };
  /**
   * 与 `clips` **逐格同 population** 的录制时长（秒），口径 `sum(n_frames × fps_den / fps_num)`，
   * 与每日进线（docs/97）、产出报表（docs/145）、游戏分布（docs/153）同一个表达式。
   *
   * 为什么要有这一组：业务目标是按**小时**下的（「某款游戏 200h」），而本页此前
   * 每一格都是件数。件数换不出小时——片段长度从几秒到几分钟都有，除不出一个可用的
   * 平均值，所以「82,989 段」这个数回答不了「采了多少小时」。
   *
   * ⚠️ **口径与「游戏分布」面板刻意不同**：本页是管道读数，`clips` 的各格都是
   * `count(*) FROM clips` 的原样切分，**含已购删（purged）的段**——所以这里的
   * 时长也含。游戏分布是存量普查，按 docs/153 §2 剔除 purged。两个面板的合计
   * 因此对不上是预期的，不是 bug；要看「此刻还剩多少存量」去游戏分布。
   */
  clipSeconds: {
    total: number;
    pending: number;
    uploading: number;
    stored: number;
    retryWait: number;
    failed: number;
    verified: number;
    rejected: number;
    purgeActive: number;
  };
  storedBytes: number;
  trainableClips: number;
  /** 可训练片段的时长（秒）。现值回看，随 consent/grant 变动，同 `trainableClips`。 */
  trainableSeconds: number;
  /**
   * depth 产物的累计读数（docs/105 的第七件产物）。
   *
   * `bytes` **是 `storedBytes` 的子集**，不是它之外的另一份——把两个数相加会重复
   * 计算。单列的理由见 `browse-repository.ts` 的 `depthBytesForClip`：depth 只有
   * 装了实验包且做过 profile 标定的机器才产得出，件数就是那条线的产能读数；
   * 而它的字节又大到能独自主导存储曲线（实测 ≈ video 的 2.08 倍）。
   *
   * `seconds` 是**带 depth 的那些 clip 的完整录制时长**，不是「深度覆盖的时长」——
   * depth 按 15fps 抽偶数帧交付（docs/68 §6.5），两者不是一回事，别拿它当交付量。
   */
  depth: { clips: number; bytes: number; seconds: number };
};

/**
 * 全局每日进线（docs/97）。与 `ContributionDay` 的差异只有两列：活跃设备与活跃
 * 采集员的 distinct 计数——「今天有没有人掉线」是总览页的第一问。状态列
 * （stored/verified/unhealthy）是现值回看，不是当日事件计数。
 */
export type DailyIntakeDay = {
  day: string;
  sessionCount: number;
  clipCount: number;
  storedClipCount: number;
  verifiedClipCount: number;
  unhealthyClipCount: number;
  capturedSeconds: number;
  storedBytes: number;
  activeDeviceCount: number;
  activeAnnotatorCount: number;
  /**
   * 该天片段中**此刻回看**可训练的数量（docs/97 P2）。`clip_trainability` 是
   * 现在时视图——consent 撤销、grant 过期都会改写历史天的这个数。它回答的是
   * 「那天录的数据现在有多少可训练」，不是「那天当时有多少」。
   */
  trainableClipCount: number;
  /**
   * 该天**带已落地 depth 产物**的片段数（docs/105 的第七件产物）。
   *
   * 单列的理由见 `depthBytesForClip` 的注释：depth 只有装了实验包、且该游戏做过
   * profile 标定的机器才产得出，所以这个数**就是那条线的产能读数**；混在
   * `clipCount` 里等于没有。
   */
  depthClipCount: number;
  /**
   * 该天已落地的 depth 字节。**是 `storedBytes` 的子集**，不是它之外的另一份。
   *
   * 之所以要单列：实测 depth ≈ video 的 2.08 倍（docs/109 §4.2），一条带 depth 的
   * clip 会把总字节抬到约三倍。只看 `storedBytes` 的话，曲线上一个陡坡分不出是
   * 「多采了三倍」还是「有人开了 depth」——而这两种情况的处置完全相反。
   */
  depthBytes: number;
};

/** 维度拆分的一组（docs/97 §7.8）。daily 是稀疏序列，空天由整体轴补齐。 */
export type DailyIntakeGroup = {
  /** game_id / annotator_id；匿名会话在 annotator 维度下归入 "none"。 */
  key: string;
  label: string;
  totals: Omit<DailyIntakeDay, "day">;
  daily: DailyIntakeDay[];
};

export type DailyIntake = {
  timeBasis: TimeBasis;
  /** 天界时区（IANA 名）。分桶依据必须是响应里可读的事实（docs/97 §4.1）。 */
  timeZone: string;
  daily: DailyIntakeDay[];
  /** 窗口合计。distinct 计数按窗口整体去重，不等于逐日行相加。 */
  totals: Omit<DailyIntakeDay, "day">;
  /** 仅在请求带 groupBy 时出现。 */
  groupBy?: "game" | "annotator";
  groups?: DailyIntakeGroup[];
};

/**
 * 产出报表的一格（docs/145）。
 *
 * 九个数**全部可加**——逐格相加得行合计，逐行相加得列合计，前端因此不必为
 * 「合计」再要一次接口。这不是巧合而是选出来的：一旦混进 `count(DISTINCT …)`
 * （会话数、活跃设备数），「按区间整体去重」和「把每列加起来」就会给出两个都对
 * 但不相等的数——docs/97 的合计不等于逐日行相加，说的正是这件事。会话数因此
 * 不在这里；要看它去总览的每日进线。
 */
export type OutputMetrics = {
  clipCount: number;
  storedClipCount: number;
  verifiedClipCount: number;
  unhealthyClipCount: number;
  /** 片段时长之和（n_frames / fps）。运营是按小时读的，计数答不了这个问题。 */
  capturedSeconds: number;
  storedBytes: number;
  depthClipCount: number;
  /** **是 `storedBytes` 的子集**，不是它之外的另一份（同 `DailyIntakeDay`）。 */
  depthBytes: number;
  /** 此刻回看可训练的数量。`clip_trainability` 是现在时视图，历史桶会被改写。 */
  trainableClipCount: number;
};

export type OutputRow = {
  /**
   * 与 `groupBy` 等长、同序的分组键。匿名会话在 annotator 维度下是 `"none"`。
   * 零产出的在册采集员只有第一维有键，其余维度是空串——他没交，也就没有游戏。
   */
  keys: string[];
  /** 与 `keys` 等长的显示名。缺 alias 时退回 id；匿名同样是 `"none"`，措辞归界面。 */
  labels: string[];
  totals: OutputMetrics;
  /** 与 `buckets` 等长、逐位对应。没有产出的格子是全零，不是缺位。 */
  cells: OutputMetrics[];
};

export type OutputReport = {
  from: string;
  to: string;
  bucket: "day" | "month";
  timeBasis: TimeBasis;
  /** 天界时区（IANA 名）。分桶依据必须是响应里可读的事实（docs/97 §4.1）。 */
  timeZone: string;
  groupBy: OutputDimension[];
  /**
   * 列轴（`YYYY-MM-DD` 或 `YYYY-MM`）。由 from/to 直接生成，不取决于数据里
   * 出现过哪些桶——没产出的那天必须占一列，否则「这天没人交」在图上看不出来。
   */
  buckets: string[];
  rows: OutputRow[];
  columnTotals: OutputMetrics[];
  totals: OutputMetrics;
  /**
   * 在册（未注销）采集员数，仅在按采集员分组时有值。界面用它显示「N / M 人有
   * 产出」；不按人分组时这个问题没有意义，所以是 null 而不是 0。
   */
  enrolledAnnotatorCount: number | null;
};

/**
 * 游戏分布普查的一行（docs/153）：全量存量的单次快照，回答「语料押在哪几款游戏上」。
 * 与每日进线（滑窗 flow）、产出报表（显式区间 flow）分工不同——这里没有时间轴，
 * 只有此刻在库的构成。已购删（`retention_state = 'purged'`）的段不再是存量，剔除；
 * purge 申请中/进行中的段数据还在，仍计。
 */
export type GameDistributionRow = {
  gameId: string;
  /** games.title；缺行时退回 id（与产出报表同款兜底）。 */
  title: string;
  /** 片段时长之和（n_frames / fps，docs/145 同一口径）。含未完成上传的段。 */
  capturedSeconds: number;
  /** 至少有一段片段的会话数。game 划分 session，所以逐行相加等于合计。 */
  sessionCount: number;
  clipCount: number;
  /** 此刻回看可训练（`clip_trainability` 现在时视图，随 consent/grant 变动）。 */
  trainableClipCount: number;
  trainableSeconds: number;
};

export type GameDistribution = {
  /** 服务端出数时刻。普查是快照，「截至什么时候」必须是响应里可读的事实。 */
  asOf: string;
  /** 按时长降序。占比不在载荷里：五个数全部可加，前端用行与合计自己算。 */
  rows: GameDistributionRow[];
  totals: Omit<GameDistributionRow, "gameId" | "title">;
};

export type PipelineHealth = {
  verification: { pending: number; running: number; retryWait: number; failed: number; deadLetter: number };
  purge: { pending: number; running: number; retryWait: number; failed: number; deadLetter: number };
  uploads: { activeIntents: number; activeMultiparts: number; expiredMultiparts: number };
  /** storageByDay 的天界时区（docs/97 P2）。与每日进线同一缺省。 */
  timeZone: string;
  storageByDay: { day: string; storedBytes: number; clipCount: number }[];
  recentFailures: {
    kind: "verification" | "purge";
    entityId: string;
    errorCode: string | null;
    attempts: number;
    updatedAt: string;
  }[];
  /** 见 `QualityBacklog`。 */
  qualityBacklog: QualityBacklog;
};

/**
 * QA 积压（issue #855 PR 5）。
 *
 * `quality_state` 按设计是人工裁决点：`clips` 四个状态维度里只有它带
 * `quality_reviewed_by`，0001 的 CHECK 又要求"不是 pending 就必须有审阅时间"。
 * 既然判定必然由人做，那么可运营与否就不该靠回放去**预测**吞吐，而应该把真实
 * 积压**直接摆出来**：等的有多少、最老那条等了多久、超阈值的有几条。
 *
 * 计入队列的口径是"此刻确实可以被裁决的片段"：通过了传输验证（`recordClipQuality`
 * 与 `recordSessionQuality` 的 WHERE 条件）、仍是 `pending`、且没有进入删除流程。
 * 等待时长从 `verified_at` 起算，因为在那之前判定根本不被允许。
 */
export type QualityBacklog = {
  pendingClipCount: number;
  /** 这些片段分布在几场录制上——判定按 session 做，这才是实际要点几次。 */
  pendingSessionCount: number;
  /** 最老那条的 `verified_at`；队列空时为 null。 */
  oldestVerifiedAt: string | null;
  /** 最老那条已经等了多久（小时，按库时钟算）。队列空时为 null。 */
  oldestWaitHours: number | null;
  /** 超期阈值。24 小时来自 issue #855 §C 给 QA 定的运行目标。 */
  overdueThresholdHours: number;
  overdueClipCount: number;
  overdueSessionCount: number;
};

/**
 * session 根审计快照（docs/47）。摄取面从第一天就在写 `session_artifacts`，
 * console 却从来没有读过它——「这次会话的目录/身份/mapping 快照有没有上来」
 * 在界面上无法回答（docs/67 §3.6）。
 */
export type SessionArtifactSummary = {
  kind: string;
  objectKey: string;
  contentType: string;
  expectedSha256: string;
  expectedSizeBytes: number;
  ingestState: string;
  verificationState: string;
  storedAt: string | null;
  verifiedAt: string | null;
  rejectedReason: string | null;
};

/** 白名单客户端遥测（docs/25 §0）。已上云多时，此前 console 无展示入口。 */
export type ClientEventEntry = {
  eventId: string;
  deviceId: string;
  annotatorId: string | null;
  sessionId: string | null;
  occurredAt: string;
  receivedAt: string;
  kind: string;
  severity: string;
  clientVersion: string;
  details: unknown;
  retentionState: string;
};

/**
 * docs/66 批次 C：`/app/v1/readiness` 的「授权」灯。只回答两件事——此刻有没有
 * 在窗口内的 active grant、下一个到期时间是什么。不含 grant 内容（游戏、profile）：
 * 授权明细属于治理面，标注员侧只需要「能不能开录」。
 */

export interface BrowseRepository {
  listAnnotators(query: AnnotatorListQuery): Promise<Page<AnnotatorSummary>>;
  getAnnotator(id: string): Promise<AnnotatorDetail | null>;
  listDevices(query: DeviceListQuery): Promise<Page<DeviceSummary>>;
  getDevice(id: string): Promise<DeviceDetail | null>;
  listSessions(query: SessionListQuery): Promise<Page<SessionSummary>>;
  getSession(id: string): Promise<SessionDetail | null>;
  listClips(query: ClipListQuery): Promise<Page<ClipSummary>>;
  getClip(id: string): Promise<ClipDetail | null>;
  listGames(): Promise<GameSummary[]>;
  listAuditEvents(query: AuditQuery): Promise<Page<AuditEntry>>;
  contributionForAnnotator(
    id: string,
    days: number,
    basis?: TimeBasis,
    tz?: string,
  ): Promise<Contribution>;
  contributionForDevice(
    id: string,
    days: number,
    basis?: TimeBasis,
    tz?: string,
  ): Promise<Contribution>;
  listSessionArtifacts(sessionId: string): Promise<SessionArtifactSummary[]>;
  listClientEvents(query: ClientEventListQuery): Promise<Page<ClientEventEntry>>;
  pipelineOverview(): Promise<PipelineOverview>;
  pipelineHealth(days: number, tz?: string): Promise<PipelineHealth>;
  dailyIntake(
    days: number,
    basis: TimeBasis | undefined,
    tz: string,
    groupBy?: "game" | "annotator",
  ): Promise<DailyIntake>;
  outputReport(query: StatsOutputQuery): Promise<OutputReport>;
  gameDistribution(): Promise<GameDistribution>;
}

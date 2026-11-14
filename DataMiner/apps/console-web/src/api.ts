/** Dispatched on the global object whenever the API answers 401, so the shell can
 *  drop an expired session instead of leaving a stale page on screen. */
export const UNAUTHENTICATED_EVENT = "console:unauthenticated";

/** The annotator surface's own 401 signal (docs/34 §6.1). Kept apart from the
 *  operator event: the two sessions are different cookies for different people,
 *  and "the admin is also an annotator" makes one browser holding both normal. */
export const APP_UNAUTHENTICATED_EVENT = "app:unauthenticated";

export type LoginMode = "password_otp" | "password_only" | "cf_access";

/** What the sign-in screen needs before a session exists. */
export type ConsoleMeta = {
  environment: ConsoleEnvironment;
  loginMode: LoginMode;
};

export type OperatorRole = "viewer" | "operator" | "admin";

export type Operator = {
  id: string;
  email: string;
  role: OperatorRole;
  status: string;
  lastLoginAt: string | null;
  createdAt: string;
};

/** An operator row plus the annotator they registered as, if they did. The two
 *  identities stay separate tables (docs/11 §4.1); this is the derived link. */
export type OperatorWithAnnotator = Operator & {
  annotator: { annotatorId: string; alias: string | null } | null;
};

export type ConsoleEnvironment = "development" | "staging" | "production";

export type SessionInfo = {
  operator: Operator;
  capabilities: string[];
  session: { expiresAt: string; sudoUntil: string | null };
  /** Which deployment this is. The header used to hardcode "staging", so the
   *  production console labelled itself staging — the one label an operator
   *  must be able to trust before acting on real data. Optional so a console-api
   *  that predates the field degrades to "unknown" rather than to a wrong name. */
  environment?: ConsoleEnvironment;
  /** Which factor a destructive write re-confirms with. Under cf_access there
   * is no console password, so the step-up re-verifies the Access assertion the
   * edge already attaches and the UI must not prompt for one. */
  stepUp: StepUpFactor;
};

export type StepUpFactor = "password" | "cf_access_assertion";

/**
 * The session's step-up factor, remembered so the confirmation dialogs can read
 * it without threading a prop through every page that has one.
 *
 * Presentation only — it decides whether to render a password field. The server
 * picks the factor it actually verifies from its own configuration, so a stale
 * or wrong value here cannot weaken anything; it would only ask for the wrong
 * thing. Defaults to the stricter prompt.
 */
let stepUpFactor: StepUpFactor = "password";

export function rememberStepUpFactor(factor: StepUpFactor): void {
  stepUpFactor = factor;
}

export function currentStepUpFactor(): StepUpFactor {
  return stepUpFactor;
}

export type Page<T> = { items: T[]; nextCursor: string | null };

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
  certifiedProfileId: string | null;
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

export type InviteSummary = {
  code: string;
  displayCode: string;
  note: string | null;
  status: "active" | "revoked";
  maxUses: number;
  usedCount: number;
  expiresAt: string | null;
  createdBy: string | null;
  createdAt: string;
  revokedBy: string | null;
  revokedAt: string | null;
  usedByAnnotatorId: string | null;
  usedAt: string | null;
};

export type CreatedInvites = { codes: string[]; summaries: InviteSummary[] };

/** Contact facts (docs/34 §5.4). Every field optional; a null row is normal. */
export type AnnotatorProfile = {
  displayName: string | null;
  phone: string | null;
  imKind: string | null;
  imHandle: string | null;
  region: string | null;
  updatedAt: string;
};

/** The PUT shape: whole-object replace, omitted fields clear. */
export type ProfileInput = {
  displayName?: string;
  phone?: string;
  imKind?: "wechat" | "qq" | "telegram" | "discord" | "other";
  imHandle?: string;
  region?: string;
};

export type AppTerms = {
  policyVersion: string;
  purpose: string;
  termsHash: string;
  allowsTrainableUse: boolean;
  body: string;
  /** 这份文书授权的范围（docs/108 §3.2）。老服务端不返回。 */
  scopes?: string[];
};

/** 本人 consent 与服务端在服务的条款的差（docs/108 §3.3）。 */
export type AppConsentState = {
  consent: {
    consentId: string;
    policyVersion: string;
    termsHash: string;
    allowsTrainableUse: boolean;
    scopes: string[];
    acceptedAt: string | null;
  } | null;
  currentPolicyVersion: string;
  currentTermsHash: string;
  currentAllowsTrainableUse: boolean;
  currentScopes: string[];
  matchesCurrentTerms: boolean;
  /** 补签会新增哪些范围。 */
  pendingScopes: string[];
  resignRequired: boolean;
};

/** scope 词的中文说法。未知词原样回显——漏掉它比显示一个英文词糟得多。 */
export function consentScopeLabel(scope: string): string {
  switch (scope) {
    case "input":
      return "键鼠输入";
    case "video":
      return "游戏画面";
    case "audio":
      return "游戏进程音频";
    case "depth":
      return "深度与相机参数";
    case "public_release":
      return "公开发布";
    case "trainable_research":
      return "模型训练研究";
    default:
      return scope;
  }
}

export type ClaimPreflight = {
  kind: "activate" | "reset";
  email: string;
  terms?: AppTerms;
};

export type ClaimIssued = {
  annotatorId: string;
  claimUrl: string;
  expiresInSeconds: number;
};

export type MyDevice = {
  deviceId: string;
  clientDeviceId: string;
  status: string;
  clientVersion: string;
  lastSeenAt: string | null;
  createdAt: string;
};

export type AnnotatorDetail = AnnotatorSummary & {
  externalSubjectHash: string | null;
  identity: AnnotatorIdentity | null;
  profile: AnnotatorProfile | null;
  consents: {
    id: string;
    policyVersion: string;
    purpose: string;
    status: string;
    allowsTrainableUse: boolean;
    scopes: string[];
    acceptedAt: string | null;
    expiresAt: string | null;
    revokedAt: string | null;
    supersededAt: string | null;
  }[];
  grants: {
    id: string;
    gameId: string;
    deviceId: string;
    status: string;
    allowsTrainable: boolean;
    validFrom: string;
    validUntil: string;
    issuedBy: string;
  }[];
  devices: DeviceSummary[];
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

export type SessionDetail = SessionSummary & {
  captureEngine: string;
  nativeWidth: number;
  nativeHeight: number;
  fpsNum: number;
  fpsDen: number;
  clientVersion: string;
  gameVersion: string;
  manifestSha256: string;
  catalogId: string | null;
  catalogVersion: string | null;
  catalogManifestSha256: string | null;
  gameProfileSha256: string | null;
  stopReason: string | null;
  rejectedReason: string | null;
  consentId: string | null;
  captureGrantId: string | null;
  deviceProfile: {
    versionId: string;
    version: string;
    status: string;
    approvedForTrainable: boolean;
  } | null;
  /**
   * 会话级可训练性汇总（issue #855 PR 5）。服务端读 `clip_trainability` 只读视图，
   * 前端不复算，也不猜。
   */
  trainability: {
    trainableClipCount: number;
    blockedClipCount: number;
    awaitingQualityReviewClipCount: number;
    blockers: { blocker: string; clipCount: number }[];
  };
  runs: {
    id: string;
    runSeq: number;
    status: string;
    encoderBackend: string;
    width: number;
    height: number;
    frameCount: number | null;
    stopReason: string | null;
    errorCode: string | null;
    preflightResult: string | null;
    preflightEvidenceSha256: string | null;
    readyClipCount: number | null;
    health: ClipHealth;
  }[];
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
  recordedAt: string | null;
  createdAt: string;
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
  blockers: string[];
  meta: unknown;
  manifest: unknown;
  artifacts: {
    kind: string;
    bucketName: string;
    objectKey: string;
    expectedSha256: string;
    expectedSizeBytes: number;
    storageState: string;
    verificationState: string;
    storedAt: string | null;
    lastErrorCode: string | null;
  }[];
  verificationJobs: {
    id: string;
    artifactKind: string;
    status: string;
    attempts: number;
    lastErrorCode: string | null;
    updatedAt: string;
  }[];
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

// status and approvedForTrainable are orthogonal — an approved row can still be
// non-trainable — so they stay two fields and get two columns, never one badge.
export type CatalogVersion = {
  id: string;
  kind: string;
  catalogId: string;
  version: string;
  gameId: string | null;
  status: string;
  contentHash: string;
  exactPayloadAvailable: boolean;
  payloadByteSize: number | null;
  publishable: boolean;
  approvedForTrainable: boolean;
  approvedBy: string | null;
  approvedAt: string | null;
  retiredAt: string | null;
  revokedAt: string | null;
  createdAt: string;
  updatedAt: string;
};

export type CatalogRelease = {
  id: string;
  channel: string;
  catalogId: string;
  catalogVersion: string;
  status: string;
  envelopeSha256: string;
  signingKeyId: string;
  entryCount: number;
  gameCount: number;
  createdBy: string;
  publishedBy: string | null;
  publishedAt: string | null;
  supersededAt: string | null;
  revokedBy: string | null;
  revokedAt: string | null;
  createdAt: string;
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

export type TimeBasis = "recorded" | "registered";

export type Contribution = {
  /** 分桶依据（docs/67 §3.3）：录制墙钟还是服务端登记时间。 */
  timeBasis: TimeBasis;
  /** 天界时区（docs/97 P2），与总览的每日进线同一缺省。 */
  timeZone: string;
  totals: {
    sessionCount: number;
    clipCount: number;
    storedClipCount: number;
    verifiedClipCount: number;
    rejectedClipCount: number;
    capturedSeconds: number;
    unhealthyClipCount: number;
    storedBytes: number;
    firstActivityAt: string | null;
    lastActivityAt: string | null;
  };
  daily: {
    day: string;
    sessionCount: number;
    clipCount: number;
    verifiedClipCount: number;
    capturedSeconds: number;
    unhealthyClipCount: number;
    storedBytes: number;
  }[];
};

/** 逐段健康计数（docs/67 §3.4）。 */
export type ClipHealth = {
  duplicateFrames: number;
  sourceSuperseded: number;
  queueDrops: number;
  inputOverflow: number;
};

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

/**
 * 会话验收报告（docs/136，schema dm-session-acceptance/1）。服务端从 R2 原样
 * 透传 accept.py 落的 JSON，所以字段保持 Python 侧的 snake_case，不做映射。
 * 报告是缓存不是判决：与 clip meta 冲突时以 meta 为准。
 */
export type AcceptanceQcCheck = {
  id: string;
  reading?: unknown;
  passed: boolean;
  /** false = 该判据对这局游戏未启用（词表/档位不适用），展示置灰。 */
  enabled?: boolean;
  source?: string;
  value?: unknown;
  threshold?: unknown;
  [extra: string]: unknown;
};

export type AcceptanceReport = {
  schema: string;
  session: string;
  clips_total: number;
  clips_complete: number;
  audio_present_clips: number;
  game_id: string;
  games_seen: Record<string, number>;
  mixed_session: boolean;
  hours: number;
  fps_seen: Record<string, number>;
  health: {
    duplicate_frames: number;
    queue_drops: number;
    input_overflow: number;
    empty_events_clips: number;
    meta_unreadable: number;
    manifest_mismatch: number;
  };
  qc: {
    ran: boolean;
    /** ran=false 时给出原因（无 game_id / 未注册 / QC 自身报错）。 */
    reason?: string;
    tier?: string;
    passed?: boolean;
    failed_checks?: string[];
    checks?: AcceptanceQcCheck[];
  };
  findings: string[];
  passed: boolean;
};

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

export type PipelineHealth = {
  verification: { pending: number; running: number; retryWait: number; failed: number; deadLetter: number };
  purge: { pending: number; running: number; retryWait: number; failed: number; deadLetter: number };
  uploads: { activeIntents: number; activeMultiparts: number; expiredMultiparts: number };
  /** storageByDay 的天界时区（docs/97 P2）。 */
  timeZone: string;
  storageByDay: { day: string; storedBytes: number; clipCount: number }[];
  recentFailures: {
    kind: "verification" | "purge";
    entityId: string;
    errorCode: string | null;
    attempts: number;
    updatedAt: string;
  }[];
  /**
   * QA 积压（issue #855 PR 5）。判定是人做的，所以这里摆的是真实队列，不是预测。
   * `pendingSessionCount` 才是"还要点几次"——判定的粒度是一场录制。
   */
  qualityBacklog: {
    pendingClipCount: number;
    pendingSessionCount: number;
    oldestVerifiedAt: string | null;
    oldestWaitHours: number | null;
    overdueThresholdHours: number;
    overdueClipCount: number;
    overdueSessionCount: number;
  };
};

export type PreviewGrant = {
  url: string;
  objectKey: string;
  expiresInSeconds: number;
  expiresAt: string;
  sizeBytes: number;
};

// --- data-review surface (docs/33) ---

export type ReviewSource = {
  id: string;
  bucket: string;
  prefixes: string[];
  dbAuthority: "production" | "none";
};

export type ReviewTreeObject = {
  key: string;
  sizeBytes: number;
  lastModified: string | null;
};

export type ReviewTreePage = {
  source: string;
  prefix: string;
  directories: string[];
  objects: ReviewTreeObject[];
  nextCursor: string | null;
};

export type ReviewObjectInfo = {
  source: string;
  key: string;
  sizeBytes: number;
  lastModified: string | null;
  dbAuthority: "production" | "none";
  dbRegistered: boolean | null;
  artifact: { clipId: string | null; kind: string; storageState: string } | null;
};

export type ReviewVideoGrant = PreviewGrant & { source: string };

// One row of p0.jsonl / frames.jsonl. Everything beyond the index signature is
// what the reviewer actually renders; the raw row keeps whatever else it had.
export type ReviewFrameRow = {
  frame: number;
  gframe: number | null;
  video_t_ms?: number;
  session_frame?: number;
  duplicate?: boolean;
  keys: number[];
  buttons?: number[];
  actions?: string[];
  pointerDx: number;
  pointerDy: number;
  pointerBins: number[];
  events?: { type: string; vk?: number; dx?: number; dy?: number }[];
  src?: string;
  [extra: string]: unknown;
};

export type ReviewFramesResult = {
  source: string;
  key: string;
  schema: "dm1" | "raw-v1" | "raw-events-v1";
  rows: ReviewFrameRow[];
  fps: number | null;
  meta: unknown;
  provenance: {
    objectKey: string;
    sizeBytes: number;
    sha256: string;
    sha256Verified: boolean;
    frameCountVerified: boolean;
  };
};

export type ReviewMetaResult = {
  source: string;
  key: string;
  sizeBytes: number;
  meta: unknown;
};

export type ReviewVerdict = {
  id: string;
  source: string;
  subjectKind: "raw_segment" | "dataset_chunk" | "session";
  subjectKey: string;
  result: "passed" | "failed";
  note: string | null;
  operatorId: string;
  operatorEmail: string | null;
  createdAt: string;
};

export type HfReleaseItem = {
  repoId: string;
  path: string;
  revision: string;
  sizeBytes: number | null;
  sessionKey: string | null;
  sampleKey: string | null;
};

export type HfSubjectStatus = {
  computed: "released" | "not_released" | "mismatch";
  matches: HfReleaseItem[];
  manualMark: {
    id: string;
    source: string;
    subjectKey: string;
    mark: string;
    note: string | null;
    operatorId: string;
    createdAt: string;
  } | null;
  checkedAt: string | null;
};

export type HfReconcileResult = {
  repoId: string;
  revision: string;
  itemCount: number;
  matchableCount: number;
  checkedAt: string;
};

export type Overview = {
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
   * 与 `clips` 逐格同 population 的录制时长（秒）。**含已购删的段**——本页是管道
   * 读数，不是存量普查；要看「此刻还剩多少」去游戏分布面板（那里剔 purged）。
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
  trainableSeconds: number;
  /**
   * depth 累计读数。**`bytes` 是 `storedBytes` 的子集**，两者相加会重复计算。
   * `seconds` 是带 depth 的那些 clip 的**完整录制时长**，不是深度覆盖时长
   * （depth 按 15fps 抽偶数帧交付）。
   */
  depth: { clips: number; bytes: number; seconds: number };
};

/**
 * 全局每日进线（docs/97）。状态列是现值回看，不是当日事件计数；`recorded`
 * 口径下补传会回填历史天的数字。
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
  /** 此刻回看可训练的数量——现在时视图，历史天的数会随 consent/grant 变动。 */
  trainableClipCount: number;
  /** 该天带已落地 depth 产物的片段数——那条线的产能读数。 */
  depthClipCount: number;
  /** 该天已落地的 depth 字节。**是 `storedBytes` 的子集**，不要相加。 */
  depthBytes: number;
};

export type DailyGroupBy = "game" | "annotator";

/** 维度拆分的一组（docs/97 §7.8）。daily 稀疏，空天由整体轴补齐。 */
export type DailyIntakeGroup = {
  key: string;
  label: string;
  totals: Omit<DailyIntakeDay, "day">;
  daily: DailyIntakeDay[];
};

export type DailyIntake = {
  timeBasis: TimeBasis;
  /** 天界时区（IANA 名），服务端缺省 Asia/Shanghai。 */
  timeZone: string;
  daily: DailyIntakeDay[];
  totals: Omit<DailyIntakeDay, "day">;
  groupBy?: DailyGroupBy;
  groups?: DailyIntakeGroup[];
};

/**
 * 产出报表（docs/145）。九个数全部可加——逐格相加得行合计，逐行相加得列合计，
 * 所以界面切「合计 / 均值」不需要再请求一次。
 */
export type OutputMetrics = {
  clipCount: number;
  storedClipCount: number;
  verifiedClipCount: number;
  unhealthyClipCount: number;
  capturedSeconds: number;
  storedBytes: number;
  depthClipCount: number;
  /** **是 `storedBytes` 的子集**，不要相加。 */
  depthBytes: number;
  trainableClipCount: number;
};

export type OutputDimension = "annotator" | "game";
export type OutputBucket = "day" | "month";

export type OutputRow = {
  /** 与 groupBy 等长、同序。匿名会话在采集员维度下是 "none"。 */
  keys: string[];
  /** 与 keys 等长的显示名。零产出行的后续维度是空串——他没交，也就没有游戏。 */
  labels: string[];
  totals: OutputMetrics;
  /** 与 buckets 等长、逐位对应。没产出的格子是全零，不是缺位。 */
  cells: OutputMetrics[];
};

export type OutputReport = {
  from: string;
  to: string;
  bucket: OutputBucket;
  timeBasis: TimeBasis;
  timeZone: string;
  groupBy: OutputDimension[];
  buckets: string[];
  rows: OutputRow[];
  columnTotals: OutputMetrics[];
  totals: OutputMetrics;
  /** 在册采集员数；不按采集员分组时为 null——那时这个问题没有意义。 */
  enrolledAnnotatorCount: number | null;
};

export type OutputQuery = {
  from: string;
  to: string;
  bucket: OutputBucket;
  groupBy: OutputDimension[];
  annotatorId: string[];
  gameId: string[];
  timeBasis: TimeBasis;
};

/**
 * 游戏分布普查（docs/153）：全量存量的单次快照，无时间轴。已购删的段不计。
 * 五个数全部可加，占比由前端用行与合计自己算。
 */
export type GameDistributionRow = {
  gameId: string;
  title: string;
  /** 片段时长之和（n_frames / fps）。含未完成上传的段。 */
  capturedSeconds: number;
  /** 至少有一段片段的会话数。game 划分 session，逐行相加等于合计。 */
  sessionCount: number;
  clipCount: number;
  /** 此刻回看可训练——现在时视图，随 consent/grant 变动。 */
  trainableClipCount: number;
  trainableSeconds: number;
};

export type GameDistribution = {
  /** 服务端出数时刻。普查是快照，「截至什么时候」是响应里可读的事实。 */
  asOf: string;
  /** 按时长降序。 */
  rows: GameDistributionRow[];
  totals: Omit<GameDistributionRow, "gameId" | "title">;
};

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly retryAfterSeconds: number | null;

  constructor(code: string, message: string, status: number, retryAfterSeconds: number | null) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

/**
 * The one route that answers with bytes instead of JSON (depth frames). Kept
 * beside `request` rather than folded into it: every other caller wants a
 * parsed body, and a generic "sometimes JSON, sometimes not" helper is how a
 * 3.5 MiB ArrayBuffer ends up being handed to `JSON.parse`.
 */
async function requestBytes(path: string): Promise<ArrayBuffer> {
  const response = await fetch(`/console/v1${path}`, {
    method: "GET",
    credentials: "same-origin",
  });
  if (!response.ok) {
    if (response.status === 401) {
      globalThis.dispatchEvent?.(new CustomEvent(UNAUTHENTICATED_EVENT));
    }
    const payload = await response.json().catch(() => null);
    throw new ApiError(
      payload?.code ?? "request_failed",
      payload?.message ?? "the request failed",
      response.status,
      null,
    );
  }
  return response.arrayBuffer();
}

async function request<T>(
  path: string,
  init: RequestInit & { idempotencyKey?: string } = {},
): Promise<T> {
  const method = init.method ?? "GET";
  const headers = new Headers(init.headers);
  if (init.idempotencyKey) headers.set("idempotency-key", init.idempotencyKey);
  if (method !== "GET") {
    // The server rejects state changing requests without this header, which a
    // cross-origin form post cannot set.
    headers.set("x-dmc-csrf", "1");
    if (init.body !== undefined) headers.set("content-type", "application/json");
  }

  const response = await fetch(`/console/v1${path}`, {
    ...init,
    method,
    headers,
    credentials: "same-origin",
  });

  if (response.status === 204) return undefined as T;
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    // A session that expired mid-visit otherwise surfaced as an English error
    // string inside whichever panel happened to refresh — the operator is left
    // reading "a console session is required" with no way to act on it. The
    // shell listens for this and returns to the sign-in form.
    if (response.status === 401) {
      globalThis.dispatchEvent?.(new CustomEvent(UNAUTHENTICATED_EVENT));
    }
    const retryAfter = response.headers.get("retry-after");
    throw new ApiError(
      payload?.code ?? "request_failed",
      payload?.message ?? "the request failed",
      response.status,
      retryAfter ? Number(retryAfter) : null,
    );
  }
  return payload as T;
}

/**
 * The annotator surface's transport (docs/34 §6.1). Same discipline as
 * `request` with three differences that mirror the server: the `/app/v1`
 * prefix, the `x-dma-csrf` header, and the annotator's own 401 event. The
 * session is the `__Host-dma_session` HttpOnly cookie — this file still holds
 * no token and touches no storage.
 */
async function appRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  const method = init.method ?? "GET";
  const headers = new Headers(init.headers);
  if (method !== "GET") {
    headers.set("x-dma-csrf", "1");
    if (init.body !== undefined) headers.set("content-type", "application/json");
  }

  const response = await fetch(`/app/v1${path}`, {
    ...init,
    method,
    headers,
    credentials: "same-origin",
  });

  if (response.status === 204) return undefined as T;
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    if (response.status === 401) {
      globalThis.dispatchEvent?.(new CustomEvent(APP_UNAUTHENTICATED_EVENT));
    }
    const retryAfter = response.headers.get("retry-after");
    throw new ApiError(
      payload?.code ?? "request_failed",
      payload?.message ?? "the request failed",
      response.status,
      retryAfter ? Number(retryAfter) : null,
    );
  }
  return payload as T;
}

function appPost<T>(path: string, body: unknown): Promise<T> {
  return appRequest<T>(path, { method: "POST", body: JSON.stringify(body) });
}

function search(params: Record<string, string | number | string[] | undefined>): string {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === "") continue;
    // 多选筛选走可重复参数（?gameId=a&gameId=b），与服务端的 repeatable 对齐。
    if (Array.isArray(value)) {
      for (const item of value) if (item !== "") query.append(key, item);
      continue;
    }
    query.set(key, String(value));
  }
  const encoded = query.toString();
  return encoded ? `?${encoded}` : "";
}

// A key is minted per attempt, not per submit: a retry after a network error
// replays the same write instead of creating a second row.
export function newIdempotencyKey(): string {
  return `web-${crypto.randomUUID()}`;
}

function post<T>(path: string, body: unknown, idempotencyKey: string): Promise<T> {
  return request<T>(path, { method: "POST", body: JSON.stringify(body), idempotencyKey });
}

function patch<T>(path: string, body: unknown, idempotencyKey: string): Promise<T> {
  return request<T>(path, { method: "PATCH", body: JSON.stringify(body), idempotencyKey });
}

export type DeviceCredential = {
  device: { id: string; clientDeviceId: string; status: string; credentialFingerprint: string };
  token: string | null;
};

export const api = {
  // Two shapes depending on the server's login mode: password_only answers with
  // the operator (a session cookie is already set), password_otp answers with a
  // challenge to carry into the OTP step. The caller branches on which arrived.
  login: (email: string, password: string) =>
    request<{ operator: Operator } | { challengeId: string; expiresInSeconds: number }>(
      "/auth/login",
      { method: "POST", body: JSON.stringify({ email, password }) },
    ),
  verifyOtp: (challengeId: string, code: string) =>
    request<{ operator: Operator }>("/auth/otp/verify", {
      method: "POST",
      body: JSON.stringify({ challengeId, code }),
    }),
  resendOtp: (challengeId: string) =>
    request<{ challengeId: string; expiresInSeconds: number }>("/auth/otp/resend", {
      method: "POST",
      body: JSON.stringify({ challengeId }),
    }),
  session: () => request<SessionInfo>("/auth/session"),
  logout: () => request<{ status: string }>("/auth/logout", { method: "POST" }),

  // Exchanges a Cloudflare Access identity for a console session (cf_access
  // mode). The Access assertion is injected by the edge, not the browser, so this
  // carries no body. Answers 404 when the console is not in cf_access mode, which
  // is how the SPA falls back to the password form.
  cfAccessLogin: () => request<{ operator: Operator }>("/auth/cf-access/login", { method: "POST" }),

  // Operator (console administrator) management. Admin-only on the server; these
  // writes are not sudo-gated and carry no idempotency key because the endpoints
  // do not read one — inviting is create-by-email (unique) and updates are
  // idempotent by construction.
  operators: () => request<{ operators: OperatorWithAnnotator[] }>("/operators"),
  // Mints the caller their own annotator invite so they can contribute capture
  // data with the client. Returns the existing unspent one if there is one.
  issueMyAnnotatorInvite: () =>
    request<{ code: string; summary: InviteSummary }>("/operators/me/annotator-invite", {
      method: "POST",
    }),
  inviteOperator: (email: string, role: OperatorRole) =>
    request<{ operator: Operator }>("/operators", {
      method: "POST",
      body: JSON.stringify({ email, role }),
    }),
  updateOperator: (id: string, changes: { role?: OperatorRole; status?: "active" | "suspended" }) =>
    request<{ operator: Operator }>(`/operators/${id}`, {
      method: "PATCH",
      body: JSON.stringify(changes),
    }),

  // Invite acceptance and password recovery. All three are unauthenticated: the
  // one-time token in the link is the credential. The request path answers 202
  // regardless of whether the address exists, so its wording must not leak that.
  acceptInvite: (challengeId: string, token: string, password: string) =>
    request<{ status: string }>("/auth/invite/accept", {
      method: "POST",
      body: JSON.stringify({ challengeId, token, password }),
    }),
  requestPasswordReset: (email: string) =>
    request<{ status: string }>("/auth/password-reset/request", {
      method: "POST",
      body: JSON.stringify({ email }),
    }),
  confirmPasswordReset: (challengeId: string, token: string, password: string) =>
    request<{ status: string }>("/auth/password-reset/confirm", {
      method: "POST",
      body: JSON.stringify({ challengeId, token, password }),
    }),
  overview: () => request<Overview>("/stats/overview"),
  pipelineHealth: (days: number) => request<PipelineHealth>(`/stats/pipeline${search({ days })}`),
  statsDaily: (days: number, timeBasis?: TimeBasis, groupBy?: DailyGroupBy) =>
    request<DailyIntake>(`/stats/daily${search({ days, timeBasis, groupBy })}`),

  statsOutput: (query: OutputQuery) =>
    request<OutputReport>(
      `/stats/output${search({
        from: query.from,
        to: query.to,
        bucket: query.bucket,
        groupBy: query.groupBy.join(","),
        annotatorId: query.annotatorId,
        gameId: query.gameId,
        timeBasis: query.timeBasis,
      })}`,
    ),
  statsGameDistribution: () => request<GameDistribution>("/stats/game-distribution"),
  previewClip: (id: string) =>
    request<PreviewGrant>(`/clips/${id}/preview-url`, { method: "POST" }),
  games: () => request<{ games: GameSummary[] }>("/games"),
  meta: () => request<ConsoleMeta>("/meta"),
  annotators: (params: Record<string, string | number | undefined>) =>
    request<Page<AnnotatorSummary>>(`/annotators${search(params)}`),
  annotator: (id: string) => request<AnnotatorDetail>(`/annotators/${id}`),
  annotatorContribution: (id: string, days: number, timeBasis?: TimeBasis) =>
    request<Contribution>(`/annotators/${id}/contribution${search({ days, timeBasis })}`),
  devices: (params: Record<string, string | number | undefined>) =>
    request<Page<DeviceSummary>>(`/devices${search(params)}`),
  device: (id: string) => request<DeviceDetail>(`/devices/${id}`),
  deviceContribution: (id: string, days: number, timeBasis?: TimeBasis) =>
    request<Contribution>(`/devices/${id}/contribution${search({ days, timeBasis })}`),
  sessions: (params: Record<string, string | number | undefined>) =>
    request<Page<SessionSummary>>(`/sessions${search(params)}`),
  sessionDetail: (id: string) => request<SessionDetail>(`/sessions/${id}`),
  clips: (params: Record<string, string | number | undefined>) =>
    request<Page<ClipSummary>>(`/clips${search(params)}`),
  clip: (id: string) => request<ClipDetail>(`/clips/${id}`),
  sessionArtifacts: (id: string) =>
    request<SessionArtifactSummary[]>(`/sessions/${id}/artifacts`),
  /** 会话验收报告（docs/136 §4）。404=未验收、503=部署没配 ACCEPTANCE_R2_*，
   *  两个状态都是常态而不是故障，调用方按 ApiError.status 分流展示。 */
  sessionAcceptance: (id: string) => request<AcceptanceReport>(`/sessions/${id}/acceptance`),
  clientEvents: (params: Record<string, string | number | undefined>) =>
    request<Page<ClientEventEntry>>(`/client-events${search(params)}`),
  auditEvents: (params: Record<string, string | number | undefined>) =>
    request<Page<AuditEntry>>(`/audit-events${search(params)}`),

  invites: (status?: "active" | "revoked") =>
    request<{ invites: InviteSummary[] }>(`/invites${search({ status })}`),
  createInvites: (
    body: { count: number; note?: string; expiresAt?: string },
    idempotencyKey: string,
  ) => post<CreatedInvites>("/invites", body, idempotencyKey),
  revokeInvite: (code: string, idempotencyKey: string) =>
    post<{ invite: InviteSummary }>(`/invites/${encodeURIComponent(code)}/revocation`, {}, idempotencyKey),

  // Omit the password under cf_access: the server re-verifies the Access
  // assertion the edge attaches instead, and there is no console password to
  // send. The body stays a JSON object either way so the route's shape check
  // does not have to special-case the mode.
  sudo: (password?: string) =>
    request<{ sudoSeconds: number }>("/auth/sudo", {
      method: "POST",
      body: JSON.stringify(password === undefined ? {} : { password }),
    }),
  catalogVersions: (params: Record<string, string | undefined>) =>
    request<{ versions: CatalogVersion[] }>(`/catalog-versions${search(params)}`),
  catalogReleases: (params: Record<string, string | undefined> = {}) =>
    request<{ releases: CatalogRelease[] }>(`/catalog-releases${search(params)}`),
  createAnnotator: (body: { alias: string }, idempotencyKey: string) =>
    post<{ annotator: AnnotatorSummary }>("/annotators", body, idempotencyKey),

  // Operator-side registration and credential administration (docs/34 §5.2 /
  // §6.5). All sudo-gated server-side except the profile edit. The claimUrl in
  // each response appears exactly once — treat it like a device token.
  registerAnnotator: (body: {
    email: string;
    alias: string;
    note?: string;
    profile?: ProfileInput;
  }) =>
    request<ClaimIssued & { alias: string }>("/annotators/registration", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  inviteAnnotatorCredential: (id: string, email: string) =>
    request<ClaimIssued>(`/annotators/${id}/credential-invite`, {
      method: "POST",
      body: JSON.stringify({ email }),
    }),
  triggerAnnotatorPasswordReset: (id: string) =>
    request<ClaimIssued>(`/annotators/${id}/password-reset`, {
      method: "POST",
      body: JSON.stringify({}),
    }),
  updateAnnotatorProfile: (id: string, profile: ProfileInput) =>
    request<{ profile: AnnotatorProfile | null }>(`/annotators/${id}/profile`, {
      method: "PUT",
      body: JSON.stringify({ profile }),
    }),
  updateAnnotator: (
    id: string,
    body: { alias?: string; status?: "active" | "suspended" },
    idempotencyKey: string,
  ) => patch<{ annotator: AnnotatorSummary }>(`/annotators/${id}`, body, idempotencyKey),
  createDevice: (body: Record<string, unknown>, idempotencyKey: string) =>
    post<DeviceCredential>("/devices", body, idempotencyKey),
  updateDevice: (
    id: string,
    body: { status?: string; annotatorId?: string },
    idempotencyKey: string,
  ) => patch<{ device: DeviceCredential["device"] }>(`/devices/${id}`, body, idempotencyKey),
  rotateDeviceCredential: (id: string, idempotencyKey: string) =>
    post<DeviceCredential>(`/devices/${id}/credential`, {}, idempotencyKey),
  createConsent: (body: Record<string, unknown>, idempotencyKey: string) =>
    post<{ consent: { id: string } }>("/consents", body, idempotencyKey),
  reviewClip: (id: string, result: "passed" | "failed", note: string | undefined, idempotencyKey: string) =>
    post<{ qualityState: string }>(`/clips/${id}/quality`, { result, note }, idempotencyKey),
  // 判定的默认粒度（issue #855 PR 5）：人审的是一场录制。逐 clip 入口保留做例外修正。
  reviewSession: (
    id: string,
    result: "passed" | "failed",
    note: string | undefined,
    idempotencyKey: string,
  ) =>
    post<{
      sessionId: string;
      qualityState: string;
      decidedClipCount: number;
      skippedClipCount: number;
    }>(`/sessions/${id}/quality`, { result, note }, idempotencyKey),

  // Data-review surface (docs/33). The video URL is a bearer token for those
  // bytes — same handling discipline as previewClip: request on explicit
  // action, keep in memory only.
  reviewSources: () => request<{ sources: ReviewSource[] }>("/review/sources"),
  reviewTree: (source: string, prefix: string, cursor?: string) =>
    request<ReviewTreePage>(`/review/tree${search({ source, prefix, cursor })}`),
  reviewObject: (source: string, key: string) =>
    request<ReviewObjectInfo>(`/review/object${search({ source, key })}`),
  reviewVideoUrl: (source: string, key: string) =>
    request<ReviewVideoGrant>("/review/video-url", {
      method: "POST",
      body: JSON.stringify({ source, key }),
    }),
  /** Presign a viz artifact under a source's viz/ prefix (docs/104 §3):
   *  viewer html, composite mp4, mesh glb/ply, fused-cloud npz. Same URL
   *  discipline as reviewVideoUrl — request on explicit action, memory only. */
  reviewVizUrl: (source: string, key: string) =>
    request<ReviewVideoGrant>("/review/viz-url", {
      method: "POST",
      body: JSON.stringify({ source, key }),
    }),
  reviewFrames: (source: string, key: string) =>
    request<ReviewFramesResult>(`/review/frames${search({ source, key })}`),
  reviewMeta: (source: string, key: string) =>
    request<ReviewMetaResult>(`/review/meta${search({ source, key })}`),
  /** Raw bytes of a `.f32` depth frame or of the `depth-index.jsonl` beside it
   *  (docs/77). Every call is a byte exit and is audited server-side. */
  reviewDepthObject: (source: string, key: string) =>
    requestBytes(`/review/depth-object${search({ source, key })}`),
  reviewVerdicts: (source: string, subjectKey: string) =>
    request<{ verdicts: ReviewVerdict[] }>(`/review/verdicts${search({ source, subjectKey })}`),
  hfRepos: () => request<{ repos: string[] }>("/hf/repos"),
  hfStatus: (source: string, subjectKey: string, videoSizeBytes?: number) =>
    request<HfSubjectStatus>(`/hf/status${search({ source, subjectKey, videoSizeBytes })}`),
  hfReconcile: (repoId: string) =>
    request<HfReconcileResult>("/hf/reconcile", { method: "POST", body: JSON.stringify({ repoId }) }),
  hfMark: (body: { source: string; subjectKey: string; mark: string; note?: string }) =>
    request<{ mark: HfSubjectStatus["manualMark"] }>("/hf/marks", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  recordReviewVerdict: (
    body: {
      source: string;
      subjectKind: ReviewVerdict["subjectKind"];
      subjectKey: string;
      result: "passed" | "failed";
      note?: string;
    },
    idempotencyKey: string,
  ) => post<{ verdict: ReviewVerdict }>("/review/verdicts", body, idempotencyKey),
  purgeClip: (id: string, reason: string, idempotencyKey: string) =>
    post<{ retentionState: string; enqueued: boolean }>(`/clips/${id}/purge`, { reason }, idempotencyKey),
};

/** `/app/v1/session` 的身份摘要（docs/65 J-45）。`annotatorId` 为兼容保留。 */
export type AppSessionSummary = {
  annotatorId: string;
  alias: string | null;
  email: string | null;
  status: string;
};

/** 本人的 consent 回执（docs/65 J-44）。 */
export type AppConsentReceipt = {
  consentId: string;
  policyVersion: string;
  purpose: string;
  termsHash: string;
  status: string;
  allowsTrainableUse: boolean;
  scopes: string[];
  acceptedAt: string | null;
  expiresAt: string | null;
  revokedAt: string | null;
  supersededAt: string | null;
  createdAt: string;
};

/** 「我能不能开始采集」五盏灯（docs/65 J-14）。 */
export type AppReadiness = {
  steps: { key: string; ok: boolean; detail: string | null }[];
};

/**
 * The annotator's own surface (docs/34): registration, sign-in, account
 * self-service. Distinct from `api` because the base path, the CSRF header,
 * and the session cookie are all different — see `appRequest`.
 */
export const appApi = {
  terms: () => appRequest<AppTerms>("/terms"),
  signup: (body: {
    inviteCode: string;
    email: string;
    password: string;
    alias: string;
    acceptedPolicyVersion: string;
    profile?: ProfileInput;
  }) => appPost<{ challengeId: string; expiresInSeconds: number }>("/signup", body),
  verifySignup: (challengeId: string, code: string) =>
    appPost<void>("/signup/verify", { challengeId, code }),
  resendSignup: (challengeId: string) =>
    appPost<{ challengeId: string; expiresInSeconds: number }>("/signup/resend", { challengeId }),
  login: (email: string, password: string) =>
    appPost<{ expiresInSeconds: number }>("/web/login", { email, password }),
  logout: () => appPost<void>("/web/logout", {}),
  session: () => appRequest<AppSessionSummary>("/session"),
  contribution: (days: number) => appRequest<Contribution>(`/contribution?days=${days}`),
  consents: () => appRequest<{ consents: AppConsentReceipt[] }>("/consents"),
  consentState: () => appRequest<AppConsentState>("/consent"),
  renewConsent: (acceptedPolicyVersion: string) =>
    appPost<{ consentId: string | null; renewed: boolean }>("/consent", { acceptedPolicyVersion }),
  readiness: () => appRequest<AppReadiness>("/readiness"),
  profile: () => appRequest<{ profile: AnnotatorProfile | null }>("/profile"),
  updateProfile: (profile: ProfileInput) =>
    appRequest<{ profile: AnnotatorProfile | null }>("/profile", {
      method: "PUT",
      body: JSON.stringify(profile),
    }),
  changePassword: (currentPassword: string, newPassword: string) =>
    appPost<void>("/password", { currentPassword, newPassword }),
  requestPasswordReset: (email: string) =>
    appPost<{ challengeId: string; expiresInSeconds: number }>("/password-reset/request", { email }),
  confirmPasswordReset: (challengeId: string, code: string, password: string) =>
    appPost<void>("/password-reset/confirm", { challengeId, code, password }),
  devices: () => appRequest<{ devices: MyDevice[] }>("/devices"),
  claimPreflight: (challengeId: string) =>
    appRequest<ClaimPreflight>(`/claim/${encodeURIComponent(challengeId)}`),
  confirmClaim: (body: {
    challengeId: string;
    token: string;
    password: string;
    acceptedPolicyVersion?: string;
  }) => appPost<void>("/claim/confirm", body),
};

/**
 * Turns an API failure into something a Chinese-language console can show.
 *
 * The server's `message` is English prose written for developers, and every
 * error surface here fell back to printing it verbatim — so an operator would
 * read "this action requires the governance.write capability" in the middle of a
 * Chinese UI. Codes are stable and are the thing worth translating; anything
 * unmapped still falls back to the raw message rather than hiding the failure.
 */
export function describeApiError(thrown: unknown, fallback = "操作失败，请重试。"): string {
  if (!(thrown instanceof ApiError)) return fallback;
  switch (thrown.code) {
    case "session_required":
      return "登录状态已失效，请重新登录。";
    case "insufficient_role":
      return "当前角色没有执行该操作的权限。";
    case "sudo_required":
      return "这是高危操作，请先重新确认身份。";
    case "invalid_credentials":
      return "无法验证，请检查后重试。";
    case "rate_limited": {
      const wait = thrown.retryAfterSeconds ? `，请在 ${thrown.retryAfterSeconds} 秒后重试` : "";
      return `尝试过于频繁${wait}。`;
    }
    case "recipient_not_allowed":
      return "该环境只向白名单收件人投递邮件。";
    case "not_found":
      return "找不到该资源。";
    case "resource_identity_conflict":
      return "该标识已被占用。";
    case "operator_exists":
      return "该邮箱已有管理员账号。";
    case "operator_role_mismatch":
      return "该邮箱已有一份待接受的邀请，且角色不同；请先改角色再重发。";
    case "policy_blocked":
      return "该操作会让数据变为可训练，已被合规策略拒绝。";
    case "invalid_request":
      return "请求内容不合法，请检查填写的字段。";
    case "dependency_unavailable":
      return "后端依赖暂时不可用，请稍后重试。";
    case "review_not_configured":
      return "该部署没有配置数据审查只读凭据，审查面不可用。";
    case "review_source_not_found":
      return "没有配置这个数据源。";
    case "key_outside_allowlist":
      return "请求的路径在该数据源的白名单前缀之外。";
    case "invalid_object_key":
      return "对象路径不合法。";
    case "object_not_found":
      return "桶里没有该对象。";
    case "object_too_large":
      return "对象超过内联渲染上限。";
    case "integrity_mismatch":
      return "对象字节与旁边 sha256.json 的声明不符，已拒绝渲染——这本身是一个审查发现。";
    case "frame_count_mismatch":
      return "行数与 meta.json 声明的帧数不符，已拒绝渲染——这本身是一个审查发现。";
    case "unsupported_review_object":
      return "该对象类型不支持逐帧渲染。";
    case "hf_not_configured":
      return "该部署没有配置 HF 对账（CONSOLE_HF_*），功能不可用。";
    case "hf_repo_not_allowlisted":
      return "该 repo 不在 CONSOLE_HF_REPOS 白名单内。";
    case "hf_unauthorized":
      return "HF token 被拒——检查其对该 repo 的读权限。";
    case "hf_repo_not_found":
      return "HF 上没有该 repo（或 revision 不存在）。";
    case "hf_unavailable":
      return "HF Hub 暂时不可用，请稍后重试。";
    case "invite_invalid":
      return "邀请码无效或已被使用。";
    case "terms_out_of_date":
      return "页面上的条款已过期，请刷新后重新阅读并勾选。";
    case "weak_password":
      return "密码不能包含你的邮箱地址。";
    case "email_in_use":
      return "该邮箱已有采集员账号。";
    case "email_mismatch":
      return "填写的邮箱与该账号登记的邮箱不一致。";
    case "credential_active":
      return "该账号已激活，请改用「触发密码重置」。";
    case "credential_not_active":
      return "该账号尚未激活，请改用「补发设密链接」。";
    case "credential_suspended":
      return "该账号已被停用。";
    case "annotator_inactive":
      return "该采集员当前不是有效身份。";
    case "annotator_tombstoned":
      return "该采集员已注销，资料不可再编辑。";
    case "claim_not_found":
      return "链接无效、已过期或已被使用，请联系运营重新发放。";
    default:
      return thrown.message;
  }
}

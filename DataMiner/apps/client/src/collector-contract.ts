export type CollectorState =
  | "idle"
  | "discovering"
  | "bound"
  | "preflighting"
  | "starting"
  | "recording"
  | "pausing"
  | "paused"
  | "stopping"
  | "completed"
  | "faulted";

export type GameSummary = {
  gameId: string;
  displayName: string;
  recordingProfileId: string;
  recordingProfileVersion: string;
  recordingProfileHash: string;
  deviceProfileId: string;
  mappingId: string;
  mappingVersion: string;
  width: number;
  height: number;
  fpsNum: number;
  fpsDen: number;
  nonTrainable: boolean;
  sessionContract: SessionContract;
  admission: "admitted" | "onboarding" | "unknown";
  admissionReason: string | null;
};

/**
 * 这场采集按哪一代摄取契约落盘与上传（issue #855 §B）。
 *
 * 断代前这里是 `CatalogMode = "dev_non_trainable"`：一个同时表达运行环境、目录渠道
 * 与训练资格的词。三件事现在各有各的答案——渠道由验签的钉死公钥回答、训练资格只由
 * 服务端 `clip_trainability` 回答，剩下的这一件才是这个类型。
 *
 * `dm-local-dev` 是 legacy v1（编译内嵌 / `local-catalog-only` 的永久归宿）；
 * `dm-session-ingest` 是中性 v2。**逐游戏**，不是逐目录：每游戏一份 bundle、
 * 每游戏独立切 current，断代窗口期里两代必然并存。
 */
export type SessionContract = "dm-local-dev" | "dm-session-ingest";

export type CatalogIdentity = {
  catalogId: string;
  catalogVersion: string;
  contentHash: string;
};

/** How the active catalog reached this client (docs/54 §6.2). */
export type CatalogProvenance = "compiled" | "delivered";

export type CatalogStatus = {
  schemaVersion: "dm-catalog-status-4";
  provenance: CatalogProvenance;
  activeCatalogs: CatalogIdentity[];
  entryCounts: {
    devices: number;
    captures: number;
    games: number;
    settings: number;
    mappings: number;
    depthProfiles: number;
    cameraBindings: number;
  };
  result: string;
  activeGeneration: number;
  cachedGeneration: number;
};

export type WindowCandidate = {
  candidateId: string;
  gameId: string;
  displayName: string;
  executableName: string;
  clientWidth: number;
  clientHeight: number;
  dpi: number;
  foreground: boolean;
  nonTrainable: boolean;
};

export type BoundTargetSummary = {
  targetId: string;
  gameId: string;
  displayName: string;
  executableName: string;
  // 实测 build 指纹；受保护进程拒绝观察时为 null，原因见下一字段（docs/55 §3.1）。
  // 缺席不影响录制，只表示这批数据的身份证据较弱、云端拿不到训练资格。
  executableSha256: string | null;
  executableSha256MissingReason: string | null;
  processId: number;
  clientWidth: number;
  clientHeight: number;
  dpi: number;
  recordingProfileId: string;
  recordingProfileVersion: string;
  recordingProfileHash: string;
  deviceProfileId: string;
  deviceProfileVersion: string;
  deviceProfileHash: string;
  gameProfileVersion: string;
  gameProfileHash: string;
  mappingId: string;
  mappingVersion: string;
  mappingHash: string;
  sessionContract: SessionContract;
  catalogId: string;
  catalogVersion: string;
  catalogHash: string;
  nonTrainable: boolean;
};

export type PreflightCheckEvidence = {
  checkId: string;
  result: string;
  facts: unknown;
};

export type PreflightStatus = {
  schemaVersion: "dm-preflight-evidence-1";
  preflightId: string;
  requestId: string;
  phase: string;
  result: string;
  sessionContract: SessionContract;
  sessionId: string | null;
  runId: string | null;
  failureCode: string | null;
  detailsId: string | null;
  evidenceHash: string;
  checks: PreflightCheckEvidence[];
};

export type CaptureStatus = {
  sessionId: string;
  runId: string;
  runSeq: number;
  frameCount: number;
  wgcCallbackCount: number;
  activeSegment: number;
  readySegments: number;
  duplicateFrames: number;
  sourceSuperseded: number;
  captureQueueDepth: number;
  captureQueueDrops: number;
  inputEventCount: number;
  inputQueueDepth: number;
  inputQueueOverflows: number;
  pressedContractKeys: number[];
  mouseButtons: number[];
  elapsedMs: number;
};

export type StorageState = "unknown" | "ready" | "warning" | "critical" | "unavailable";

export type StorageStatus = {
  state: StorageState;
  freeBytes: number | null;
  requiredStartBytes: number;
  reserveBytes: number;
  estimatedRemainingSeconds: number | null;
  startAllowed: boolean;
  stopRequested: boolean;
};

export type CaptureStorageStatus = {
  schemaVersion: "dm-capture-storage-status-1";
  mode: "app_local" | "custom";
  activeLocation: string;
  configuredLocation: string | null;
  restartRequired: boolean;
  canChange: boolean;
  freeBytes: number | null;
};

export type CollectorFault = {
  code: string;
  message: string;
  action: string;
  retryable: boolean;
  detailsId: string;
};

export type CollectorNotification = CollectorFault;

export type DiagnosticExportResult = {
  exportId: string;
  fileName: string;
  sha256: string;
  eventCount: number;
};

export type SessionHistorySummary = {
  sessionId: string;
  state: "active" | "paused" | "completed" | "faulted" | "corrupt";
  gameId: string;
  recordingProfileId: string;
  mappingId: string;
  mappingVersion: string;
  sessionContract: string;
  catalogId: string;
  catalogVersion: string;
  catalogHash: string;
  deviceProfileId: string;
  deviceProfileVersion: string;
  deviceProfileHash: string;
  recordingProfileVersion: string;
  recordingProfileHash: string;
  recordingProfileHashKind: string;
  gameProfileVersion: string;
  gameProfileHash: string;
  mappingHash: string;
  startedAtUtc: string;
  endedAtUtc: string | null;
  stopReason: string | null;
  errorCode: string | null;
  runCount: number;
  segmentCount: number;
  readySegments: number;
  corruptSegments: number;
  quarantinedSegments: number;
  frameCount: number;
  inputEventCount: number;
  markerCount: number;
  videoBytes: number;
  /** Durable manual gate. Queue creation never implies permission to upload. */
  uploadRequested: boolean;
  uploadRequestedAtUtc: string | null;
  uploadJobSegments: number;
  /** Any performance-validation run makes this session permanently non-deliverable. */
  performanceValidationOnly: boolean;
  uploadRequestable: boolean;
  uploadPendingSegments: number;
  uploadRunningSegments: number;
  uploadRetryWaitSegments: number;
  uploadStoredSegments: number;
  uploadedSegments: number;
  uploadVerifiedSegments: number;
  uploadRejectedSegments: number;
  uploadLastErrorCode: string | null;
  uploadLastHttpStatus: number | null;
  uploadLastServerCode: string | null;
  uploadLastServerMessage: string | null;
  uploadLastRequestId: string | null;
  /** completed + every ready segment server-verified: safe to reclaim via the
   * existing staged deletion. Never deletes anything by itself. */
  reclaimable: boolean;
};

export type SessionHistoryRun = {
  runId: string;
  runName: string;
  seq: number;
  state: string;
  frameCount: number | null;
  readySegments: number | null;
  inputEventCount: number | null;
  inputQueueOverflows: number | null;
  stopReason: string | null;
  errorCode: string | null;
  startedAtUtc: string | null;
  endedAtUtc: string | null;
};

export type SessionHistorySegment = {
  runId: string;
  runName: string;
  seq: number;
  state: "active" | "finalizing" | "ready" | "aborted" | "failed" | "corrupt" | "quarantined";
  sessionFrameStart: number;
  frameCount: number | null;
  qpcStart: number | null;
  qpcEnd: number | null;
  videoBytes: number | null;
  eventCount: number | null;
  hashesComplete: boolean;
  audioState: "ready" | "degraded" | "absent" | null;
  audioFailureCode: string | null;
  audioBytes: number | null;
  errorCode: string | null;
  readyAtUtc: string | null;
  /** 该分段 upload job 的状态；尚未入队为 null。 */
  uploadState: "pending" | "running" | "retry_wait" | "stored" | "verified" | "rejected" | "purged" | null;
  /** 最近一次上传失败/拒收的稳定错误码（docs/65 J-29）。 */
  uploadErrorCode: string | null;
  uploadHttpStatus: number | null;
  uploadServerCode: string | null;
  uploadServerMessage: string | null;
  uploadRequestId: string | null;
  uploadAttemptCount: number | null;
  uploadNextAttemptAtMs: number | null;
  uploadedPartCount: number;
  uploadedBytes: number;
};

export type SessionHistoryMarker = {
  markerId: string;
  runId: string;
  runName: string;
  seq: number;
  kind: "manual";
  qpc: number;
  qpcFrequency: number;
  runFrame: number;
  sessionFrame: number;
  videoTimeUs: number;
  alignmentState: "pending" | "aligned" | "outside_video";
  segmentSeq: number | null;
  createdAtUtc: string;
};

export type SessionHistoryDetail = {
  session: SessionHistorySummary;
  runs: SessionHistoryRun[];
  segments: SessionHistorySegment[];
  markers: SessionHistoryMarker[];
  preflights: PreflightStatus[];
};

export type SessionDeletionResult = {
  sessionId: string;
  deletionId: string;
  artifactsRemoved: boolean;
};

export type DataModelJobStatus = {
  sessionId: string;
  state: "waiting" | "pending" | "running" | "finalizing" | "completed" | "failed" | "cancelled";
  readySegments: number;
  processedSegments: number;
  publishedChunks: number;
  publishedFrames: number;
  pendingFrames: number;
  finalizeRequested: boolean;
  outputRelativePath: string | null;
  errorCode: string | null;
};

/** 上传面（docs/65 J-29/J-30）。uploader worker 每轮推一次；`uploaderRunning`
 * 默认 false——上传器启动失败时没人会把它翻成 true，这就是降级徽章的判据。 */
export type UploadStatus = {
  uploaderRunning: boolean;
  credentialsMissing: boolean;
  pendingSegments: number;
  inflightSegments: number;
  awaitingVerificationSegments: number;
  rejectedSegments: number;
  pendingSessionArtifacts: number;
  lastErrorCode: string | null;
  nextRetryUnixMs: number | null;
};

export type CollectorSnapshot = {
  schemaVersion: "dm-collector-snapshot-5";
  revision: number;
  state: CollectorState;
  // 车道分类随目录渠道/绑定目标派生（docs/65 J-26），不再是字面量常量。
  nonTrainable: boolean;
  productionCertified: boolean;
  catalog: CatalogStatus;
  preflight: PreflightStatus | null;
  boundTarget: BoundTargetSummary | null;
  capture: CaptureStatus | null;
  storage: StorageStatus;
  rebindRequired: boolean;
  rebindGameId: string | null;
  fault: CollectorFault | null;
  upload: UploadStatus;
};

export type OverlaySnapshot = {
  recordingState: CollectorState;
  // 当前会话的车道分类（docs/65 J-26）。只剩一条车道后恒 true（docs/100）。
  nonTrainable: boolean;
  productionCertified: boolean;
  elapsedMs: number;
  segmentSeq: number;
  captureHealth: "healthy" | "fault";
  storageState: StorageState;
  storageFreeBytes: number | null;
  storageRemainingSeconds: number | null;
  storageStopRequested: boolean;
  pressedContractKeys: number[];
  mouseButtons: number[];
};

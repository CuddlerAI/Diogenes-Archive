import type {
  ArtifactDeclaration,
  ArtifactKind,
  CreateSessionInput,
  PutClipInput,
  PutRunInput,
  PutSessionArtifactsInput,
  SealSessionInput,
  SessionArtifactKind,
  SignPartsInput,
} from "./contracts.js";
import type { AdmissionFamily } from "./environment.js";

export type SessionArtifactRecord = {
  id: string;
  sessionId: string;
  /**
   * 拥有它的 session 的 `schema_version`——v1/v2 并存期的**唯一**存储路由键
   * （issue #855 §B）。选前缀、选 object class、选 key 形状都读它。
   */
  sessionSchemaVersion: string;
  kind: SessionArtifactKind;
  objectKey: string;
  contentType: string;
  expectedBytes: number;
  expectedSha256: string;
  ingestState: "pending" | "uploading" | "stored" | "retry_wait" | "failed";
  verificationState: "unverified" | "queued" | "verifying" | "verified" | "rejected";
  revision: number;
};

export type SessionRecord = {
  id: string;
  deviceId: string;
  localSessionId: string;
  state: "open" | "sealed" | "rejected";
  /**
   * 这一场采集是在哪一版 session 契约下登记的。创建时写一次、之后不可变，
   * run / clip / multipart / session artifact 全链都按它选存储策略
   * （issue #855 §B）；不再按 Catalog 身份或后续请求重新推断车道。
   */
  sessionSchemaVersion: string;
  createdAt: string;
  updatedAt: string;
};

export type RunRecord = {
  id: string;
  sessionId: string;
  localRunId: string;
  sequence: number;
  state: "closed" | "rejected";
  createdAt: string;
  updatedAt: string;
};

export type ArtifactState =
  | "declared"
  | "uploading"
  | "stored"
  | "purging"
  | "purged"
  | "verifying"
  | "verified"
  | "rejected";

export type ClipState =
  | "declared"
  | "uploading"
  | "stored"
  | "queued"
  | "verifying"
  | "verified"
  | "rejected";

export type ArtifactRecord = {
  id: string;
  clipId: string;
  kind: ArtifactKind;
  objectKey: string;
  expectedBytes: number;
  expectedSha256: string;
  contentType: string;
  state: ArtifactState;
  actualBytes?: number;
  actualSha256?: string;
  rejectionCode?: string;
};

export type ClipRecord = {
  id: string;
  sessionId: string;
  runId: string;
  segmentIndex: number;
  revision: number;
  state: ClipState;
  storageState: "pending" | "uploading" | "stored" | "retry_wait" | "failed";
  verificationState: "unverified" | "queued" | "verifying" | "verified" | "rejected";
  retentionState: "retained" | "purge_requested" | "purging" | "purged" | "purge_failed";
  retentionDeadline: string;
  artifacts: ArtifactRecord[];
  isTrainable: boolean;
  trainabilityBlockers: string[];
  createdAt: string;
  updatedAt: string;
};

export type MultipartRecord = {
  id: string;
  uploadIntentId: string;
  artifactId: string;
  clipId: string;
  kind: ArtifactKind;
  objectKey: string;
  contentType: string;
  providerUploadId?: string;
  state: "initiated" | "completed" | "aborted";
  /** Opaque provider-attempt generation exposed to clients; never the provider upload id. */
  attemptNo: number;
  partSizeBytes: number;
  expectedBytes: number;
  createdAt: string;
};

export type MultipartCompletionRecord = MultipartRecord & {
  completionLeaseId: string;
};

export type UploadIntentRecord = {
  id: string;
  clipId: string;
  state: "initiated" | "completed" | "aborted";
  /** 拥有它的 session 的 `schema_version`——选车道的前缀与 object class。 */
  sessionSchemaVersion: string;
  uploads: MultipartRecord[];
  createdAt: string;
};

export type VerificationJob = {
  id: string;
  artifactId: string;
  clipId: string;
  objectKey: string;
  expectedBytes: number;
  expectedSha256: string;
  attempt: number;
  kind: ArtifactKind;
  semantic?:
    | { kind: "manifest"; expectedEntries: Record<string, string> }
    | {
        kind: "meta";
        expectedSubset: Record<string, unknown>;
        /** Top-level keys retired by this session generation and therefore required to be absent. */
        forbiddenTopLevelKeys?: readonly string[];
      };
};

export type PurgeJob = {
  id: string;
  clipId: string;
  attempt: number;
  leaseOwner: string;
  multiparts: ReadonlyArray<{
    id: string;
    objectKey: string;
    providerUploadId: string;
    aborted: boolean;
  }>;
  artifacts: ReadonlyArray<{
    id: string;
    objectKey: string;
    confirmedAbsent: boolean;
  }>;
};

export type PurgeProgress =
  | { kind: "multipart_aborted"; multipartId: string }
  | { kind: "artifact_absent"; artifactId: string };

export type IdempotencyResponse = {
  statusCode: number;
  body: unknown;
  headers?: Record<string, string>;
};

// The gate resolves a bearer token to the device that owns it. `annotatorId` is
// null while a device is still unattached; that is legal for non-trainable devices
// provisioned before the annotator identity exists (11 §6.3).
export type AuthenticatedDevice = {
  id: string;
  annotatorId: string | null;
  credentialFingerprint: string;
};

export type CurrentCatalogRelease = {
  catalogId: string;
  catalogVersion: string;
  envelopeBytes: Buffer;
  envelopeSha256: string;
};

export type GameAdmission = {
  gameId: string;
  admission: "admitted" | "onboarding";
  reason: null | "capture_not_allowed";
};

export interface IngestionRepository {
  ping(): Promise<void>;
  authenticateDevice(credentialFingerprint: string): Promise<AuthenticatedDevice | null>;
  /** 该渠道全部已发布的 bundle（docs/57 §3.3：每游戏一份）。空数组=未发布。 */
  currentCatalogRelease(channel: "dev" | "certified"): Promise<CurrentCatalogRelease[]>;
  /** 服务端实时准入事实；缺失的 game_id 由客户端解释为 game_not_registered。 */
  listGameAdmissions(): Promise<GameAdmission[]>;
  getIdempotency(scope: string, key: string): Promise<{
    requestHash: string;
    response?: IdempotencyResponse;
  } | null>;
  reserveIdempotency(scope: string, key: string, requestHash: string): Promise<"reserved" | "exists">;
  finishIdempotency(scope: string, key: string, response: IdempotencyResponse): Promise<void>;
  releaseIdempotency(scope: string, key: string, requestHash: string): Promise<void>;

  createSession(
    deviceId: string,
    input: CreateSessionInput,
    family: AdmissionFamily,
  ): Promise<SessionRecord>;
  getSession(deviceId: string, sessionId: string): Promise<SessionRecord | null>;
  putRun(deviceId: string, sessionId: string, localRunId: string, input: PutRunInput): Promise<RunRecord>;
  putClip(
    deviceId: string,
    sessionId: string,
    localRunId: string,
    segmentIndex: number,
    input: PutClipInput,
    objectPrefix: string,
    bucket: string,
    /**
     * The clip's admitted family. It must be the family that owns the parent
     * session's `schema_version` (a cross-lane clip is refused), and its audio
     * pairing decides which session schema generation the clip may attach to.
     */
    family: AdmissionFamily,
  ): Promise<ClipRecord>;
  getClip(deviceId: string, clipId: string): Promise<ClipRecord | null>;
  requestClipPurge(deviceId: string, clipId: string): Promise<void>;
  sealSession(deviceId: string, sessionId: string, input: SealSessionInput): Promise<SessionRecord>;

  // docs/47 · session-root artifacts (single presigned PUT, server-verified).
  declareSessionArtifacts(
    deviceId: string,
    sessionId: string,
    input: PutSessionArtifactsInput,
    objectPrefix: string,
    /** 车道的会话快照 key 形状（issue #855 §B）；legacy 与 raw/v2 不同。 */
    keyStyle: AdmissionFamily["sessionArtifactKeyStyle"],
  ): Promise<SessionArtifactRecord[]>;
  getSessionArtifacts(deviceId: string, sessionId: string): Promise<SessionArtifactRecord[]>;
  getSessionArtifact(
    deviceId: string,
    sessionId: string,
    kind: SessionArtifactKind,
  ): Promise<SessionArtifactRecord | null>;
  finalizeSessionArtifact(
    deviceId: string,
    sessionId: string,
    kind: SessionArtifactKind,
    outcome:
      | { verified: true; actualBytes: number; actualSha256: string }
      | { verified: false; rejectionCode: string },
  ): Promise<SessionArtifactRecord>;

  assertUploadAllowed(deviceId: string, clipId: string): Promise<void>;
  beginUploadIntent(
    deviceId: string,
    clipId: string,
    uploads: ReadonlyArray<{
      artifactId: string;
      kind: ArtifactKind;
      partSizeBytes: number;
    }>,
  ): Promise<UploadIntentRecord>;
  bindMultipartProviderUpload(
    deviceId: string,
    multipartId: string,
    providerUploadId: string,
  ): Promise<void>;
  findUploadIntentForClip(deviceId: string, clipId: string): Promise<UploadIntentRecord | null>;
  getUploadIntent(deviceId: string, uploadIntentId: string): Promise<UploadIntentRecord | null>;
  recordSignedParts(
    deviceId: string,
    uploadIntentId: string,
    parts: SignPartsInput["parts"],
    minimumValiditySeconds: number,
  ): Promise<void>;
  beginMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseSeconds: number,
  ): Promise<MultipartCompletionRecord>;
  releaseMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    errorCode: string,
  ): Promise<boolean>;
  restartMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    providerUploadId: string,
  ): Promise<MultipartRecord>;
  failMultipart(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    errorCode: string,
  ): Promise<boolean>;
  completeMultipart(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    actualBytes: number,
    parts: ReadonlyArray<{ partNumber: number; etag: string; bytes: number }>,
  ): Promise<MultipartRecord>;

  leaseVerificationJobs(limit: number, leaseSeconds: number): Promise<VerificationJob[]>;
  finishVerification(
    job: VerificationJob,
    result:
      | { outcome: "verified"; actualBytes: number; actualSha256: string }
      | { outcome: "rejected"; actualBytes: number; actualSha256: string; rejectionCode: string },
  ): Promise<void>;
  /**
   * 返回本次尝试的去向，好让调用方能把「永久判死」单独记一笔——
   * 第 8 次失败与前 7 次在日志里长得一模一样，是 #701 的观测缺口之一。
   * `stale` 表示这次租约已经不属于本 worker，什么都没改。
   */
  retryVerification(
    job: VerificationJob,
    errorCode: string,
  ): Promise<"retry_scheduled" | "dead_letter" | "stale">;

  enqueueMaintenancePurges(limit: number): Promise<number>;
  /** Telemetry with no consent to purge it (11 §6.4). Returns rows tombstoned. */
  sweepUnconsentedTelemetry(ttlDays: number, limit: number): Promise<number>;
  leasePurgeJobs(
    limit: number,
    leaseSeconds: number,
    completionLeaseSeconds: number,
  ): Promise<PurgeJob[]>;
  renewPurgeLease(job: PurgeJob): Promise<void>;
  recordPurgeProgress(job: PurgeJob, progress: PurgeProgress): Promise<void>;
  finishPurge(job: PurgeJob): Promise<void>;
  retryPurge(job: PurgeJob, errorCode: string): Promise<void>;
  isKnownActiveMultipart(providerUploadId: string, objectKey: string): Promise<boolean>;
  recordOrphanMultipartAborted(objectKey: string): Promise<void>;
}

export function artifactForKind(clip: ClipRecord, kind: ArtifactKind): ArtifactRecord {
  const artifact = clip.artifacts.find((candidate) => candidate.kind === kind);
  if (!artifact) {
    throw new Error(`clip ${clip.id} has no ${kind} artifact`);
  }
  return artifact;
}

export const artifactContentTypes: Record<ArtifactKind, string> = {
  video: "video/mp4",
  frames: "application/zstd",
  events: "application/zstd",
  meta: "application/json",
  manifest: "application/json",
  audio: "application/zstd",
  depth: "application/zip",
};

export function artifactInput(input: PutClipInput, kind: ArtifactKind): ArtifactDeclaration | undefined {
  return input.artifacts[kind];
}

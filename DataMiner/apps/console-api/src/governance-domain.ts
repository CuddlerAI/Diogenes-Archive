import type {
  CreateAnnotatorInput,
  CreateConsentInput,
  CreateDeviceInput,
  ReviewVerdictInput,
  UpdateAnnotatorInput,
  UpdateDeviceInput,
} from "./governance-contracts.js";

export type WrittenAnnotator = {
  id: string;
  alias: string | null;
  status: string;
  createdAt: string;
};

export type WrittenConsent = {
  id: string;
  annotatorId: string;
  policyVersion: string;
  status: string;
  allowsTrainableUse: boolean;
  acceptedAt: string | null;
  expiresAt: string | null;
};

export type WrittenDevice = {
  id: string;
  clientDeviceId: string;
  annotatorId: string | null;
  status: string;
  credentialFingerprint: string;
};


// `status` and `approvedForTrainable` are orthogonal, not two names for one
// thing: an approved row may still carry approvedForTrainable = false. They are
// carried — and rendered — separately on purpose.
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

export type QualityOutcome = {
  clipId: string;
  qualityState: string;
  reviewedBy: string;
  reviewedAt: string;
};

/**
 * 一次 session 级 QA 判定的结果（issue #855 PR 5）。
 *
 * 人实际审的是**一场录制**，不是一条 10 秒片段：按 clip 判是万级点击，按 session
 * 判是百级。所以判定入口有两个粒度，写的仍然是同一列 `clips.quality_state`，走的
 * 仍然是同一条审计路径，记的仍然是那位 operator——`quality_reviewed_by` 永远是人，
 * 不存在"系统"这个作者。
 *
 * `decidedClipCount` / `skippedClipCount` 是**这次判定的账**：跳过的是尚未通过传输
 * 验证的 clip，它们照旧留在 `pending`，与逐 clip 入口的 WHERE 条件逐字一致。
 */
export type SessionQualityOutcome = {
  sessionId: string;
  qualityState: "passed" | "failed";
  reviewedBy: string;
  reviewedAt: string;
  /** 本次判定实际落到的 clip 数（`verification_state = 'verified'`）。 */
  decidedClipCount: number;
  /** 尚未通过传输验证、因此照旧留 `pending` 的 clip 数。 */
  skippedClipCount: number;
};

export type PurgeOutcome = {
  clipId: string;
  retentionState: string;
  enqueued: boolean;
};

// A human review conclusion over a bucket subject (docs/33 §5). Append-only:
// the latest row per (source, subjectKey) is the effective verdict, and it
// deliberately does NOT touch clips.quality_state or trainability.
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

export type IdempotentReplay = {
  status: "replayed";
  body: unknown;
};

export interface GovernanceRepository {
  createAnnotator(input: CreateAnnotatorInput): Promise<WrittenAnnotator>;
  updateAnnotator(id: string, input: UpdateAnnotatorInput): Promise<WrittenAnnotator>;
  createConsent(input: CreateConsentInput): Promise<WrittenConsent>;
  revokeConsent(id: string, reason: string): Promise<WrittenConsent>;
  createDevice(input: CreateDeviceInput, credentialFingerprint: string): Promise<WrittenDevice>;
  updateDevice(id: string, input: UpdateDeviceInput): Promise<WrittenDevice>;
  rotateDeviceCredential(id: string, credentialFingerprint: string): Promise<WrittenDevice>;
  recordClipQuality(clipId: string, result: "passed" | "failed", reviewedBy: string): Promise<QualityOutcome>;
  recordSessionQuality(
    sessionId: string,
    result: "passed" | "failed",
    reviewedBy: string,
  ): Promise<SessionQualityOutcome>;
  requestClipPurge(clipId: string, reason: string, operatorId: string): Promise<PurgeOutcome>;
  recordReviewVerdict(input: ReviewVerdictInput, operatorId: string, requestId: string | null): Promise<ReviewVerdict>;
  listReviewVerdicts(source: string, subjectKey: string, limit: number): Promise<ReviewVerdict[]>;
  listCatalogVersions(kind?: string, gameId?: string): Promise<CatalogVersion[]>;
  listCatalogReleases(channel?: string): Promise<CatalogRelease[]>;

  beginIdempotent(scope: string, key: string, requestHash: string): Promise<IdempotentReplay | null>;
  finishIdempotent(scope: string, key: string, body: unknown): Promise<void>;
  releaseIdempotent(scope: string, key: string): Promise<void>;
}

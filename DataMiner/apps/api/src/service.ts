import { createHash } from "node:crypto";

import type { AdmittedFamily } from "./config.js";
import type {
  CompleteUploadIntentInput,
  CreateSessionInput,
  CreateUploadIntentInput,
  PutClipInput,
  PutRunInput,
  PutSessionArtifactsInput,
  SealSessionInput,
  SessionArtifactKind,
  SignPartsInput,
} from "./contracts.js";
import { artifactKinds, requiredSessionArtifactKinds } from "./contracts.js";
import {
  artifactForKind,
  type IngestionRepository,
  type MultipartRecord,
  type SessionArtifactRecord,
} from "./domain.js";
import type { AdmissionFamily } from "./environment.js";
import {
  ApiError,
  badRequest,
  conflict,
  dependencyUnavailable,
  notFound,
  retryableConflict,
} from "./errors.js";
import {
  isNoSuchUploadError,
  storageErrorDiagnostics,
  type ObjectStorage,
  type UploadPart,
} from "./storage.js";

/**
 * session artifact 申报集合的裁决（docs/47 + docs/46 §5.2）：六件必需快照必须一次性
 * 申报齐，`markers` / `input_calibration` 可选。
 *
 * 断代前这里还有第二条分支——"可训练车道只收 `input_calibration` 一件"——服务于
 * 那条 consented first-party 车道：它的快照由 catalog 注册通道承载，所以摄取面
 * 上的快照申报是禁令。那条车道随 docs/57 §3.3 退役，分支从此没有主人，而它的
 * 判定条件读的正是 `admission.nonTrainable`——issue #855 §B 要删的训练声明字段之一
 * 被当成了车道路由键。中性摄取车道与 legacy 一样自带六件快照（`raw/v2/<deployment>/
 * sessions/<id>/session/...`），所以这里收敛成一条规则，而不是给新车道再配一个
 * 只有一种取值的开关。
 *
 * 导出为纯函数以便直接测试。
 */
export function sessionArtifactSetDecision(
  kinds: readonly SessionArtifactKind[],
): "ok" | "incomplete" {
  return requiredSessionArtifactKinds.every((required) => kinds.includes(required))
    ? "ok"
    : "incomplete";
}

const FIVE_MIB = 5 * 1024 * 1024;
const PRESIGN_SAFETY_MARGIN_SECONDS = 5;

export class IngestionService {
  /**
   * `sessions.schema_version` → 已准入车道。
   *
   * 这是 issue #855 §B 的路由表本身：删掉 `catalogMode` 之后 lane 不能靠猜，也不能
   * 再换一个环境词续命，唯一的键是服务端已经校验并持久化的 session 契约版本。
   * legacy 家族的两个音频世代（`-2` / `-3`）都指向同一条车道——世代不是车道。
   */
  private readonly lanesBySessionSchema: Map<string, AdmittedFamily>;

  constructor(
    private readonly repository: IngestionRepository,
    private readonly storage: ObjectStorage,
    private readonly bucket: string,
    admittedFamilies: readonly AdmittedFamily[],
    private readonly partSizeBytes: number,
    private readonly presignTtlSeconds: number,
    private readonly multipartCompletionLeaseSeconds: number,
  ) {
    this.lanesBySessionSchema = new Map(
      admittedFamilies.flatMap((entry) =>
        [entry.family.sessionSchemaVersion, entry.family.audio?.sessionSchemaVersion]
          .filter((version): version is string => version !== undefined)
          .map((version) => [version, entry] as const),
      ),
    );
  }

  /**
   * The lane a stored session belongs to. Refusing here is deliberate: a session
   * admitted while a lane was configured must not fall back to another lane's
   * prefix or object class after a deployment change — its uploads stall loudly
   * instead of landing under the wrong key space.
   */
  private laneFor(sessionSchemaVersion: string): AdmittedFamily {
    const lane = this.lanesBySessionSchema.get(sessionSchemaVersion);
    if (!lane) {
      throw conflict(
        "lane_not_admitted",
        "this deployment no longer admits the session's artifact lane",
      );
    }
    return lane;
  }

  /** 已准入家族本身的车道，按同一张表查——同一个"现在还准入吗"的答案。 */
  private laneForFamily(family: AdmissionFamily): AdmittedFamily {
    return this.laneFor(family.sessionSchemaVersion);
  }

  private sessionArtifactLaneFor(
    sessionSchemaVersion: string,
    kinds?: readonly SessionArtifactKind[],
  ): AdmittedFamily {
    const lane = this.laneFor(sessionSchemaVersion);
    if (kinds !== undefined && sessionArtifactSetDecision(kinds) === "incomplete") {
      throw conflict(
        "session_artifact_set_incomplete",
        "a session must declare all six required session snapshots at once",
      );
    }
    return lane;
  }

  createSession(deviceId: string, input: CreateSessionInput, family: AdmissionFamily) {
    return this.repository.createSession(deviceId, input, family);
  }

  putRun(deviceId: string, sessionId: string, localRunId: string, input: PutRunInput) {
    return this.repository.putRun(deviceId, sessionId, localRunId, input);
  }

  putClip(
    deviceId: string,
    sessionId: string,
    localRunId: string,
    segmentIndex: number,
    input: PutClipInput,
    family: AdmissionFamily,
  ) {
    const lane = this.laneForFamily(family);
    return this.repository.putClip(
      deviceId,
      sessionId,
      localRunId,
      segmentIndex,
      input,
      lane.prefix,
      this.bucket,
      family,
    );
  }

  async createUploadIntent(deviceId: string, clipId: string, input: CreateUploadIntentInput) {
    const clip = await this.repository.getClip(deviceId, clipId);
    if (!clip) {
      throw notFound("clip_not_found", "clip was not found");
    }
    if (clip.revision !== input.clipRevision) {
      throw conflict("clip_revision_mismatch", "clipRevision does not match the immutable declaration");
    }
    const existing = await this.repository.findUploadIntentForClip(deviceId, clipId);
    if (existing) {
      return uploadIntentResponse(await this.ensureProviderUploads(deviceId, clip, existing));
    }
    if (clip.artifacts.some((artifact) => artifact.state !== "declared")) {
      throw conflict("clip_upload_started", "an upload intent already exists or the clip is immutable");
    }
    await this.repository.assertUploadAllowed(deviceId, clipId);
    const intent = await this.repository.beginUploadIntent(
      deviceId,
      clipId,
      clip.artifacts.map((artifact) => {
        return {
          artifactId: artifact.id,
          kind: artifact.kind,
          partSizeBytes: partSizeFor(artifact.expectedBytes, this.partSizeBytes),
        };
      }),
    );
    return uploadIntentResponse(await this.ensureProviderUploads(deviceId, clip, intent));
  }

  async signParts(deviceId: string, uploadIntentId: string, input: SignPartsInput) {
    const intent = await this.repository.getUploadIntent(deviceId, uploadIntentId);
    if (!intent) {
      throw notFound("upload_intent_not_found", "upload intent was not found");
    }
    if (intent.state !== "initiated") {
      throw conflict("upload_intent_not_active", "upload intent is no longer active");
    }
    const uploads = new Map(intent.uploads.map((upload) => [upload.kind, upload]));
    for (const part of input.parts) {
      const upload = uploads.get(part.artifactType);
      if (!upload || upload.state !== "initiated") {
        throw conflict("artifact_upload_not_active", `${part.artifactType} upload is no longer active`);
      }
      validatePartSize(upload.expectedBytes, upload.partSizeBytes, part.partNumber, part.byteSize);
    }
    await this.repository.recordSignedParts(
      deviceId,
      uploadIntentId,
      input.parts,
      this.presignTtlSeconds + PRESIGN_SAFETY_MARGIN_SECONDS,
    );
    const signed = await Promise.all(
      input.parts.map(async (part) => {
      const upload = uploads.get(part.artifactType)!;
      const providerUploadId = requiredProviderUploadId(upload);
      return {
          artifactType: part.artifactType,
          partNumber: part.partNumber,
          ...(await this.storage.signUploadPart({
          objectKey: upload.objectKey,
          providerUploadId,
            partNumber: part.partNumber,
            bytes: part.byteSize,
            contentMd5: part.contentMd5,
          })),
        };
      }),
    );
    return { uploadIntentId, parts: signed };
  }

  async completeUploadIntent(
    deviceId: string,
    uploadIntentId: string,
    input: CompleteUploadIntentInput,
  ) {
    let intent = await this.repository.getUploadIntent(deviceId, uploadIntentId);
    if (!intent) {
      throw notFound("upload_intent_not_found", "upload intent was not found");
    }
    if (intent.state === "completed") {
      return { uploadIntentId, state: "stored" as const };
    }
    if (intent.state !== "initiated") {
      throw conflict("upload_intent_not_active", "upload intent is no longer active");
    }
    const objectClass = this.laneFor(intent.sessionSchemaVersion).family.objectClass;
    const completionKinds = artifactKinds.filter((kind) => input.artifacts[kind] !== undefined);
    const intentKinds = new Set(intent.uploads.map((upload) => upload.kind));
    if (
      completionKinds.length !== intentKinds.size ||
      completionKinds.some((kind) => !intentKinds.has(kind))
    ) {
      throw conflict("upload_plan_mismatch", "completion evidence must match every declared artifact");
    }

    for (const { kind } of intent.uploads) {
      const upload = requiredUpload(intent.uploads, kind);
      const completionArtifact = input.artifacts[kind];
      if (!completionArtifact) {
        throw conflict("upload_plan_mismatch", `completion evidence is missing ${kind}`);
      }
      if (upload.state === "completed") {
        continue;
      }
      if (await this.storage.objectExists(upload.objectKey)) {
        const completion = await this.repository.beginMultipartCompletion(
          deviceId,
          upload.id,
          this.multipartCompletionLeaseSeconds,
        );
        await this.adoptStoredMultipartWithLease(
          deviceId,
          upload,
          completion.completionLeaseId,
          kind,
          partsWithExpectedBytes(upload, completionArtifact.parts),
          objectClass,
        );
        continue;
      }
      const completion = await this.repository.beginMultipartCompletion(
        deviceId,
        upload.id,
        this.multipartCompletionLeaseSeconds,
      );
      await this.completeActiveMultipart(
        deviceId,
        upload,
        completion.completionLeaseId,
        kind,
        completionArtifact.parts,
        objectClass,
      );
    }
    intent = await this.repository.getUploadIntent(deviceId, uploadIntentId);
    if (!intent || intent.state !== "completed") {
      throw conflict("upload_intent_incomplete", "not all artifacts reached stored state");
    }
    return { uploadIntentId, state: "stored" as const };
  }

  private async completeActiveMultipart(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    requestedParts: ReadonlyArray<{ partNumber: number; etag: string }>,
    objectClass: string,
  ): Promise<void> {
    const providerUploadId = requiredProviderUploadId(upload);
    let listed: UploadPart[];
    try {
      listed = await this.storage.listParts(upload.objectKey, providerUploadId);
    } catch (error) {
      if (isNoSuchUploadError(error)) {
        await this.restartMissingMultipart(
          deviceId,
          upload,
          completionLeaseId,
          kind,
          requestedParts,
          objectClass,
          error,
        );
        return;
      }
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "provider_list_parts_failed",
      );
      throw error;
    }

    let authoritative: UploadPart[];
    try {
      authoritative = reconcileParts(
        upload.expectedBytes,
        upload.partSizeBytes,
        requestedParts,
        listed,
      );
    } catch (error) {
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "multipart_evidence_incomplete",
      );
      if (
        upload.attemptNo > 1 &&
        error instanceof ApiError &&
        error.code === "multipart_incomplete"
      ) {
        throw retryableConflict(
          "multipart_upload_restarted",
          `${kind} provider upload was restarted; retry with the refreshed upload plan`,
          completionDiagnostics(kind, "list_parts", error),
        );
      }
      throw error;
    }

    try {
      await this.storage.completeMultipart(upload.objectKey, providerUploadId, authoritative);
    } catch (error) {
      await this.reconcileFailedProviderCompletion(
        deviceId,
        upload,
        completionLeaseId,
        kind,
        authoritative,
        objectClass,
        error,
      );
      return;
    }

    await this.adoptStoredMultipartWithLease(
      deviceId,
      upload,
      completionLeaseId,
      kind,
      authoritativeParts(upload, authoritative),
      objectClass,
    );
  }

  private async reconcileFailedProviderCompletion(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    authoritative: UploadPart[],
    objectClass: string,
    providerError: unknown,
  ): Promise<void> {
    let objectExists: boolean;
    try {
      objectExists = await this.storage.objectExists(upload.objectKey);
    } catch (reconcileError) {
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "provider_completion_reconcile_failed",
      );
      throw dependencyUnavailable(
        "multipart_completion_reconcile_failed",
        `${kind} completion state could not be reconciled`,
        completionDiagnostics(kind, "head_object", reconcileError),
      );
    }
    if (objectExists) {
      await this.adoptStoredMultipartWithLease(
        deviceId,
        upload,
        completionLeaseId,
        kind,
        authoritativeParts(upload, authoritative),
        objectClass,
      );
      return;
    }

    try {
      await this.storage.listParts(upload.objectKey, requiredProviderUploadId(upload));
    } catch (error) {
      if (isNoSuchUploadError(error)) {
        await this.restartMissingMultipart(
          deviceId,
          upload,
          completionLeaseId,
          kind,
          authoritative,
          objectClass,
          providerError,
        );
        return;
      }
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "provider_completion_reconcile_failed",
      );
      throw dependencyUnavailable(
        "multipart_completion_reconcile_failed",
        `${kind} completion state could not be reconciled`,
        completionDiagnostics(kind, "list_parts", error),
      );
    }

    await this.releaseCompletionForRetry(
      deviceId,
      upload,
      completionLeaseId,
      "provider_completion_retryable",
    );
    throw dependencyUnavailable(
      "multipart_completion_retryable",
      `${kind} provider upload remains active after a failed completion`,
      completionDiagnostics(kind, "complete_multipart", providerError),
    );
  }

  private async restartMissingMultipart(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    requestedParts: ReadonlyArray<{ partNumber: number; etag: string }>,
    objectClass: string,
    providerError: unknown,
  ): Promise<void> {
    let objectExists: boolean;
    try {
      objectExists = await this.storage.objectExists(upload.objectKey);
    } catch (error) {
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "provider_restart_head_failed",
      );
      throw dependencyUnavailable(
        "multipart_completion_reconcile_failed",
        `${kind} completion state could not be reconciled`,
        completionDiagnostics(kind, "head_object", error),
      );
    }
    if (objectExists) {
      await this.adoptStoredMultipartWithLease(
        deviceId,
        upload,
        completionLeaseId,
        kind,
        partsWithExpectedBytes(upload, requestedParts),
        objectClass,
      );
      return;
    }

    let replacementProviderUploadId: string;
    try {
      replacementProviderUploadId = await this.storage.createMultipart(
        upload.objectKey,
        upload.contentType,
        objectClass,
      );
    } catch (error) {
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "provider_restart_create_failed",
      );
      throw dependencyUnavailable(
        "multipart_provider_restart_failed",
        `${kind} provider upload could not be restarted`,
        completionDiagnostics(kind, "create_multipart", error),
      );
    }

    try {
      await this.repository.restartMultipartCompletion(
        deviceId,
        upload.id,
        completionLeaseId,
        replacementProviderUploadId,
      );
    } catch (error) {
      await this.storage
        .abortMultipart(upload.objectKey, replacementProviderUploadId)
        .catch(() => undefined);
      throw error;
    }
    throw retryableConflict(
      "multipart_upload_restarted",
      `${kind} provider upload was restarted; retry with the refreshed upload plan`,
      completionDiagnostics(kind, "complete_multipart", providerError),
    );
  }

  private async adoptStoredMultipart(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    parts: ReadonlyArray<{ partNumber: number; etag: string; bytes: number }>,
    objectClass: string,
  ): Promise<void> {
    const head = await this.storage.headObject(upload.objectKey);
    await this.validateStoredObject(
      deviceId,
      upload,
      completionLeaseId,
      kind,
      head,
      objectClass,
    );
    await this.persistCompletedMultipart(
      deviceId,
      upload,
      completionLeaseId,
      head.bytes,
      parts,
    );
  }

  private async adoptStoredMultipartWithLease(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    parts: ReadonlyArray<{ partNumber: number; etag: string; bytes: number }>,
    objectClass: string,
  ): Promise<void> {
    try {
      await this.adoptStoredMultipart(
        deviceId,
        upload,
        completionLeaseId,
        kind,
        parts,
        objectClass,
      );
    } catch (error) {
      await this.releaseCompletionForRetry(
        deviceId,
        upload,
        completionLeaseId,
        "stored_object_reconcile_failed",
      );
      throw error;
    }
  }

  private async releaseCompletionForRetry(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    errorCode: string,
  ): Promise<void> {
    await this.repository
      .releaseMultipartCompletion(deviceId, upload.id, completionLeaseId, errorCode)
      .catch(() => false);
  }

  private async persistCompletedMultipart(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    actualBytes: number,
    parts: ReadonlyArray<{ partNumber: number; etag: string; bytes: number }>,
  ): Promise<void> {
    try {
      await this.repository.completeMultipart(
        deviceId,
        upload.id,
        completionLeaseId,
        actualBytes,
        parts,
      );
    } catch (error) {
      if (isPostStorageGateFailure(error)) {
        await this.repository
          .failMultipart(deviceId, upload.id, completionLeaseId, error.code)
          .catch(() => undefined);
      }
      throw error;
    }
  }

  async getClip(deviceId: string, clipId: string) {
    const clip = await this.repository.getClip(deviceId, clipId);
    if (!clip) {
      throw notFound("clip_not_found", "clip was not found");
    }
    return clip;
  }

  async requestClipPurge(deviceId: string, clipId: string) {
    await this.repository.requestClipPurge(deviceId, clipId);
    return this.getClip(deviceId, clipId);
  }

  private async ensureProviderUploads(
    deviceId: string,
    clip: import("./domain.js").ClipRecord,
    initial: import("./domain.js").UploadIntentRecord,
  ) {
    let intent = initial;
    const objectClass = this.laneFor(intent.sessionSchemaVersion).family.objectClass;
    for (const upload of intent.uploads) {
      if (upload.providerUploadId !== undefined) continue;
      const artifact = artifactForKind(clip, upload.kind);
      const providerUploadId = await this.storage.createMultipart(
        upload.objectKey,
        artifact.contentType,
        objectClass,
      );
      await this.repository.bindMultipartProviderUpload(
        deviceId,
        upload.id,
        providerUploadId,
      );
    }
    intent =
      (await this.repository.getUploadIntent(deviceId, initial.id)) ??
      (() => {
        throw notFound("upload_intent_not_found", "upload intent disappeared after provider binding");
      })();
    if (intent.uploads.some((upload) => upload.providerUploadId === undefined)) {
      throw conflict("upload_intent_not_ready", "not every multipart creation claim is provider-bound");
    }
    return intent;
  }

  sealSession(deviceId: string, sessionId: string, input: SealSessionInput) {
    return this.repository.sealSession(deviceId, sessionId, input);
  }

  // docs/47 · session-root artifacts: declare -> presigned PUT -> synchronous
  // server verify (small files, so no async worker). Same server-owned +
  // server-verified-SHA-256 property as the clip path.
  async declareSessionArtifacts(deviceId: string, sessionId: string, input: PutSessionArtifactsInput) {
    const session = await this.repository.getSession(deviceId, sessionId);
    if (!session) {
      throw notFound("session_not_found", "session was not found");
    }
    const declaredKinds = Object.keys(input.artifacts).filter(
      (kind) => input.artifacts[kind as SessionArtifactKind] !== undefined,
    ) as SessionArtifactKind[];
    if (declaredKinds.length === 0) {
      throw conflict("session_artifact_set_incomplete", "at least one artifact must be declared");
    }
    const lane = this.sessionArtifactLaneFor(session.sessionSchemaVersion, declaredKinds);
    const artifacts = await this.repository.declareSessionArtifacts(
      deviceId,
      sessionId,
      input,
      lane.prefix,
      lane.family.sessionArtifactKeyStyle,
    );
    return { sessionId, artifacts: artifacts.map(sessionArtifactStatus) };
  }

  async getSessionArtifacts(deviceId: string, sessionId: string) {
    const artifacts = await this.repository.getSessionArtifacts(deviceId, sessionId);
    return { sessionId, artifacts: artifacts.map(sessionArtifactStatus) };
  }

  async signSessionArtifact(
    deviceId: string,
    sessionId: string,
    kind: SessionArtifactKind,
    contentMd5: string,
  ) {
    const artifact = await this.repository.getSessionArtifact(deviceId, sessionId, kind);
    if (!artifact) {
      throw notFound("session_artifact_not_found", "session artifact was not declared");
    }
    if (artifact.verificationState === "verified" || artifact.verificationState === "rejected") {
      throw conflict(
        "session_artifact_immutable",
        "session artifact verification is already terminal",
      );
    }
    const objectClass = this.sessionArtifactLaneFor(artifact.sessionSchemaVersion).family.objectClass;
    const signed = await this.storage.signPutObject({
      objectKey: artifact.objectKey,
      contentType: artifact.contentType,
      bytes: artifact.expectedBytes,
      contentMd5,
      objectClass,
    });
    return { kind, objectKey: artifact.objectKey, ...signed };
  }

  async completeSessionArtifact(deviceId: string, sessionId: string, kind: SessionArtifactKind) {
    const artifact = await this.repository.getSessionArtifact(deviceId, sessionId, kind);
    if (!artifact) {
      throw notFound("session_artifact_not_found", "session artifact was not declared");
    }
    if (artifact.verificationState === "verified") {
      return sessionArtifactStatus(artifact);
    }
    if (artifact.verificationState === "rejected") {
      return sessionArtifactStatus(artifact);
    }
    const reject = (rejectionCode: string) =>
      this.repository
        .finalizeSessionArtifact(deviceId, sessionId, kind, { verified: false, rejectionCode })
        .then(sessionArtifactStatus);

    const objectClass = this.sessionArtifactLaneFor(artifact.sessionSchemaVersion).family.objectClass;
    const head = await this.storage.headObject(artifact.objectKey);
    if (head.bytes !== artifact.expectedBytes) return reject("size_mismatch");
    if (head.metadata.dataminer_class !== objectClass) return reject("metadata_mismatch");

    const stream = await this.storage.getObject(artifact.objectKey);
    const hash = createHash("sha256");
    let bytes = 0;
    for await (const chunk of stream) {
      const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk as Uint8Array);
      hash.update(buffer);
      bytes += buffer.length;
    }
    const actualSha256 = `sha256:${hash.digest("hex")}`;
    if (bytes !== artifact.expectedBytes) return reject("size_mismatch");
    if (actualSha256 !== artifact.expectedSha256) return reject("sha256_mismatch");
    return this.repository
      .finalizeSessionArtifact(deviceId, sessionId, kind, {
        verified: true,
        actualBytes: bytes,
        actualSha256,
      })
      .then(sessionArtifactStatus);
  }

  private async validateStoredObject(
    deviceId: string,
    upload: MultipartRecord,
    completionLeaseId: string,
    kind: (typeof artifactKinds)[number],
    head: { bytes: number; metadata: Record<string, string> },
    objectClass: string,
  ): Promise<void> {
    const errorCode =
      head.bytes !== upload.expectedBytes
        ? "head_size_mismatch"
        : head.metadata.dataminer_class !== objectClass
          ? "head_metadata_mismatch"
          : undefined;
    if (errorCode) {
      await this.repository.failMultipart(deviceId, upload.id, completionLeaseId, errorCode);
      throw conflict("object_head_mismatch", `${kind} object does not match its server declaration`);
    }
  }
}

function isPostStorageGateFailure(error: unknown): error is ApiError {
  return (
    error instanceof ApiError &&
    [
      "device_not_registered",
      "classification_blocked",
      "retention_blocked",
      "upload_intent_expired",
    ].includes(error.code)
  );
}

function uploadIntentResponse(intent: import("./domain.js").UploadIntentRecord) {
  return {
    uploadIntentId: intent.id,
    transferMode: "server_owned_r2_multipart" as const,
    artifacts: Object.fromEntries(
      intent.uploads.map((upload) => [
        upload.kind,
        {
          partSizeBytes: upload.partSizeBytes,
          partCount: expectedPartCount(upload.expectedBytes, upload.partSizeBytes),
          providerAttemptNumber: upload.attemptNo,
        },
      ]),
    ),
  };
}

function partsWithExpectedBytes(
  upload: MultipartRecord,
  parts: ReadonlyArray<{ partNumber: number; etag: string }>,
): Array<{ partNumber: number; etag: string; bytes: number }> {
  return parts.map((part) => ({
    ...part,
    bytes: expectedPartBytes(upload.expectedBytes, upload.partSizeBytes, part.partNumber),
  }));
}

function authoritativeParts(
  upload: MultipartRecord,
  parts: ReadonlyArray<UploadPart>,
): Array<{ partNumber: number; etag: string; bytes: number }> {
  return parts.map((part) => ({
    partNumber: part.partNumber,
    etag: part.etag,
    bytes:
      part.bytes ??
      expectedPartBytes(upload.expectedBytes, upload.partSizeBytes, part.partNumber),
  }));
}

function completionDiagnostics(
  kind: (typeof artifactKinds)[number],
  phase: string,
  error: unknown,
): Readonly<Record<string, string | number>> {
  return {
    artifactType: kind,
    phase,
    ...storageErrorDiagnostics(error),
  };
}

function sessionArtifactStatus(artifact: SessionArtifactRecord) {
  return {
    kind: artifact.kind,
    objectKey: artifact.objectKey,
    ingestState: artifact.ingestState,
    verificationState: artifact.verificationState,
    expectedBytes: artifact.expectedBytes,
    expectedSha256: artifact.expectedSha256,
    revision: artifact.revision,
  };
}

function requiredUpload(
  uploads: MultipartRecord[],
  kind: (typeof artifactKinds)[number],
): MultipartRecord {
  const upload = uploads.find((candidate) => candidate.kind === kind);
  if (!upload) {
    throw conflict("upload_intent_corrupt", `upload intent has no ${kind} multipart upload`);
  }
  return upload;
}

function requiredProviderUploadId(upload: MultipartRecord): string {
  if (!upload.providerUploadId) {
    throw conflict("upload_intent_not_ready", `${upload.kind} multipart is not provider-bound`);
  }
  return upload.providerUploadId;
}

function expectedPartCount(expectedBytes: number, partSizeBytes: number): number {
  const count = Math.ceil(expectedBytes / partSizeBytes);
  if (count > 10_000) {
    throw conflict("multipart_plan_too_large", "multipart upload cannot exceed 10000 parts");
  }
  return count;
}

function partSizeFor(expectedBytes: number, preferredBytes: number): number {
  const oneMib = 1024 * 1024;
  const minimumForPartLimit = Math.ceil(expectedBytes / 10_000);
  return Math.ceil(Math.max(preferredBytes, FIVE_MIB, minimumForPartLimit) / oneMib) * oneMib;
}

function expectedPartBytes(expectedBytes: number, partSizeBytes: number, partNumber: number): number {
  const partCount = expectedPartCount(expectedBytes, partSizeBytes);
  if (partNumber < 1 || partNumber > partCount) {
    throw badRequest("invalid_part_number", `partNumber must be between 1 and ${partCount}`);
  }
  return partNumber === partCount ? expectedBytes - partSizeBytes * (partCount - 1) : partSizeBytes;
}

function validatePartSize(
  expectedBytes: number,
  partSizeBytes: number,
  partNumber: number,
  bytes: number,
): void {
  const expected = expectedPartBytes(expectedBytes, partSizeBytes, partNumber);
  if (bytes !== expected) {
    throw badRequest("invalid_part_size", `part ${partNumber} must contain exactly ${expected} bytes`);
  }
  const partCount = expectedPartCount(expectedBytes, partSizeBytes);
  if (partNumber !== partCount && bytes < FIVE_MIB) {
    throw badRequest("part_too_small", "all multipart chunks except the last must be at least 5 MiB");
  }
}

function normalizeEtag(etag: string): string {
  return etag.replace(/^"|"$/g, "");
}

function reconcileParts(
  expectedBytes: number,
  partSizeBytes: number,
  requested: ReadonlyArray<{ partNumber: number; etag: string }>,
  listed: UploadPart[],
): UploadPart[] {
  const expectedCount = expectedPartCount(expectedBytes, partSizeBytes);
  if (requested.length !== expectedCount || listed.length !== expectedCount) {
    throw conflict("multipart_incomplete", `multipart upload must contain exactly ${expectedCount} parts`);
  }
  const listedByNumber = new Map(listed.map((part) => [part.partNumber, part]));
  return requested.map((part) => {
    const actual = listedByNumber.get(part.partNumber);
    if (!actual || normalizeEtag(actual.etag) !== normalizeEtag(part.etag)) {
      throw conflict("multipart_etag_mismatch", `R2 does not contain the declared ETag for part ${part.partNumber}`);
    }
    const expectedSize = expectedPartBytes(expectedBytes, partSizeBytes, part.partNumber);
    if (actual.bytes !== undefined && actual.bytes !== expectedSize) {
      throw conflict("multipart_size_mismatch", `R2 part ${part.partNumber} has an unexpected size`);
    }
    return actual;
  });
}

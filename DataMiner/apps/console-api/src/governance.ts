import { createHash } from "node:crypto";

import type { ConsoleConfig } from "./config.js";
import type { AuthenticatedSession, ConsoleRepository } from "./domain.js";
import { badRequest, forbidden, policyBlocked } from "./errors.js";
import type {
  ClipQualityInput,
  CreateAnnotatorInput,
  CreateConsentInput,
  CreateDeviceInput,
  ReviewVerdictInput,
  SessionQualityInput,
  UpdateAnnotatorInput,
  UpdateDeviceInput,
} from "./governance-contracts.js";
import type {
  CatalogRelease,
  CatalogVersion,
  GovernanceRepository,
  PurgeOutcome,
  QualityOutcome,
  ReviewVerdict,
  SessionQualityOutcome,
  WrittenAnnotator,
  WrittenConsent,
  WrittenDevice,
} from "./governance-domain.js";
import type { RequestContext } from "./auth.js";
import { requireCapability, type Capability } from "./rbac.js";
import { generateOpaqueToken, hashSessionToken } from "./secrets.js";

export type DeviceCredential = {
  device: WrittenDevice;
  // Returned once, never stored: the database keeps only sha256 of this value.
  token: string;
};

type Guard = {
  capability: Capability;
  sudo?: boolean;
};

export class GovernanceService {
  readonly #repository: GovernanceRepository;
  readonly #auditSink: ConsoleRepository;
  readonly #config: ConsoleConfig;

  constructor(
    repository: GovernanceRepository,
    auditSink: ConsoleRepository,
    config: ConsoleConfig,
  ) {
    this.#repository = repository;
    this.#auditSink = auditSink;
    this.#config = config;
  }

  async createAnnotator(
    actor: AuthenticatedSession,
    key: string,
    input: CreateAnnotatorInput,
    context: RequestContext,
  ): Promise<WrittenAnnotator> {
    return this.#write(
      actor,
      { capability: "identity.write" },
      "annotator.create",
      key,
      input,
      context,
      async () => {
        const annotator = await this.#repository.createAnnotator(input);
        return { value: annotator, entityType: "annotator", entityId: annotator.id, details: {} };
      },
    );
  }

  async updateAnnotator(
    actor: AuthenticatedSession,
    key: string,
    id: string,
    input: UpdateAnnotatorInput,
    context: RequestContext,
  ): Promise<WrittenAnnotator> {
    return this.#write(
      actor,
      { capability: "identity.write" },
      "annotator.update",
      key,
      { id, ...input },
      context,
      async () => {
        const annotator = await this.#repository.updateAnnotator(id, input);
        return {
          value: annotator,
          entityType: "annotator",
          entityId: id,
          details: { status: input.status ?? null, aliasChanged: input.alias !== undefined },
        };
      },
    );
  }

  async createConsent(
    actor: AuthenticatedSession,
    key: string,
    input: CreateConsentInput,
    context: RequestContext,
  ): Promise<WrittenConsent> {
    this.#refuseTrainable(input.allowsTrainableUse, "consent");
    return this.#write(
      actor,
      { capability: "governance.write" },
      "consent.register",
      key,
      input,
      context,
      async () => {
        const consent = await this.#repository.createConsent(input);
        return {
          value: consent,
          entityType: "consent",
          entityId: consent.id,
          details: { annotatorId: input.annotatorId, policyVersion: input.policyVersion },
        };
      },
    );
  }

  async revokeConsent(
    actor: AuthenticatedSession,
    key: string,
    id: string,
    reason: string,
    context: RequestContext,
  ): Promise<WrittenConsent> {
    return this.#write(
      actor,
      { capability: "governance.write", sudo: true },
      "consent.revoke",
      key,
      { id, reason },
      context,
      async () => {
        const consent = await this.#repository.revokeConsent(id, reason);
        return { value: consent, entityType: "consent", entityId: id, details: { reason } };
      },
    );
  }

  async createDevice(
    actor: AuthenticatedSession,
    key: string,
    input: CreateDeviceInput,
    context: RequestContext,
  ): Promise<DeviceCredential> {
    return this.#write(
      actor,
      { capability: "identity.write", sudo: true },
      "device.register",
      key,
      input,
      context,
      async () => {
        const token = generateOpaqueToken();
        const device = await this.#repository.createDevice(input, hashSessionToken(token));
        return {
          value: { device, token },
          entityType: "device",
          entityId: device.id,
          details: { clientDeviceId: input.clientDeviceId, credentialIssued: true },
          // Replaying a device registration must not hand the token out again;
          // the caller sees the device and a null token instead.
          replayable: { device, token: null },
        };
      },
    );
  }

  async updateDevice(
    actor: AuthenticatedSession,
    key: string,
    id: string,
    input: UpdateDeviceInput,
    context: RequestContext,
  ): Promise<WrittenDevice> {
    return this.#write(
      actor,
      { capability: "identity.write" },
      "device.update",
      key,
      { id, ...input },
      context,
      async () => {
        const device = await this.#repository.updateDevice(id, input);
        return {
          value: device,
          entityType: "device",
          entityId: id,
          details: { status: input.status ?? null, annotatorId: input.annotatorId ?? null },
        };
      },
    );
  }

  async rotateDeviceCredential(
    actor: AuthenticatedSession,
    key: string,
    id: string,
    context: RequestContext,
  ): Promise<DeviceCredential> {
    return this.#write(
      actor,
      { capability: "identity.write", sudo: true },
      "device.rotate_credential",
      key,
      { id },
      context,
      async () => {
        const token = generateOpaqueToken();
        const device = await this.#repository.rotateDeviceCredential(id, hashSessionToken(token));
        return {
          value: { device, token },
          entityType: "device",
          entityId: id,
          details: { credentialRotated: true },
          replayable: { device, token: null },
        };
      },
    );
  }

  async recordClipQuality(
    actor: AuthenticatedSession,
    key: string,
    clipId: string,
    input: ClipQualityInput,
    context: RequestContext,
  ): Promise<QualityOutcome> {
    return this.#write(
      actor,
      { capability: "governance.write" },
      "clip.quality_reviewed",
      key,
      { clipId, ...input },
      context,
      async () => {
        const outcome = await this.#repository.recordClipQuality(
          clipId,
          input.result,
          actor.operator.email,
        );
        return {
          value: outcome,
          entityType: "clip",
          entityId: clipId,
          details: { result: input.result, note: input.note ?? null },
        };
      },
    );
  }

  /**
   * session 级 QA 判定（issue #855 PR 5）。
   *
   * 与逐 clip 判定**同一道闸**（`governance.write`，无 sudo，docs/10 §5）、同一条
   * 审计写路径、同一把幂等键。差别只有粒度：一条 audit event 记的是整场，details
   * 里带上这次落到几条、跳过几条。
   *
   * `quality_reviewed_by` 记的是发起这次判定的 operator 邮箱——与逐 clip 入口逐字
   * 一致。这里永远不会出现 "system"：`clips` 四个状态里只有 quality 带
   * `quality_reviewed_by` 一列，0001 的 CHECK 又要求"不是 pending 就必须有审阅时间"，
   * 这套结构本身就是在说 QA 是人的裁决点。
   */
  async recordSessionQuality(
    actor: AuthenticatedSession,
    key: string,
    sessionId: string,
    input: SessionQualityInput,
    context: RequestContext,
  ): Promise<SessionQualityOutcome> {
    return this.#write(
      actor,
      { capability: "governance.write" },
      "session.quality_reviewed",
      key,
      { sessionId, ...input },
      context,
      async () => {
        const outcome = await this.#repository.recordSessionQuality(
          sessionId,
          input.result,
          actor.operator.email,
        );
        return {
          value: outcome,
          entityType: "session",
          entityId: sessionId,
          details: {
            result: input.result,
            note: input.note ?? null,
            decidedClipCount: outcome.decidedClipCount,
            skippedClipCount: outcome.skippedClipCount,
          },
        };
      },
    );
  }

  async recordReviewVerdict(
    actor: AuthenticatedSession,
    key: string,
    input: ReviewVerdictInput,
    context: RequestContext,
  ): Promise<ReviewVerdict> {
    // Same guard tier as the clip quality decision (docs/10 §5): governance
    // write without sudo. The subject may have no clip row at all — that is the
    // point of this ledger (docs/33 §5).
    return this.#write(
      actor,
      { capability: "governance.write" },
      "review.verdict_recorded",
      key,
      input,
      context,
      async () => {
        const verdict = await this.#repository.recordReviewVerdict(
          input,
          actor.operator.id,
          context.requestId,
        );
        return {
          value: verdict,
          entityType: "r2_object",
          entityId: input.subjectKey,
          details: {
            source: input.source,
            subjectKind: input.subjectKind,
            result: input.result,
            note: input.note ?? null,
          },
        };
      },
    );
  }

  async listReviewVerdicts(
    actor: AuthenticatedSession,
    source: string,
    subjectKey: string,
  ): Promise<ReviewVerdict[]> {
    requireCapability(actor.operator.role, "review.read");
    return this.#repository.listReviewVerdicts(source, subjectKey, 50);
  }

  async requestClipPurge(
    actor: AuthenticatedSession,
    key: string,
    clipId: string,
    reason: string,
    context: RequestContext,
  ): Promise<PurgeOutcome> {
    return this.#write(
      actor,
      { capability: "governance.write", sudo: true },
      "clip.purge_requested",
      key,
      { clipId, reason },
      context,
      async () => {
        const outcome = await this.#repository.requestClipPurge(clipId, reason, actor.operator.id);
        return { value: outcome, entityType: "clip", entityId: clipId, details: { reason } };
      },
    );
  }

  async listCatalogVersions(
    actor: AuthenticatedSession,
    kind?: string,
    gameId?: string,
  ): Promise<CatalogVersion[]> {
    requireCapability(actor.operator.role, "console.read");
    return this.#repository.listCatalogVersions(kind, gameId);
  }

  async listCatalogReleases(
    actor: AuthenticatedSession,
    channel?: string,
  ): Promise<CatalogRelease[]> {
    requireCapability(actor.operator.role, "console.read");
    return this.#repository.listCatalogReleases(channel);
  }

  // A trainable consent or grant is refused rather than silently downgraded, so
  // an operator cannot believe the permission was recorded. The refusal is now
  // configuration, not a constant (docs/49 E7): a deployment that has completed
  // consent closure and admits the first-party lane sets
  // CONSOLE_GOVERNANCE_ALLOW_TRAINABLE=true; everywhere else the historical
  // fail-closed behaviour stands.
  #refuseTrainable(requested: boolean, subject: string): void {
    if (!requested) return;
    if (this.#config.governance.allowTrainable) return;
    throw policyBlocked(
      `this deployment does not issue a trainable ${subject}: set CONSOLE_GOVERNANCE_ALLOW_TRAINABLE only once consent closure and the first-party lane are in place`,
    );
  }

  #requireGuard(actor: AuthenticatedSession, guard: Guard): void {
    requireCapability(actor.operator.role, guard.capability);
    if (guard.sudo && !actor.session.sudoActive) {
      throw forbidden(
        "sudo_required",
        `re-enter your password to confirm this action; it stays confirmed for ${this.#config.session.sudoMinutes} minutes`,
      );
    }
  }

  async #write<T>(
    actor: AuthenticatedSession,
    guard: Guard,
    action: string,
    key: string,
    request: unknown,
    context: RequestContext,
    operation: () => Promise<{
      value: T;
      entityType: string;
      entityId: string;
      details: Record<string, unknown>;
      replayable?: unknown;
    }>,
  ): Promise<T> {
    this.#requireGuard(actor, guard);

    const scope = `console:${action}:${actor.operator.id}`;
    const requestHash = `sha256:${createHash("sha256")
      .update(JSON.stringify(request ?? null))
      .digest("hex")}`;

    const replay = await this.#repository.beginIdempotent(scope, key, requestHash);
    if (replay) return replay.body as T;

    let outcome;
    try {
      outcome = await operation();
    } catch (error) {
      await this.#repository.releaseIdempotent(scope, key);
      throw error;
    }

    await this.#auditSink.recordAudit({
      actorType: "operator",
      actorId: actor.operator.id,
      action,
      entityType: outcome.entityType,
      entityId: outcome.entityId,
      requestId: context.requestId,
      details: { ...outcome.details, ipHash: context.ipHash, operator: actor.operator.email },
    });
    await this.#repository.finishIdempotent(
      scope,
      key,
      outcome.replayable === undefined ? outcome.value : outcome.replayable,
    );
    return outcome.value;
  }
}

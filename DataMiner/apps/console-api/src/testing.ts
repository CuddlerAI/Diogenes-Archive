import { randomUUID } from "node:crypto";

import type {
  AuditEvent,
  ChallengeIssueInput,
  ChallengePurpose,
  ChallengeRecord,
  ConsoleRepository,
  CreateOperatorInput,
  OperatorRecord,
  OperatorRole,
  OperatorStatus,
  RateLimitVerdict,
  SessionIssueInput,
  SessionLookup,
  SessionRecord,
} from "./domain.js";
import type { StatsOutputQuery, TimeBasis } from "./browse-contracts.js";
import type {
  AnnotatorDetail,
  AnnotatorSummary,
  AuditEntry,
  BrowseRepository,
  ClientEventEntry,
  ClipDetail,
  ClipSummary,
  Contribution,
  DailyIntake,
  DeviceDetail,
  DeviceSummary,
  GameDistribution,
  GameSummary,
  OutputReport,
  PipelineHealth,
  PipelineOverview,
  SessionArtifactSummary,
  SessionDetail,
  SessionSummary,
} from "./browse-domain.js";
import type { ReviewVerdictInput } from "./governance-contracts.js";
import type {
  CatalogRelease,
  CatalogVersion,
  GovernanceRepository,
  IdempotentReplay,
  PurgeOutcome,
  QualityOutcome,
  ReviewVerdict,
  SessionQualityOutcome,
  WrittenAnnotator,
  WrittenConsent,
  WrittenDevice,
} from "./governance-domain.js";
import type { ConsoleMail, Mailer } from "./mailer.js";
import type { Page } from "./pagination.js";

/**
 * Test budget for the files whose cases perform real password hashing.
 *
 * Argon2id at the profile `secrets.ts` pins (64 MiB, t=3) costs ~80 ms per
 * operation on an unloaded developer machine, and a case that walks the lockout
 * ladder spends twenty of them. That cost is the point: it is the password
 * hardening parameter, and `login` deliberately verifies an unknown address
 * against a decoy hash so timing cannot separate a missing account from a wrong
 * password. Neither may be weakened to make the suite finish sooner, so the
 * budget moves instead.
 *
 * `vitest run` executes the package's twenty-three files in parallel workers,
 * and hashing is memory-hard, so these cases contend for cache and memory
 * bandwidth rather than sharing it. Measured against an unloaded baseline they
 * run ~4x slower under moderate contention, and the failures this constant
 * exists to stop were ~12x. Vitest's five-second default leaves no room for
 * that: the slowest hashing case takes 1.6 s alone and would need only ~3x to
 * trip it.
 *
 * This is therefore sized as a hang detector, not a performance target — ~35x
 * the slowest hashing case. A case here does no I/O it could block on, so
 * exceeding a minute means something is genuinely stuck, which is the only
 * thing the timeout should be reporting. Apply it per file with
 * `vi.setConfig({ testTimeout: argon2TestTimeoutMs })`.
 */
export const argon2TestTimeoutMs = 60_000;

type StoredChallenge = ChallengeRecord & {
  consumedAt: number | null;
  supersededAt: number | null;
  expiresAtMs: number;
};

type StoredSession = Omit<SessionRecord, "sudoActive"> & {
  tokenHash: string;
  revokedAt: number | null;
  revokeReason: string | null;
  expiresAtMs: number;
  lastSeenAtMs: number;
};

// `locked` is derived from the clock on read, exactly as the SQL projection does.
type StoredOperator = Omit<OperatorRecord, "locked"> & { lockedUntilMs: number | null };

/**
 * In-memory stand-in for the Cloud SQL repository. It mirrors the guards the SQL
 * statements enforce (single-use challenges, attempt budgets, fixed rate-limit
 * windows, session revocation) so behaviour tests do not need a database.
 */
export class MemoryConsoleRepository implements ConsoleRepository {
  readonly operators = new Map<string, StoredOperator>();
  readonly challenges = new Map<string, StoredChallenge>();
  readonly sessions = new Map<string, StoredSession>();
  readonly rateLimits = new Map<string, { windowStartedAtMs: number; counter: number }>();
  readonly auditEvents: AuditEvent[] = [];
  pingFails = false;
  #now: number;

  constructor(startMs = Date.parse("2026-07-24T00:00:00.000Z")) {
    this.#now = startMs;
  }

  now(): number {
    return this.#now;
  }

  advance(ms: number): void {
    this.#now += ms;
  }

  /** Overridden by the readyz tests to simulate a deploy that skipped grants. */
  missingGrants: string[] = [];

  async missingConsoleGrants(): Promise<string[]> {
    return this.missingGrants;
  }

  async ping(): Promise<void> {
    if (this.pingFails) throw new Error("database unavailable");
  }

  async close(): Promise<void> {}

  async findOperatorByEmail(email: string): Promise<OperatorRecord | null> {
    for (const operator of this.operators.values()) {
      if (operator.email === email.toLowerCase()) return this.#project(operator);
    }
    return null;
  }

  async findOperatorById(id: string): Promise<OperatorRecord | null> {
    const operator = this.operators.get(id);
    return operator ? this.#project(operator) : null;
  }

  async listOperators(): Promise<OperatorRecord[]> {
    return [...this.operators.values()]
      .filter((operator) => operator.status !== "tombstoned")
      .map((operator) => this.#project(operator));
  }

  async createOperator(input: CreateOperatorInput): Promise<OperatorRecord> {
    const timestamp = new Date(this.#now).toISOString();
    const operator: StoredOperator = {
      id: randomUUID(),
      email: input.email.toLowerCase(),
      passwordHash: null,
      role: input.role,
      status: "invited",
      failedLoginCount: 0,
      lockedUntil: null,
      lockedUntilMs: null,
      passwordUpdatedAt: null,
      lastLoginAt: null,
      createdAt: timestamp,
      updatedAt: timestamp,
    };
    this.operators.set(operator.id, operator);
    return this.#project(operator);
  }

  async setOperatorRole(operatorId: string, role: OperatorRole): Promise<OperatorRecord> {
    const operator = this.#operator(operatorId);
    operator.role = role;
    return this.#project(operator);
  }

  async setOperatorStatus(operatorId: string, status: OperatorStatus): Promise<OperatorRecord> {
    const operator = this.#operator(operatorId);
    operator.status = status;
    if (status === "active") {
      operator.failedLoginCount = 0;
      operator.lockedUntil = null;
      operator.lockedUntilMs = null;
    }
    return this.#project(operator);
  }

  async activateOperatorWithPassword(operatorId: string, passwordHash: string): Promise<void> {
    const operator = this.#operator(operatorId);
    if (operator.status !== "invited") throw new Error("operator is not invited");
    operator.passwordHash = passwordHash;
    operator.passwordUpdatedAt = new Date(this.#now).toISOString();
    operator.status = "active";
    operator.failedLoginCount = 0;
    operator.lockedUntil = null;
    operator.lockedUntilMs = null;
  }

  async setOperatorPassword(operatorId: string, passwordHash: string): Promise<void> {
    const operator = this.#operator(operatorId);
    if (operator.status !== "active") throw new Error("operator is not active");
    operator.passwordHash = passwordHash;
    operator.passwordUpdatedAt = new Date(this.#now).toISOString();
    operator.failedLoginCount = 0;
    operator.lockedUntil = null;
    operator.lockedUntilMs = null;
  }

  async registerLoginFailure(
    operatorId: string,
    policy: { threshold: number; baseMinutes: number; maxMinutes: number },
  ): Promise<void> {
    const operator = this.#operator(operatorId);
    operator.failedLoginCount += 1;
    if (operator.failedLoginCount % policy.threshold === 0) {
      const step = Math.max(operator.failedLoginCount / policy.threshold - 1, 0);
      const minutes = Math.min(policy.maxMinutes, policy.baseMinutes * 2 ** step);
      operator.lockedUntilMs = this.#now + minutes * 60_000;
      operator.lockedUntil = new Date(operator.lockedUntilMs).toISOString();
    }
  }

  async clearLoginFailures(operatorId: string): Promise<void> {
    const operator = this.#operator(operatorId);
    operator.failedLoginCount = 0;
    operator.lockedUntil = null;
    operator.lockedUntilMs = null;
  }

  async markOperatorLoggedIn(operatorId: string): Promise<void> {
    this.#operator(operatorId).lastLoginAt = new Date(this.#now).toISOString();
  }

  async issueChallenge(input: ChallengeIssueInput): Promise<ChallengeRecord> {
    const expiresAtMs = this.#now + input.ttlSeconds * 1_000;
    const challenge: StoredChallenge = {
      id: randomUUID(),
      operatorId: input.operatorId,
      purpose: input.purpose,
      codeSalt: input.codeSalt,
      codeHash: input.codeHash,
      expiresAt: new Date(expiresAtMs).toISOString(),
      expiresAtMs,
      attemptCount: 0,
      consumedAt: null,
      supersededAt: null,
    };
    this.challenges.set(challenge.id, challenge);
    return { ...challenge };
  }

  async supersedeOpenChallenges(operatorId: string, purpose: ChallengePurpose): Promise<void> {
    for (const challenge of this.challenges.values()) {
      if (challenge.operatorId !== operatorId || challenge.purpose !== purpose) continue;
      if (challenge.consumedAt !== null || challenge.supersededAt !== null) continue;
      challenge.supersededAt = this.#now;
    }
  }

  async findOpenChallenge(challengeId: string): Promise<ChallengeRecord | null> {
    const challenge = this.challenges.get(challengeId);
    if (!challenge || !this.#challengeOpen(challenge)) return null;
    return { ...challenge };
  }

  async claimChallengeAttempt(
    challengeId: string,
    maxAttempts: number,
  ): Promise<ChallengeRecord | null> {
    const challenge = this.challenges.get(challengeId);
    if (!challenge || !this.#challengeOpen(challenge)) return null;
    if (challenge.attemptCount >= maxAttempts) return null;
    challenge.attemptCount += 1;
    return { ...challenge };
  }

  async consumeChallenge(challengeId: string): Promise<boolean> {
    const challenge = this.challenges.get(challengeId);
    if (!challenge || challenge.consumedAt !== null) return false;
    challenge.consumedAt = this.#now;
    return true;
  }

  async issueSession(input: SessionIssueInput): Promise<SessionRecord> {
    const expiresAtMs = this.#now + input.absoluteSeconds * 1_000;
    const session: StoredSession = {
      id: randomUUID(),
      operatorId: input.operatorId,
      tokenHash: input.tokenHash,
      sudoUntil: null,
      expiresAt: new Date(expiresAtMs).toISOString(),
      expiresAtMs,
      lastSeenAt: new Date(this.#now).toISOString(),
      lastSeenAtMs: this.#now,
      createdAt: new Date(this.#now).toISOString(),
      revokedAt: null,
      revokeReason: null,
    };
    this.sessions.set(session.id, session);
    return this.#projectSession(session);
  }

  async findSessionByTokenHash(tokenHash: string, idleSeconds: number): Promise<SessionLookup> {
    for (const session of this.sessions.values()) {
      if (session.tokenHash !== tokenHash) continue;
      if (session.revokedAt !== null || session.expiresAtMs <= this.#now) {
        return { outcome: "missing" };
      }
      if (session.lastSeenAtMs <= this.#now - idleSeconds * 1_000) {
        return { outcome: "idle", sessionId: session.id };
      }
      const operator = this.operators.get(session.operatorId);
      if (!operator) return { outcome: "missing" };
      return {
        outcome: "active",
        session: this.#projectSession(session),
        operator: { ...operator, locked: this.#lockedNow(operator) },
      };
    }
    return { outcome: "missing" };
  }

  async touchSession(sessionId: string): Promise<void> {
    const session = this.sessions.get(sessionId);
    if (!session || session.revokedAt !== null) return;
    session.lastSeenAtMs = this.#now;
    session.lastSeenAt = new Date(this.#now).toISOString();
  }

  async grantSessionSudo(sessionId: string, sudoSeconds: number): Promise<void> {
    const session = this.sessions.get(sessionId);
    if (!session || session.revokedAt !== null) return;
    session.sudoUntil = new Date(this.#now + sudoSeconds * 1_000).toISOString();
  }

  async revokeSession(sessionId: string, reason: string): Promise<void> {
    const session = this.sessions.get(sessionId);
    if (!session || session.revokedAt !== null) return;
    session.revokedAt = this.#now;
    session.revokeReason = reason;
  }

  async revokeOperatorSessions(operatorId: string, reason: string): Promise<number> {
    let revoked = 0;
    for (const session of this.sessions.values()) {
      if (session.operatorId !== operatorId || session.revokedAt !== null) continue;
      session.revokedAt = this.#now;
      session.revokeReason = reason;
      revoked += 1;
    }
    return revoked;
  }

  async consumeRateLimit(
    bucket: string,
    windowSeconds: number,
    limit: number,
  ): Promise<RateLimitVerdict> {
    const windowMs = windowSeconds * 1_000;
    const existing = this.rateLimits.get(bucket);
    const state =
      existing && existing.windowStartedAtMs + windowMs > this.#now
        ? { windowStartedAtMs: existing.windowStartedAtMs, counter: existing.counter + 1 }
        : { windowStartedAtMs: this.#now, counter: 1 };
    this.rateLimits.set(bucket, state);
    const elapsedSeconds = Math.max(0, Math.floor((this.#now - state.windowStartedAtMs) / 1_000));
    return {
      allowed: state.counter <= limit,
      retryAfterSeconds: Math.max(1, windowSeconds - elapsedSeconds),
    };
  }

  async recordAudit(event: AuditEvent): Promise<void> {
    this.auditEvents.push(event);
  }

  actionsFor(action: string): AuditEvent[] {
    return this.auditEvents.filter((event) => event.action === action);
  }

  #projectSession(session: StoredSession): SessionRecord {
    return {
      ...session,
      sudoActive: session.sudoUntil !== null && Date.parse(session.sudoUntil) > this.#now,
    };
  }

  #project(operator: StoredOperator): OperatorRecord {
    return { ...operator, locked: this.#lockedNow(operator) };
  }

  #lockedNow(operator: StoredOperator): boolean {
    return operator.lockedUntilMs !== null && operator.lockedUntilMs > this.#now;
  }

  #challengeOpen(challenge: StoredChallenge): boolean {
    return (
      challenge.consumedAt === null &&
      challenge.supersededAt === null &&
      challenge.expiresAtMs > this.#now
    );
  }

  #operator(operatorId: string): StoredOperator {
    const operator = this.operators.get(operatorId);
    if (!operator) throw new Error(`unknown operator ${operatorId}`);
    return operator;
  }
}

export class MemoryMailer implements Mailer {
  readonly outbox: ConsoleMail[] = [];
  failWith: Error | null = null;

  async send(mail: ConsoleMail): Promise<void> {
    if (this.failWith) throw this.failWith;
    this.outbox.push(mail);
  }

  lastCode(): string {
    const last = this.outbox.at(-1);
    if (!last) throw new Error("no mail was sent");
    const match = /\b([0-9]{6})\b/.exec(last.body);
    if (!match?.[1]) throw new Error("no verification code in the last mail");
    return match[1];
  }

  lastLinkParams(): { challenge: string; token: string } {
    const last = this.outbox.at(-1);
    if (!last) throw new Error("no mail was sent");
    const match = /https?:\/\/\S+/.exec(last.body);
    if (!match) throw new Error("no link in the last mail");
    const url = new URL(match[0]);
    const challenge = url.searchParams.get("challenge");
    const token = url.searchParams.get("token");
    if (!challenge || !token) throw new Error("link is missing challenge or token");
    return { challenge, token };
  }
}

/** Minimal browse stand-in for HTTP boundary tests, which assert routing and roles. */
export class StubBrowseRepository implements BrowseRepository {
  readonly calls: string[] = [];
  annotator: AnnotatorDetail | null = null;
  device: DeviceDetail | null = null;
  session: SessionDetail | null = null;
  clip: ClipDetail | null = null;

  async listAnnotators(): Promise<Page<AnnotatorSummary>> {
    return this.#page("listAnnotators");
  }

  async getAnnotator(id: string): Promise<AnnotatorDetail | null> {
    this.calls.push(`getAnnotator:${id}`);
    return this.annotator;
  }

  async listDevices(): Promise<Page<DeviceSummary>> {
    return this.#page("listDevices");
  }

  async getDevice(id: string): Promise<DeviceDetail | null> {
    this.calls.push(`getDevice:${id}`);
    return this.device;
  }

  async listSessions(): Promise<Page<SessionSummary>> {
    return this.#page("listSessions");
  }

  async getSession(id: string): Promise<SessionDetail | null> {
    this.calls.push(`getSession:${id}`);
    return this.session;
  }

  async listClips(): Promise<Page<ClipSummary>> {
    return this.#page("listClips");
  }

  async getClip(id: string): Promise<ClipDetail | null> {
    this.calls.push(`getClip:${id}`);
    return this.clip;
  }

  async listGames(): Promise<GameSummary[]> {
    this.calls.push("listGames");
    return [];
  }

  async listAuditEvents(): Promise<Page<AuditEntry>> {
    return this.#page("listAuditEvents");
  }

  async contributionForAnnotator(id: string): Promise<Contribution> {
    this.calls.push(`contributionForAnnotator:${id}`);
    return emptyContribution();
  }

  async contributionForDevice(id: string): Promise<Contribution> {
    this.calls.push(`contributionForDevice:${id}`);
    return emptyContribution();
  }

  async listSessionArtifacts(sessionId: string): Promise<SessionArtifactSummary[]> {
    this.calls.push(`listSessionArtifacts:${sessionId}`);
    return [];
  }

  async listClientEvents(): Promise<Page<ClientEventEntry>> {
    return this.#page("listClientEvents");
  }


  async pipelineOverview(): Promise<PipelineOverview> {
    this.calls.push("pipelineOverview");
    return {
      devices: { total: 0, active: 0, seenLast24h: 0, seenLast7d: 0 },
      sessions: { total: 0, last24h: 0 },
      clips: {
        total: 0,
        pending: 0,
        uploading: 0,
        stored: 0,
        retryWait: 0,
        failed: 0,
        verified: 0,
        rejected: 0,
        purgeActive: 0,
      },
      clipSeconds: {
        total: 0,
        pending: 0,
        uploading: 0,
        stored: 0,
        retryWait: 0,
        failed: 0,
        verified: 0,
        rejected: 0,
        purgeActive: 0,
      },
      storedBytes: 0,
      trainableClips: 0,
      trainableSeconds: 0,
      depth: { clips: 0, bytes: 0, seconds: 0 },
    };
  }

  async pipelineHealth(): Promise<PipelineHealth> {
    this.calls.push("pipelineHealth");
    const empty = { pending: 0, running: 0, retryWait: 0, failed: 0, deadLetter: 0 };
    return {
      verification: empty,
      purge: empty,
      uploads: { activeIntents: 0, activeMultiparts: 0, expiredMultiparts: 0 },
      timeZone: "Asia/Shanghai",
      storageByDay: [],
      recentFailures: [],
      qualityBacklog: {
        pendingClipCount: 0,
        pendingSessionCount: 0,
        oldestVerifiedAt: null,
        oldestWaitHours: null,
        overdueThresholdHours: 24,
        overdueClipCount: 0,
        overdueSessionCount: 0,
      },
    };
  }

  async dailyIntake(days: number, basis: TimeBasis | undefined, tz: string): Promise<DailyIntake> {
    this.calls.push(`dailyIntake:${days}:${basis ?? "default"}:${tz}`);
    return {
      timeBasis: basis ?? "recorded",
      timeZone: tz,
      daily: [],
      totals: {
        sessionCount: 0,
        clipCount: 0,
        storedClipCount: 0,
        verifiedClipCount: 0,
        unhealthyClipCount: 0,
        capturedSeconds: 0,
        storedBytes: 0,
        activeDeviceCount: 0,
        activeAnnotatorCount: 0,
        trainableClipCount: 0,
        depthClipCount: 0,
        depthBytes: 0,
      },
    };
  }

  async outputReport(query: StatsOutputQuery): Promise<OutputReport> {
    this.calls.push(
      `outputReport:${query.from}:${query.to}:${query.bucket}:${query.groupBy.join("+")}:${query.tz}`,
    );
    return {
      from: query.from,
      to: query.to,
      bucket: query.bucket,
      timeBasis: query.timeBasis ?? "recorded",
      timeZone: query.tz,
      groupBy: query.groupBy,
      buckets: [],
      rows: [],
      columnTotals: [],
      totals: {
        clipCount: 0,
        storedClipCount: 0,
        verifiedClipCount: 0,
        unhealthyClipCount: 0,
        capturedSeconds: 0,
        storedBytes: 0,
        depthClipCount: 0,
        depthBytes: 0,
        trainableClipCount: 0,
      },
      enrolledAnnotatorCount: null,
    };
  }

  async gameDistribution(): Promise<GameDistribution> {
    this.calls.push("gameDistribution");
    return {
      asOf: "2026-01-01T00:00:00.000Z",
      rows: [],
      totals: {
        capturedSeconds: 0,
        sessionCount: 0,
        clipCount: 0,
        trainableClipCount: 0,
        trainableSeconds: 0,
      },
    };
  }

  #page<T>(call: string): Page<T> {
    this.calls.push(call);
    return { items: [], nextCursor: null };
  }
}

function emptyContribution(): Contribution {
  return {
    timeBasis: "recorded",
    timeZone: "Asia/Shanghai",
    totals: {
      sessionCount: 0,
      clipCount: 0,
      storedClipCount: 0,
      verifiedClipCount: 0,
      rejectedClipCount: 0,
      capturedSeconds: 0,
      unhealthyClipCount: 0,
      storedBytes: 0,
      firstActivityAt: null,
      lastActivityAt: null,
    },
    daily: [],
  };
}


/** Governance stand-in for HTTP boundary tests: records calls, writes nothing. */
export class StubGovernanceRepository implements GovernanceRepository {
  readonly calls: string[] = [];
  readonly idempotencyKeys = new Map<string, unknown>();

  async createAnnotator(): Promise<WrittenAnnotator> {
    this.calls.push("createAnnotator");
    return { id: randomUUID(), alias: "alpha", status: "active", createdAt: new Date(0).toISOString() };
  }

  async updateAnnotator(id: string): Promise<WrittenAnnotator> {
    this.calls.push("updateAnnotator");
    return { id, alias: "alpha", status: "suspended", createdAt: new Date(0).toISOString() };
  }

  async recordReviewVerdict(input: ReviewVerdictInput, operatorId: string): Promise<ReviewVerdict> {
    this.calls.push("recordReviewVerdict");
    return {
      id: randomUUID(),
      source: input.source,
      subjectKind: input.subjectKind,
      subjectKey: input.subjectKey,
      result: input.result,
      note: input.note ?? null,
      operatorId,
      operatorEmail: null,
      createdAt: new Date(0).toISOString(),
    };
  }

  async listReviewVerdicts(): Promise<ReviewVerdict[]> {
    this.calls.push("listReviewVerdicts");
    return [];
  }

  async createConsent(): Promise<WrittenConsent> {
    this.calls.push("createConsent");
    return {
      id: randomUUID(),
      annotatorId: randomUUID(),
      policyVersion: "v1",
      status: "active",
      allowsTrainableUse: false,
      acceptedAt: new Date(0).toISOString(),
      expiresAt: null,
    };
  }

  async revokeConsent(id: string): Promise<WrittenConsent> {
    this.calls.push("revokeConsent");
    return {
      id,
      annotatorId: randomUUID(),
      policyVersion: "v1",
      status: "revoked",
      allowsTrainableUse: false,
      acceptedAt: new Date(0).toISOString(),
      expiresAt: null,
    };
  }

  async createDevice(_input: unknown, credentialFingerprint: string): Promise<WrittenDevice> {
    this.calls.push("createDevice");
    return {
      id: randomUUID(),
      clientDeviceId: "dev-1",
      annotatorId: null,
      status: "pending",
      credentialFingerprint,
    };
  }

  async updateDevice(id: string): Promise<WrittenDevice> {
    this.calls.push("updateDevice");
    return {
      id,
      clientDeviceId: "dev-1",
      annotatorId: null,
      status: "active",
      credentialFingerprint: "a".repeat(64),
    };
  }

  async rotateDeviceCredential(id: string, credentialFingerprint: string): Promise<WrittenDevice> {
    this.calls.push("rotateDeviceCredential");
    return {
      id,
      clientDeviceId: "dev-1",
      annotatorId: null,
      status: "active",
      credentialFingerprint,
    };
  }


  async recordClipQuality(clipId: string, result: "passed" | "failed"): Promise<QualityOutcome> {
    this.calls.push("recordClipQuality");
    return {
      clipId,
      qualityState: result,
      reviewedBy: "ops@diogenesmodel.com",
      reviewedAt: new Date(0).toISOString(),
    };
  }

  async recordSessionQuality(
    sessionId: string,
    result: "passed" | "failed",
  ): Promise<SessionQualityOutcome> {
    this.calls.push("recordSessionQuality");
    return {
      sessionId,
      qualityState: result,
      reviewedBy: "ops@diogenesmodel.com",
      reviewedAt: new Date(0).toISOString(),
      decidedClipCount: 3,
      skippedClipCount: 1,
    };
  }

  async requestClipPurge(clipId: string): Promise<PurgeOutcome> {
    this.calls.push("requestClipPurge");
    return { clipId, retentionState: "purge_requested", enqueued: true };
  }

  async listCatalogVersions(): Promise<CatalogVersion[]> {
    this.calls.push("listCatalogVersions");
    return [];
  }

  async listCatalogReleases(): Promise<CatalogRelease[]> {
    this.calls.push("listCatalogReleases");
    return [];
  }

  async beginIdempotent(scope: string, key: string): Promise<IdempotentReplay | null> {
    const composite = `${scope}|${key}`;
    if (this.idempotencyKeys.has(composite)) {
      return { status: "replayed", body: this.idempotencyKeys.get(composite) };
    }
    this.idempotencyKeys.set(composite, null);
    return null;
  }

  async finishIdempotent(scope: string, key: string, body: unknown): Promise<void> {
    this.idempotencyKeys.set(`${scope}|${key}`, body);
  }

  async releaseIdempotent(scope: string, key: string): Promise<void> {
    this.idempotencyKeys.delete(`${scope}|${key}`);
  }
}

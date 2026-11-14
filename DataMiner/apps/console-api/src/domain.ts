export type OperatorRole = "viewer" | "operator" | "admin";
export type OperatorStatus = "invited" | "active" | "suspended" | "tombstoned";
export type ChallengePurpose = "login_otp" | "invite" | "password_reset";

export type OperatorRecord = {
  id: string;
  email: string;
  passwordHash: string | null;
  role: OperatorRole;
  status: OperatorStatus;
  failedLoginCount: number;
  lockedUntil: string | null;
  // Every deadline is decided by the database clock. The service never compares
  // a stored timestamp against its own wall clock.
  locked: boolean;
  passwordUpdatedAt: string | null;
  lastLoginAt: string | null;
  createdAt: string;
  updatedAt: string;
};

export type ChallengeRecord = {
  id: string;
  operatorId: string;
  purpose: ChallengePurpose;
  codeSalt: Buffer;
  codeHash: string;
  expiresAt: string;
  attemptCount: number;
};

export type SessionRecord = {
  id: string;
  operatorId: string;
  sudoUntil: string | null;
  // Decided by the database clock, like every other deadline in this service.
  sudoActive: boolean;
  expiresAt: string;
  lastSeenAt: string;
  createdAt: string;
};

export type AuthenticatedSession = {
  session: SessionRecord;
  operator: OperatorRecord;
};

export type SessionLookup =
  | ({ outcome: "active" } & AuthenticatedSession)
  | { outcome: "idle"; sessionId: string }
  | { outcome: "missing" };

export type RateLimitVerdict = {
  allowed: boolean;
  retryAfterSeconds: number;
};

export type AuditEvent = {
  actorType: "operator" | "service";
  actorId: string | null;
  action: string;
  entityType: string;
  entityId: string;
  requestId?: string;
  details?: Record<string, unknown>;
};

export type CreateOperatorInput = {
  email: string;
  role: OperatorRole;
  createdBy: string | null;
};

export type ChallengeIssueInput = {
  operatorId: string;
  purpose: ChallengePurpose;
  codeSalt: Buffer;
  codeHash: string;
  ttlSeconds: number;
};

export type SessionIssueInput = {
  operatorId: string;
  tokenHash: string;
  absoluteSeconds: number;
  ipHash: string | null;
  userAgent: string | null;
};

export interface ConsoleRepository {
  ping(): Promise<void>;
  /** Names of `table.PRIVILEGE` pairs the console pool needs but does not hold.
   *  Empty means the canary set is satisfied. See the implementation for why it
   *  is a canary and not the authority. */
  missingConsoleGrants(): Promise<string[]>;
  close(): Promise<void>;

  findOperatorByEmail(email: string): Promise<OperatorRecord | null>;
  findOperatorById(id: string): Promise<OperatorRecord | null>;
  listOperators(): Promise<OperatorRecord[]>;
  createOperator(input: CreateOperatorInput): Promise<OperatorRecord>;
  setOperatorRole(operatorId: string, role: OperatorRole): Promise<OperatorRecord>;
  setOperatorStatus(operatorId: string, status: OperatorStatus): Promise<OperatorRecord>;
  activateOperatorWithPassword(operatorId: string, passwordHash: string): Promise<void>;
  setOperatorPassword(operatorId: string, passwordHash: string): Promise<void>;

  registerLoginFailure(
    operatorId: string,
    policy: { threshold: number; baseMinutes: number; maxMinutes: number },
  ): Promise<void>;
  clearLoginFailures(operatorId: string): Promise<void>;
  markOperatorLoggedIn(operatorId: string): Promise<void>;

  issueChallenge(input: ChallengeIssueInput): Promise<ChallengeRecord>;
  supersedeOpenChallenges(operatorId: string, purpose: ChallengePurpose): Promise<void>;
  findOpenChallenge(challengeId: string): Promise<ChallengeRecord | null>;
  claimChallengeAttempt(challengeId: string, maxAttempts: number): Promise<ChallengeRecord | null>;
  consumeChallenge(challengeId: string): Promise<boolean>;

  issueSession(input: SessionIssueInput): Promise<SessionRecord>;
  findSessionByTokenHash(tokenHash: string, idleSeconds: number): Promise<SessionLookup>;
  touchSession(sessionId: string): Promise<void>;
  grantSessionSudo(sessionId: string, sudoSeconds: number): Promise<void>;
  revokeSession(sessionId: string, reason: string): Promise<void>;
  revokeOperatorSessions(operatorId: string, reason: string): Promise<number>;

  consumeRateLimit(bucket: string, windowSeconds: number, limit: number): Promise<RateLimitVerdict>;

  recordAudit(event: AuditEvent): Promise<void>;
}

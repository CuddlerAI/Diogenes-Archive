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
import { notFound } from "./errors.js";
import type { SqlClient } from "./sql.js";

type OperatorRow = {
  id: string;
  email: string;
  password_hash: string | null;
  role: OperatorRole;
  status: OperatorStatus;
  failed_login_count: number;
  locked_until: Date | null;
  locked: boolean;
  password_updated_at: Date | null;
  last_login_at: Date | null;
  created_at: Date;
  updated_at: Date;
};

type ChallengeRow = {
  id: string;
  operator_id: string;
  purpose: ChallengePurpose;
  code_salt: Buffer;
  code_hash: string;
  expires_at: Date;
  attempt_count: number;
};

type SessionRow = {
  id: string;
  operator_id: string;
  sudo_until: Date | null;
  sudo_active: boolean;
  expires_at: Date;
  last_seen_at: Date;
  created_at: Date;
};

type SessionWithOperatorRow = SessionRow & {
  is_idle: boolean;
  o_id: string;
  o_email: string;
  o_password_hash: string | null;
  o_role: OperatorRole;
  o_status: OperatorStatus;
  o_failed_login_count: number;
  o_locked_until: Date | null;
  o_locked: boolean;
  o_password_updated_at: Date | null;
  o_last_login_at: Date | null;
  o_created_at: Date;
  o_updated_at: Date;
};

const operatorColumns = `id, email, password_hash, role, status, failed_login_count,
       locked_until,
       (locked_until IS NOT NULL AND locked_until > transaction_timestamp()) AS locked,
       password_updated_at, last_login_at, created_at, updated_at`;

function toOperator(row: OperatorRow): OperatorRecord {
  return {
    id: row.id,
    email: row.email,
    passwordHash: row.password_hash,
    role: row.role,
    status: row.status,
    failedLoginCount: row.failed_login_count,
    lockedUntil: row.locked_until?.toISOString() ?? null,
    locked: row.locked,
    passwordUpdatedAt: row.password_updated_at?.toISOString() ?? null,
    lastLoginAt: row.last_login_at?.toISOString() ?? null,
    createdAt: row.created_at.toISOString(),
    updatedAt: row.updated_at.toISOString(),
  };
}

function toChallenge(row: ChallengeRow): ChallengeRecord {
  return {
    id: row.id,
    operatorId: row.operator_id,
    purpose: row.purpose,
    codeSalt: row.code_salt,
    codeHash: row.code_hash,
    expiresAt: row.expires_at.toISOString(),
    attemptCount: row.attempt_count,
  };
}

function toSession(row: SessionRow): SessionRecord {
  return {
    id: row.id,
    operatorId: row.operator_id,
    sudoUntil: row.sudo_until?.toISOString() ?? null,
    sudoActive: row.sudo_active,
    expiresAt: row.expires_at.toISOString(),
    lastSeenAt: row.last_seen_at.toISOString(),
    createdAt: row.created_at.toISOString(),
  };
}

export class PostgresConsoleRepository implements ConsoleRepository {
  readonly #pool: SqlClient;
  readonly #onClose: (() => Promise<void>) | null;

  constructor(pool: SqlClient, onClose: (() => Promise<void>) | null = null) {
    this.#pool = pool;
    this.#onClose = onClose;
  }

  async close(): Promise<void> {
    await this.#onClose?.();
  }

  async ping(): Promise<void> {
    await this.#pool.query("SELECT 1");
  }

  /**
   * Deploy-time canary for the failure that broke production on 2026-07-28: the
   * migrations ran, the image shipped, the grants step did not, and two console
   * routes 500'd on bare tables until a human happened to click Review.
   *
   * Deliberately NOT exhaustive, and not the authority — that is
   * scripts/grant-console-roles.sql, whose completeness assertion CI runs
   * against real PostgreSQL. This is the subset whose absence is invisible until
   * someone uses the feature, so /readyz can say it out loud at deploy time.
   * Pure catalog lookups; no table is read.
   */
  async missingConsoleGrants(): Promise<string[]> {
    const required: ReadonlyArray<readonly [string, string]> = [
      // Invite administration writes through the governance pool. SELECT alone
      // kept the list green while issue/revoke failed 42501 in production.
      ["invite_codes", "SELECT"],
      ["invite_codes", "INSERT"],
      ["invite_codes", "UPDATE"],
      ["review_verdicts", "SELECT"],
      ["review_verdicts", "INSERT"],
      ["hf_release_items", "SELECT"],
      ["hf_release_items", "DELETE"],
      ["hf_release_checks", "SELECT"],
      ["hf_manual_marks", "SELECT"],
      // Consent revocation erases telemetry in the same statement (docs/11 §6.4);
      // without UPDATE the erase fails and the revoke takes the whole request with it.
      ["client_events", "UPDATE"],
    ];
    const result = await this.#pool.query<{ missing: string }>(
      `SELECT t.tbl || '.' || t.priv AS missing
         FROM unnest($1::text[], $2::text[]) AS t(tbl, priv)
        WHERE to_regclass('public.' || t.tbl) IS NULL
           OR NOT has_table_privilege(current_user, 'public.' || t.tbl, t.priv)
        ORDER BY 1`,
      [required.map(([table]) => table), required.map(([, privilege]) => privilege)],
    );
    return result.rows.map((row) => row.missing);
  }

  async findOperatorByEmail(email: string): Promise<OperatorRecord | null> {
    const result = await this.#pool.query<OperatorRow>(
      `SELECT ${operatorColumns} FROM operators WHERE email = $1`,
      [email.toLowerCase()],
    );
    const row = result.rows[0];
    return row ? toOperator(row) : null;
  }

  async findOperatorById(id: string): Promise<OperatorRecord | null> {
    const result = await this.#pool.query<OperatorRow>(
      `SELECT ${operatorColumns} FROM operators WHERE id = $1`,
      [id],
    );
    const row = result.rows[0];
    return row ? toOperator(row) : null;
  }

  async listOperators(): Promise<OperatorRecord[]> {
    const result = await this.#pool.query<OperatorRow>(
      `SELECT ${operatorColumns} FROM operators
        WHERE status <> 'tombstoned'
        ORDER BY created_at DESC, id DESC`,
    );
    return result.rows.map(toOperator);
  }

  async createOperator(input: CreateOperatorInput): Promise<OperatorRecord> {
    const result = await this.#pool.query<OperatorRow>(
      `INSERT INTO operators(id, email, role, status, created_by)
       VALUES ($1, $2, $3, 'invited', $4)
       RETURNING ${operatorColumns}`,
      [randomUUID(), input.email.toLowerCase(), input.role, input.createdBy],
    );
    return toOperator(this.#required(result.rows[0], "operator_not_created"));
  }

  async setOperatorRole(operatorId: string, role: OperatorRole): Promise<OperatorRecord> {
    const result = await this.#pool.query<OperatorRow>(
      `UPDATE operators SET role = $2 WHERE id = $1 RETURNING ${operatorColumns}`,
      [operatorId, role],
    );
    return toOperator(this.#required(result.rows[0], "operator_not_found"));
  }

  async setOperatorStatus(operatorId: string, status: OperatorStatus): Promise<OperatorRecord> {
    const result = await this.#pool.query<OperatorRow>(
      `UPDATE operators
          SET status = $2,
              suspended_at = CASE WHEN $2 = 'suspended' THEN transaction_timestamp() ELSE suspended_at END,
              tombstoned_at = CASE WHEN $2 = 'tombstoned' THEN transaction_timestamp() ELSE tombstoned_at END,
              failed_login_count = CASE WHEN $2 = 'active' THEN 0 ELSE failed_login_count END,
              locked_until = CASE WHEN $2 = 'active' THEN NULL ELSE locked_until END
        WHERE id = $1
        RETURNING ${operatorColumns}`,
      [operatorId, status],
    );
    return toOperator(this.#required(result.rows[0], "operator_not_found"));
  }

  async activateOperatorWithPassword(operatorId: string, passwordHash: string): Promise<void> {
    const result = await this.#pool.query<{ id: string }>(
      `UPDATE operators
          SET password_hash = $2,
              password_updated_at = transaction_timestamp(),
              status = 'active',
              failed_login_count = 0,
              locked_until = NULL
        WHERE id = $1 AND status = 'invited'
        RETURNING id`,
      [operatorId, passwordHash],
    );
    if (result.rows.length !== 1) {
      throw notFound("operator_not_found", "no pending invitation for this operator");
    }
  }

  async setOperatorPassword(operatorId: string, passwordHash: string): Promise<void> {
    const result = await this.#pool.query<{ id: string }>(
      `UPDATE operators
          SET password_hash = $2,
              password_updated_at = transaction_timestamp(),
              failed_login_count = 0,
              locked_until = NULL
        WHERE id = $1 AND status = 'active'
        RETURNING id`,
      [operatorId, passwordHash],
    );
    if (result.rows.length !== 1) throw notFound("operator_not_found", "operator was not found");
  }

  async registerLoginFailure(
    operatorId: string,
    policy: { threshold: number; baseMinutes: number; maxMinutes: number },
  ): Promise<void> {
    // The lock doubles for every additional threshold reached, capped by policy,
    // and is computed in SQL so concurrent attempts cannot race the counter.
    //
    // The exponent is clamped before `power` sees it: `power(int, int)` resolves
    // to the double-precision overload regardless of the `::numeric` casts, so
    // an unclamped exponent overflows float8 after roughly ten thousand
    // failures — and from then on every failed login raises instead of locking.
    // 30 doublings is already far beyond the cap.
    await this.#pool.query(
      `UPDATE operators
          SET failed_login_count = failed_login_count + 1,
              locked_until = CASE
                WHEN (failed_login_count + 1) % $2 = 0 THEN transaction_timestamp() + make_interval(
                  mins => LEAST($4::numeric, $3::numeric * power(2, LEAST(GREATEST((failed_login_count + 1) / $2 - 1, 0), 30)))::int
                )
                ELSE locked_until
              END
        WHERE id = $1`,
      [operatorId, policy.threshold, policy.baseMinutes, policy.maxMinutes],
    );
  }

  async clearLoginFailures(operatorId: string): Promise<void> {
    await this.#pool.query(
      `UPDATE operators SET failed_login_count = 0, locked_until = NULL
        WHERE id = $1 AND (failed_login_count <> 0 OR locked_until IS NOT NULL)`,
      [operatorId],
    );
  }

  async markOperatorLoggedIn(operatorId: string): Promise<void> {
    await this.#pool.query(
      `UPDATE operators SET last_login_at = transaction_timestamp() WHERE id = $1`,
      [operatorId],
    );
  }

  async issueChallenge(input: ChallengeIssueInput): Promise<ChallengeRecord> {
    const result = await this.#pool.query<ChallengeRow>(
      `INSERT INTO operator_email_challenges(
           id, operator_id, purpose, code_salt, code_hash, expires_at)
       VALUES ($1, $2, $3, $4, $5, transaction_timestamp() + make_interval(secs => $6))
       RETURNING id, operator_id, purpose, code_salt, code_hash, expires_at, attempt_count`,
      [
        randomUUID(),
        input.operatorId,
        input.purpose,
        input.codeSalt,
        input.codeHash,
        input.ttlSeconds,
      ],
    );
    return toChallenge(this.#required(result.rows[0], "challenge_not_created"));
  }

  async supersedeOpenChallenges(operatorId: string, purpose: ChallengePurpose): Promise<void> {
    await this.#pool.query(
      `UPDATE operator_email_challenges
          SET superseded_at = transaction_timestamp()
        WHERE operator_id = $1 AND purpose = $2
          AND consumed_at IS NULL AND superseded_at IS NULL`,
      [operatorId, purpose],
    );
  }

  async findOpenChallenge(challengeId: string): Promise<ChallengeRecord | null> {
    const result = await this.#pool.query<ChallengeRow>(
      `SELECT id, operator_id, purpose, code_salt, code_hash, expires_at, attempt_count
         FROM operator_email_challenges
        WHERE id = $1
          AND consumed_at IS NULL
          AND superseded_at IS NULL
          AND expires_at > transaction_timestamp()`,
      [challengeId],
    );
    const row = result.rows[0];
    return row ? toChallenge(row) : null;
  }

  async claimChallengeAttempt(
    challengeId: string,
    maxAttempts: number,
  ): Promise<ChallengeRecord | null> {
    // One statement spends the attempt and returns the row, so parallel guesses
    // cannot share a single attempt budget.
    const result = await this.#pool.query<ChallengeRow>(
      `UPDATE operator_email_challenges
          SET attempt_count = attempt_count + 1
        WHERE id = $1
          AND consumed_at IS NULL
          AND superseded_at IS NULL
          AND expires_at > transaction_timestamp()
          AND attempt_count < $2
        RETURNING id, operator_id, purpose, code_salt, code_hash, expires_at, attempt_count`,
      [challengeId, maxAttempts],
    );
    const row = result.rows[0];
    return row ? toChallenge(row) : null;
  }

  async consumeChallenge(challengeId: string): Promise<boolean> {
    const result = await this.#pool.query<{ id: string }>(
      `UPDATE operator_email_challenges
          SET consumed_at = transaction_timestamp()
        WHERE id = $1 AND consumed_at IS NULL
        RETURNING id`,
      [challengeId],
    );
    return result.rows.length === 1;
  }

  async issueSession(input: SessionIssueInput): Promise<SessionRecord> {
    const result = await this.#pool.query<SessionRow>(
      `INSERT INTO operator_sessions(
           id, operator_id, token_hash, expires_at, ip_hash, user_agent)
       VALUES ($1, $2, $3, transaction_timestamp() + make_interval(secs => $4), $5, $6)
       RETURNING id, operator_id, sudo_until,
                 (sudo_until IS NOT NULL AND sudo_until > transaction_timestamp()) AS sudo_active,
                 expires_at, last_seen_at, created_at`,
      [
        randomUUID(),
        input.operatorId,
        input.tokenHash,
        input.absoluteSeconds,
        input.ipHash,
        input.userAgent,
      ],
    );
    return toSession(this.#required(result.rows[0], "session_not_created"));
  }

  async findSessionByTokenHash(tokenHash: string, idleSeconds: number): Promise<SessionLookup> {
    // The absolute deadline, the idle deadline, and the lock state are all
    // evaluated against the database clock in this one statement.
    const result = await this.#pool.query<SessionWithOperatorRow>(
      `SELECT s.id, s.operator_id, s.sudo_until, s.expires_at, s.last_seen_at, s.created_at,
              (s.sudo_until IS NOT NULL AND s.sudo_until > transaction_timestamp()) AS sudo_active,
              (s.last_seen_at <= transaction_timestamp() - make_interval(secs => $2)) AS is_idle,
              o.id AS o_id, o.email AS o_email, o.password_hash AS o_password_hash,
              o.role AS o_role, o.status AS o_status,
              o.failed_login_count AS o_failed_login_count, o.locked_until AS o_locked_until,
              (o.locked_until IS NOT NULL AND o.locked_until > transaction_timestamp()) AS o_locked,
              o.password_updated_at AS o_password_updated_at, o.last_login_at AS o_last_login_at,
              o.created_at AS o_created_at, o.updated_at AS o_updated_at
         FROM operator_sessions s
         JOIN operators o ON o.id = s.operator_id
        WHERE s.token_hash = $1
          AND s.revoked_at IS NULL
          AND s.expires_at > transaction_timestamp()`,
      [tokenHash, idleSeconds],
    );
    const row = result.rows[0];
    if (!row) return { outcome: "missing" };
    if (row.is_idle) return { outcome: "idle", sessionId: row.id };
    return {
      outcome: "active",
      session: toSession(row),
      operator: toOperator({
        id: row.o_id,
        email: row.o_email,
        password_hash: row.o_password_hash,
        role: row.o_role,
        status: row.o_status,
        failed_login_count: row.o_failed_login_count,
        locked_until: row.o_locked_until,
        locked: row.o_locked,
        password_updated_at: row.o_password_updated_at,
        last_login_at: row.o_last_login_at,
        created_at: row.o_created_at,
        updated_at: row.o_updated_at,
      }),
    };
  }

  async touchSession(sessionId: string): Promise<void> {
    await this.#pool.query(
      `UPDATE operator_sessions SET last_seen_at = transaction_timestamp()
        WHERE id = $1 AND revoked_at IS NULL`,
      [sessionId],
    );
  }

  async grantSessionSudo(sessionId: string, sudoSeconds: number): Promise<void> {
    await this.#pool.query(
      `UPDATE operator_sessions
          SET sudo_until = transaction_timestamp() + make_interval(secs => $2)
        WHERE id = $1 AND revoked_at IS NULL`,
      [sessionId, sudoSeconds],
    );
  }

  async revokeSession(sessionId: string, reason: string): Promise<void> {
    await this.#pool.query(
      `UPDATE operator_sessions
          SET revoked_at = transaction_timestamp(), revoke_reason = $2
        WHERE id = $1 AND revoked_at IS NULL`,
      [sessionId, reason],
    );
  }

  async revokeOperatorSessions(operatorId: string, reason: string): Promise<number> {
    const result = await this.#pool.query<{ id: string }>(
      `UPDATE operator_sessions
          SET revoked_at = transaction_timestamp(), revoke_reason = $2
        WHERE operator_id = $1 AND revoked_at IS NULL
        RETURNING id`,
      [operatorId, reason],
    );
    return result.rows.length;
  }

  async consumeRateLimit(
    bucket: string,
    windowSeconds: number,
    limit: number,
  ): Promise<RateLimitVerdict> {
    // A single upsert both rolls the window and increments, so instances of the
    // service share one counter without a lock round trip.
    const result = await this.#pool.query<{ counter: number; window_started_at: Date }>(
      `INSERT INTO console_rate_limits(bucket, window_started_at, counter)
       VALUES ($1, transaction_timestamp(), 1)
       ON CONFLICT (bucket) DO UPDATE
          SET window_started_at = CASE
                WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                     <= transaction_timestamp()
                THEN transaction_timestamp()
                ELSE console_rate_limits.window_started_at
              END,
              counter = CASE
                WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                     <= transaction_timestamp()
                THEN 1
                ELSE console_rate_limits.counter + 1
              END,
              updated_at = transaction_timestamp()
       RETURNING counter, window_started_at`,
      [bucket, windowSeconds],
    );
    const row = this.#required(result.rows[0], "rate_limit_not_recorded");
    const elapsedSeconds = Math.max(
      0,
      Math.floor((Date.now() - row.window_started_at.getTime()) / 1000),
    );
    return {
      allowed: row.counter <= limit,
      retryAfterSeconds: Math.max(1, windowSeconds - elapsedSeconds),
    };
  }

  async recordAudit(event: AuditEvent): Promise<void> {
    await this.#pool.query(
      `INSERT INTO audit_events(
           event_id, request_id, actor_type, actor_id, action, entity_type, entity_id, details)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8)`,
      [
        randomUUID(),
        event.requestId ?? null,
        event.actorType,
        event.actorId,
        event.action,
        event.entityType,
        event.entityId,
        event.details ?? {},
      ],
    );
  }

  #required<T>(row: T | undefined, code: string): T {
    if (!row) throw notFound(code, "the expected row was not returned");
    return row;
  }
}

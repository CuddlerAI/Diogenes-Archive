import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { AuthService, type RequestContext } from "./auth.js";
import { PostgresBrowseRepository } from "./browse-repository.js";
import { PostgresGovernanceRepository } from "./governance-repository.js";
import { PreviewService } from "./preview.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import { buildApp } from "./app.js";
import { PostgresConsoleRepository } from "./repository.js";
import { hashChallengeCode, hashPassword, hashSessionToken } from "./secrets.js";
import type { SqlClient } from "./sql.js";
import { argon2TestTimeoutMs, MemoryMailer } from "./testing.js";

// The end-to-end sign-in cases run the real password verification. See
// argon2TestTimeoutMs for why the budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

// The auth statements carry the guards that keep a code single use and a session
// revocable. These run them on a real PostgreSQL engine rather than on the
// in-memory stand-in, so the guards are executed, not described.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const password = "a sufficiently long console password";
let passwordHash = "";
let db: PGlite;
let repository: PostgresConsoleRepository;

const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "q".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
  // The end-to-end sign-in test drives password → OTP over HTTP, so this stays on
  // the two-step flow even though the config default is now password_only.
  CONSOLE_LOGIN_MODE: "password_otp",
});

const context: RequestContext = {
  requestId: "req_repository_test",
  ipHash: "a".repeat(64),
  userAgent: "vitest",
};

async function createActiveOperator(role: "viewer" | "operator" | "admin" = "admin") {
  const id = randomUUID();
  await db.query(
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, $2, $3, transaction_timestamp(), $4, 'active')`,
    [id, `ops-${id.slice(0, 8)}@diogenesmodel.com`, passwordHash, role],
  );
  const operator = await repository.findOperatorById(id);
  if (!operator) throw new Error("operator was not created");
  return operator;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  repository = new PostgresConsoleRepository(db as unknown as SqlClient);
  passwordHash = await hashPassword(password);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

beforeEach(async () => {
  await db.query(`DELETE FROM console_rate_limits`);
});

describe("operator statements", () => {
  it("stores and reads an operator with the lock derived from the database clock", async () => {
    const operator = await createActiveOperator();

    expect(operator.locked).toBe(false);
    await repository.registerLoginFailure(operator.id, {
      threshold: 1,
      baseMinutes: 15,
      maxMinutes: 240,
    });

    const locked = await repository.findOperatorById(operator.id);
    expect(locked?.locked).toBe(true);
    expect(locked?.failedLoginCount).toBe(1);
  });

  it("clears the lock when the failures are cleared", async () => {
    const operator = await createActiveOperator();
    await repository.registerLoginFailure(operator.id, {
      threshold: 1,
      baseMinutes: 15,
      maxMinutes: 240,
    });

    await repository.clearLoginFailures(operator.id);

    expect((await repository.findOperatorById(operator.id))?.locked).toBe(false);
  });

  it("refuses to set a password on an operator that never accepted an invitation", async () => {
    const id = randomUUID();
    await db.query(`INSERT INTO operators(id, email) VALUES ($1, $2)`, [
      id,
      `invited-${id.slice(0, 8)}@diogenesmodel.com`,
    ]);

    await expect(repository.setOperatorPassword(id, passwordHash)).rejects.toMatchObject({
      code: "operator_not_found",
    });
    await expect(repository.activateOperatorWithPassword(id, passwordHash)).resolves.toBeUndefined();
    await expect(repository.activateOperatorWithPassword(id, passwordHash)).rejects.toMatchObject({
      code: "operator_not_found",
    });
  });
});

describe("challenge statements", () => {
  it("hands out one attempt per claim and refuses a superseded challenge", async () => {
    const operator = await createActiveOperator();
    const salt = Buffer.alloc(32, 3);
    const first = await repository.issueChallenge({
      operatorId: operator.id,
      purpose: "login_otp",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, "123456"),
      ttlSeconds: 600,
    });

    expect((await repository.claimChallengeAttempt(first.id, 5))?.attemptCount).toBe(1);
    await repository.supersedeOpenChallenges(operator.id, "login_otp");

    expect(await repository.claimChallengeAttempt(first.id, 5)).toBeNull();
    expect(await repository.findOpenChallenge(first.id)).toBeNull();
  });

  it("round-trips the salt so a code hashes to the stored digest", async () => {
    const operator = await createActiveOperator();
    const salt = Buffer.from(randomUUID().replace(/-/g, "") + randomUUID().replace(/-/g, ""), "hex");
    const stored = hashChallengeCode(salt, "424242");
    const challenge = await repository.issueChallenge({
      operatorId: operator.id,
      purpose: "login_otp",
      codeSalt: salt,
      codeHash: stored,
      ttlSeconds: 600,
    });

    const claimed = await repository.claimChallengeAttempt(challenge.id, 5);

    expect(claimed).not.toBeNull();
    expect(hashChallengeCode(claimed!.codeSalt, "424242")).toBe(stored);
  });

  it("consumes a challenge exactly once", async () => {
    const operator = await createActiveOperator();
    const salt = Buffer.alloc(32, 5);
    const challenge = await repository.issueChallenge({
      operatorId: operator.id,
      purpose: "login_otp",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, "999999"),
      ttlSeconds: 600,
    });

    expect(await repository.consumeChallenge(challenge.id)).toBe(true);
    expect(await repository.consumeChallenge(challenge.id)).toBe(false);
  });

  it("stops claiming an expired challenge", async () => {
    const operator = await createActiveOperator();
    const salt = Buffer.alloc(32, 6);
    const challenge = await repository.issueChallenge({
      operatorId: operator.id,
      purpose: "login_otp",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, "111111"),
      ttlSeconds: 600,
    });
    // created_at moves with expires_at: the table refuses a row that expires
    // before it was created, so an expired fixture has to stay internally valid.
    await db.query(
      `UPDATE operator_email_challenges
          SET created_at = transaction_timestamp() - interval '1 hour',
              expires_at = transaction_timestamp() - interval '1 second'
        WHERE id = $1`,
      [challenge.id],
    );

    expect(await repository.claimChallengeAttempt(challenge.id, 5)).toBeNull();
  });
});

describe("session statements", () => {
  it("finds an active session and reports an idle one separately", async () => {
    const operator = await createActiveOperator();
    const tokenHash = hashSessionToken("token-a");
    const session = await repository.issueSession({
      operatorId: operator.id,
      tokenHash,
      absoluteSeconds: 3_600,
      ipHash: "b".repeat(64),
      userAgent: "vitest",
    });

    expect(await repository.findSessionByTokenHash(tokenHash, 3_600)).toMatchObject({
      outcome: "active",
    });

    await db.query(
      `UPDATE operator_sessions SET last_seen_at = transaction_timestamp() - interval '2 hours'
        WHERE id = $1`,
      [session.id],
    );

    expect(await repository.findSessionByTokenHash(tokenHash, 3_600)).toMatchObject({
      outcome: "idle",
      sessionId: session.id,
    });
  });

  it("treats an expired or revoked session as missing", async () => {
    const operator = await createActiveOperator();
    const expiredHash = hashSessionToken("token-expired");
    const revokedHash = hashSessionToken("token-revoked");
    const expired = await repository.issueSession({
      operatorId: operator.id,
      tokenHash: expiredHash,
      absoluteSeconds: 3_600,
      ipHash: null,
      userAgent: null,
    });
    const revoked = await repository.issueSession({
      operatorId: operator.id,
      tokenHash: revokedHash,
      absoluteSeconds: 3_600,
      ipHash: null,
      userAgent: null,
    });
    await db.query(
      `UPDATE operator_sessions
          SET created_at = transaction_timestamp() - interval '3 hours',
              expires_at = transaction_timestamp() - interval '1 second'
        WHERE id = $1`,
      [expired.id],
    );
    await repository.revokeSession(revoked.id, "logout");

    expect(await repository.findSessionByTokenHash(expiredHash, 3_600)).toMatchObject({
      outcome: "missing",
    });
    expect(await repository.findSessionByTokenHash(revokedHash, 3_600)).toMatchObject({
      outcome: "missing",
    });
  });

  it("revokes every live session of an operator and reports how many", async () => {
    const operator = await createActiveOperator();
    for (const token of ["one", "two", "three"]) {
      await repository.issueSession({
        operatorId: operator.id,
        tokenHash: hashSessionToken(`${operator.id}-${token}`),
        absoluteSeconds: 3_600,
        ipHash: null,
        userAgent: null,
      });
    }

    expect(await repository.revokeOperatorSessions(operator.id, "password_changed")).toBe(3);
    expect(await repository.revokeOperatorSessions(operator.id, "password_changed")).toBe(0);
  });
});

describe("rate limit statements", () => {
  it("counts within a window and resets once the window has elapsed", async () => {
    const bucket = `otp_min:${randomUUID()}`;

    const first = await repository.consumeRateLimit(bucket, 3_600, 2);
    const second = await repository.consumeRateLimit(bucket, 3_600, 2);
    const third = await repository.consumeRateLimit(bucket, 3_600, 2);
    const afterWindow = await repository.consumeRateLimit(bucket, 0, 2);

    expect(first.allowed).toBe(true);
    expect(second.allowed).toBe(true);
    expect(third.allowed).toBe(false);
    expect(third.retryAfterSeconds).toBeGreaterThan(0);
    expect(afterWindow.allowed).toBe(true);
  });
});

describe("sign-in against a real database", () => {
  it("completes password, code, session, and audit end to end", async () => {
    const operator = await createActiveOperator();
    const mailer = new MemoryMailer();
    const auth = new AuthService(repository, mailer, config);

    const login = await auth.login(operator.email, password, context);
    const grant = await auth.verifyOtp(login.challengeId, mailer.lastCode(), context);
    const authenticated = await auth.authenticate(grant.token);

    expect(authenticated?.operator.id).toBe(operator.id);
    const audit = await db.query<{ action: string }>(
      `SELECT action FROM audit_events WHERE actor_id = $1 ORDER BY id`,
      [operator.id],
    );
    expect(audit.rows.map((row) => row.action)).toEqual([
      "operator.otp_sent",
      "operator.session_created",
    ]);
  });

  it("refuses to reuse a code and leaves the session unaffected", async () => {
    const operator = await createActiveOperator();
    const mailer = new MemoryMailer();
    const auth = new AuthService(repository, mailer, config);
    const login = await auth.login(operator.email, password, context);
    const code = mailer.lastCode();
    await auth.verifyOtp(login.challengeId, code, context);

    await expect(auth.verifyOtp(login.challengeId, code, context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
  });

  it("serves the browsing routes over a real session cookie", async () => {
    const operator = await createActiveOperator();
    const mailer = new MemoryMailer();
    const browse = new PostgresBrowseRepository(db as unknown as SqlClient);
    const governance = new PostgresGovernanceRepository(db as unknown as SqlClient);
    const preview = new PreviewService(db as unknown as SqlClient, null, config);
    const app = buildApp({ config, repository, browse, governance, preview, mailer });

    const login = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: { "content-type": "application/json", "x-dmc-csrf": "1" },
      payload: { email: operator.email, password },
    });
    const verify = await app.inject({
      method: "POST",
      url: "/console/v1/auth/otp/verify",
      headers: { "content-type": "application/json", "x-dmc-csrf": "1" },
      payload: { challengeId: login.json().challengeId, code: mailer.lastCode() },
    });
    const setCookie = String(verify.headers["set-cookie"]);
    const cookie = setCookie.slice(0, setCookie.indexOf(";"));

    const overview = await app.inject({
      method: "GET",
      url: "/console/v1/stats/overview",
      headers: { cookie },
    });
    const clips = await app.inject({
      method: "GET",
      url: "/console/v1/clips?annotatorId=none",
      headers: { cookie },
    });

    expect(verify.statusCode).toBe(200);
    expect(overview.statusCode).toBe(200);
    expect(overview.json()).toMatchObject({ trainableClips: 0 });
    expect(clips.statusCode).toBe(200);
    await app.close();
  });
});

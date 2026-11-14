/**
 * Runs the console's own statements against a real PostgreSQL server.
 *
 * The test suite uses an embedded engine so CI needs no service container. This
 * script is the counterpart: it exercises the same paths through the actual `pg`
 * driver against a real server, which is where driver-specific behaviour (result
 * metadata, type coercion, parameter inference) would diverge.
 *
 * It WRITES to the database it points at and deletes what it created. Point it at
 * a scratch database or a freshly migrated staging one, never at anything holding
 * real capture data.
 *
 *   DATABASE_URL=postgres://... \
 *   CONSOLE_PUBLIC_ORIGIN=http://localhost:5174 \
 *   CONSOLE_IP_HASH_SALT=<32+ bytes> \
 *   CONSOLE_ENV=development CONSOLE_MAIL_MODE=capture \
 *   pnpm --filter @dataminer/console-api verify:postgres
 */
import { randomUUID } from "node:crypto";

import { AuthService, type RequestContext } from "../src/auth.js";
import { PostgresBrowseRepository } from "../src/browse-repository.js";
import { loadConfig } from "../src/config.js";
import type { AuthenticatedSession } from "../src/domain.js";
import { PostgresGovernanceRepository } from "../src/governance-repository.js";
import { GovernanceService } from "../src/governance.js";
import { PostgresConsoleRepository } from "../src/repository.js";
import { hashPassword, hashSessionToken } from "../src/secrets.js";
import { createPool } from "../src/sql.js";
import { MemoryMailer } from "../src/testing.js";

const config = loadConfig();
const pool = createPool(config.databaseUrl, 4);
const repository = new PostgresConsoleRepository(pool, () => pool.end());
const browse = new PostgresBrowseRepository(pool);
const governance = new GovernanceService(
  new PostgresGovernanceRepository(pool),
  repository,
  config,
);
const mailer = new MemoryMailer();
const auth = new AuthService(repository, mailer, config);
const context: RequestContext = {
  requestId: "req_verify_postgres",
  ipHash: "c".repeat(64),
  userAgent: "verify-against-postgres",
};

const results: { name: string; ok: boolean; detail?: string }[] = [];
const createdOperators: string[] = [];
const createdAnnotators: string[] = [];
// The auth flow opens rate-limit buckets of its own beyond the one this script
// creates directly, and their keys are hashed, so cleanup diffs the bucket set
// rather than trying to reconstruct the names.
const bucketsBefore = new Set<string>(
  (await pool.query<{ bucket: string }>(`SELECT bucket FROM console_rate_limits`)).rows.map(
    (row) => row.bucket,
  ),
);

function check(name: string, ok: boolean, detail?: string) {
  results.push(detail === undefined ? { name, ok } : { name, ok, detail });
}

async function seedOperator(role: "viewer" | "admin"): Promise<string> {
  const id = randomUUID();
  await pool.query(
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, $2, $3, transaction_timestamp(), $4, 'active')`,
    [id, `verify-${id.slice(0, 8)}@diogenesmodel.invalid`, passwordHash, role],
  );
  createdOperators.push(id);
  return id;
}

const password = `verify-${randomUUID()}`;
const passwordHash = await hashPassword(password);

try {
  const adminId = await seedOperator("admin");
  const admin = await repository.findOperatorById(adminId);
  if (!admin) throw new Error("seeded operator was not readable");

  const login = await auth.login(admin.email, password, context);
  const grant = await auth.verifyOtp(login.challengeId, mailer.lastCode(), context);
  const authenticated = await auth.authenticate(grant.token);
  check("sign-in issues a usable session", authenticated?.operator.id === adminId);

  const digestRows = await pool.query<{ n: number }>(
    `SELECT count(*)::int AS n FROM operator_sessions WHERE token_hash = $1`,
    [hashSessionToken(grant.token)],
  );
  check("session token is stored only as a digest", digestRows.rows[0]?.n === 1);

  let replayRefused = false;
  try {
    await auth.verifyOtp(login.challengeId, mailer.lastCode(), context);
  } catch {
    replayRefused = true;
  }
  check("a verification code cannot be replayed", replayRefused);

  const bucket = `verify:${randomUUID()}`;
  const first = await repository.consumeRateLimit(bucket, 3_600, 2);
  const second = await repository.consumeRateLimit(bucket, 3_600, 2);
  const third = await repository.consumeRateLimit(bucket, 3_600, 2);
  check(
    "the rate limit counts within a window then refuses",
    first.allowed && second.allowed && !third.allowed,
  );

  const lockTarget = await seedOperator("viewer");
  for (let attempt = 0; attempt < 10; attempt += 1) {
    await repository.registerLoginFailure(lockTarget, {
      threshold: 10,
      baseMinutes: 15,
      maxMinutes: 240,
    });
  }
  check(
    "reaching the threshold locks the account",
    (await repository.findOperatorById(lockTarget))?.locked === true,
  );

  const actor: AuthenticatedSession = {
    operator: authenticated!.operator,
    session: { ...authenticated!.session, sudoActive: true },
  };

  const annotator = await governance.createAnnotator(
    actor,
    `verify-${randomUUID()}`,
    { alias: `verify-${randomUUID().slice(0, 6)}` },
    context,
  );
  createdAnnotators.push(annotator.id);

  let trainableRefused = false;
  try {
    await governance.createConsent(
      actor,
      `verify-${randomUUID()}`,
      {
        annotatorId: annotator.id,
        policyVersion: "verify",
        purpose: "verification run",
        termsHash: "a".repeat(64),
        acceptedAt: new Date().toISOString(),
        allowsTrainableUse: true,
      },
      context,
    );
  } catch (error) {
    trainableRefused = (error as { code?: string }).code === "policy_blocked";
  }
  check("a trainable consent is refused", trainableRefused);

  const replayKey = `verify-${randomUUID()}`;
  const alias = `verify-dup-${randomUUID().slice(0, 6)}`;
  const firstWrite = await governance.createAnnotator(actor, replayKey, { alias }, context);
  const replayed = await governance.createAnnotator(actor, replayKey, { alias }, context);
  createdAnnotators.push(firstWrite.id);
  check("an idempotent replay returns the first row", firstWrite.id === replayed.id);
  const duplicateRows = await pool.query<{ n: number }>(
    `SELECT count(*)::int AS n FROM annotators WHERE alias = $1`,
    [alias],
  );
  check("the replay wrote exactly one row", duplicateRows.rows[0]?.n === 1);

  let auditProtected = false;
  try {
    await pool.query(`UPDATE audit_events SET action = 'tampered' WHERE actor_id = $1`, [adminId]);
  } catch {
    auditProtected = true;
  }
  check("the audit trail rejects updates", auditProtected);

  const overview = await browse.pipelineOverview();
  const health = await browse.pipelineHealth(7);
  const unattached = await browse.listClips({ annotatorId: "none", pageSize: 10 });
  check("the pipeline overview query runs", overview.trainableClips === 0);
  check("the pipeline health query runs", health.storageByDay.length === 7);
  check("the unattached clip filter runs", Array.isArray(unattached.items));
} finally {
  // Audit rows are append-only by design and are deliberately left behind. Every
  // other row this script created is removed, including from the two governance
  // tables whose delete triggers exist to stop exactly this in normal operation —
  // which is why this script must never point at a database holding real data.
  //
  // Nothing here swallows an error: a cleanup that fails silently would let the
  // script claim it left no trace while it did.
  await pool.query(`ALTER TABLE annotators DISABLE TRIGGER annotators_reject_delete`);
  await pool.query(`ALTER TABLE operators DISABLE TRIGGER operators_reject_delete`);
  try {
    for (const id of createdAnnotators) {
      await pool.query(`DELETE FROM consents WHERE annotator_id = $1`, [id]);
      await pool.query(`DELETE FROM annotators WHERE id = $1`, [id]);
    }
    for (const id of createdOperators) {
      await pool.query(`DELETE FROM operator_sessions WHERE operator_id = $1`, [id]);
      await pool.query(`DELETE FROM operator_email_challenges WHERE operator_id = $1`, [id]);
      await pool.query(`DELETE FROM operators WHERE id = $1`, [id]);
    }
    const bucketsAfter = await pool.query<{ bucket: string }>(
      `SELECT bucket FROM console_rate_limits`,
    );
    const opened = bucketsAfter.rows
      .map((row) => row.bucket)
      .filter((bucket) => !bucketsBefore.has(bucket));
    if (opened.length > 0) {
      await pool.query(`DELETE FROM console_rate_limits WHERE bucket = ANY($1::text[])`, [opened]);
    }
  } finally {
    await pool.query(`ALTER TABLE operators ENABLE TRIGGER operators_reject_delete`);
    await pool.query(`ALTER TABLE annotators ENABLE TRIGGER annotators_reject_delete`);
  }
  await repository.close();
}

const failures = results.filter((result) => !result.ok);
for (const result of results) {
  process.stdout.write(
    `${result.ok ? "PASS" : "FAIL"}  ${result.name}${result.detail ? ` (${result.detail})` : ""}\n`,
  );
}
process.stdout.write(
  `\n${results.length - failures.length}/${results.length} checks passed against ${new URL(config.databaseUrl).host}\n`,
);
process.exit(failures.length === 0 ? 0 : 1);

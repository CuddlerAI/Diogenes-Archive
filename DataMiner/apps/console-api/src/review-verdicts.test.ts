import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import type { RequestContext } from "./auth.js";
import { loadConfig } from "./config.js";
import type { AuditEvent, AuthenticatedSession, ConsoleRepository } from "./domain.js";
import { PostgresGovernanceRepository } from "./governance-repository.js";
import { GovernanceService } from "./governance.js";
import type { SqlClient } from "./sql.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const config = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "v".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
});

const context: RequestContext = { requestId: "req_test", ipHash: "ip", userAgent: "vitest" };

const subjectKey =
  "raw/v1/non-trainable/manual/HOST/app/non-trainable-sessions/s1/runs/r0000/segments/00000/";

let db: PGlite;
let governance: GovernanceService;
let operatorId: string;
const audits: AuditEvent[] = [];

function session(role: "viewer" | "operator" | "admin"): AuthenticatedSession {
  return {
    session: {
      id: randomUUID(),
      operatorId,
      sudoUntil: null,
      sudoActive: false,
      expiresAt: new Date(Date.now() + 3_600_000).toISOString(),
      lastSeenAt: new Date().toISOString(),
      createdAt: new Date().toISOString(),
    },
    operator: {
      id: operatorId,
      email: "reviewer@example.com",
      passwordHash: null,
      role,
      status: "active",
      failedLoginCount: 0,
      lockedUntil: null,
      locked: false,
      passwordUpdatedAt: null,
      lastLoginAt: null,
      createdAt: new Date(0).toISOString(),
      updatedAt: new Date(0).toISOString(),
    },
  };
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  operatorId = randomUUID();
  await db.query(
    `INSERT INTO operators(id, email, role, status, password_hash, password_updated_at)
     VALUES ($1, $2, 'operator', 'active', '$argon2id$v=19$m=65536,t=3,p=4$dGVzdA$dGVzdA',
             transaction_timestamp())`,
    [operatorId, "reviewer@example.com"],
  );
  const repository = new PostgresGovernanceRepository(db as unknown as SqlClient);
  const auditSink = {
    recordAudit: async (event: AuditEvent) => {
      audits.push(event);
    },
  } as unknown as ConsoleRepository;
  governance = new GovernanceService(repository, auditSink, config);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("review verdicts", () => {
  it("records a verdict for a subject with no clip row and audits it", async () => {
    const verdict = await governance.recordReviewVerdict(
      session("operator"),
      `key-${randomUUID()}`,
      { source: "raw-prod", subjectKind: "raw_segment", subjectKey, result: "passed", note: "对齐正常" },
      context,
    );
    expect(verdict.result).toBe("passed");
    expect(verdict.subjectKey).toBe(subjectKey);
    const audit = audits.at(-1);
    expect(audit?.action).toBe("review.verdict_recorded");
    expect(audit?.entityType).toBe("r2_object");
    expect(audit?.entityId).toBe(subjectKey);
  });

  it("is append-only: a newer verdict supersedes without rewriting history", async () => {
    const actor = session("operator");
    await governance.recordReviewVerdict(
      actor,
      `key-${randomUUID()}`,
      { source: "raw-prod", subjectKind: "raw_segment", subjectKey, result: "failed", note: "错位" },
      context,
    );
    const history = await governance.listReviewVerdicts(actor, "raw-prod", subjectKey);
    expect(history.length).toBeGreaterThanOrEqual(2);
    expect(history[0]?.result).toBe("failed");
    expect(history.at(-1)?.result).toBe("passed");
    expect(history[0]?.operatorEmail).toBe("reviewer@example.com");
  });

  it("replays on the same idempotency key instead of writing twice", async () => {
    const actor = session("operator");
    const key = `key-${randomUUID()}`;
    const first = await governance.recordReviewVerdict(
      actor,
      key,
      { source: "raw-prod", subjectKind: "raw_segment", subjectKey, result: "passed" },
      context,
    );
    const before = await governance.listReviewVerdicts(actor, "raw-prod", subjectKey);
    const replay = await governance.recordReviewVerdict(
      actor,
      key,
      { source: "raw-prod", subjectKind: "raw_segment", subjectKey, result: "passed" },
      context,
    );
    const after = await governance.listReviewVerdicts(actor, "raw-prod", subjectKey);
    expect(replay.id).toBe(first.id);
    expect(after.length).toBe(before.length);
  });

  it("refuses a viewer on both the write and the read", async () => {
    await expect(
      governance.recordReviewVerdict(
        session("viewer"),
        `key-${randomUUID()}`,
        { source: "raw-prod", subjectKind: "raw_segment", subjectKey, result: "passed" },
        context,
      ),
    ).rejects.toMatchObject({ code: "insufficient_role" });
    await expect(
      governance.listReviewVerdicts(session("viewer"), "raw-prod", subjectKey),
    ).rejects.toMatchObject({ code: "insufficient_role" });
  });

  it("does not touch clips.quality_state", async () => {
    // The whole point of the ledger: a verdict over an out-of-band subject
    // must not nudge any clip toward or away from eligibility.
    const clips = await db.query(`SELECT count(*)::int AS n FROM clips WHERE quality_state <> 'pending'`);
    expect((clips.rows[0] as { n: number }).n).toBe(0);
  });
});

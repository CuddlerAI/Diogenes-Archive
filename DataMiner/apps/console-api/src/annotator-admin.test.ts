import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { AnnotatorAdminService } from "./annotator-admin.js";
import { buildApp } from "./app.js";
import { PostgresAppRepository } from "./app-repository.js";
import { AppService, type TermsSnapshot } from "./app-service.js";
import type { ConsoleConfig } from "./config.js";
import type { AuditEvent, AuthenticatedSession, ConsoleRepository, OperatorRole } from "./domain.js";
import type { BrowseRepository } from "./browse-domain.js";
import { ConsoleError } from "./errors.js";
import type { GovernanceRepository } from "./governance-domain.js";
import type { ConsoleMail, Mailer } from "./mailer.js";
import type { PreviewService } from "./preview.js";
import type { SqlClient } from "./sql.js";
import { argon2TestTimeoutMs } from "./testing.js";

// Claim confirmation sets a password and the reset branch swaps one, both for
// real. See argon2TestTimeoutMs for why the budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

// The operator-side registration and claim flows run against a real PostgreSQL
// engine end to end: register mints a claim challenge, the claim confirm
// activates the credential and records consent, and the reset branch swaps a
// password and kills sessions. Mocking the repository would test the mock.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const terms: TermsSnapshot & { body: string } = {
  policyVersion: "2026-07-01",
  purpose: "staging capture",
  termsHash: "a".repeat(64),
  body: "DataMiner contributor terms, test fixture.\n",
  allowsTrainableUse: false,
  scopes: ["input", "video"],
};

let db: PGlite;
let operatorId: string;
let outbox: ConsoleMail[];
let auditTrail: AuditEvent[];

const mailer: Mailer = {
  send: async (mail) => {
    outbox.push(mail);
  },
};

function configFor(): ConsoleConfig {
  return {
    environment: "development",
    databaseUrl: "postgresql://example/example",
    host: "127.0.0.1",
    port: 8090,
    publicOrigin: "http://localhost:8090",
    ipHashSalt: "salt-for-tests",
    trustedProxyHops: 1,
    mail: { mode: "capture", from: "no-reply@example.com", captureDir: "./.mail", allowlist: [] },
    session: { absoluteMinutes: 720, idleMinutes: 60, sudoMinutes: 15 },
    appSession: { slidingDays: 30 },
    challenge: {
      otpTtlMinutes: 10,
      otpMaxAttempts: 5,
      inviteTtlHours: 24,
      passwordResetTtlMinutes: 30,
    },
    limits: {
      otpSendPerMinute: 1,
      otpSendPerHour: 100,
      otpSendPerDay: 200,
      otpSendPerIpHour: 100,
      loginPerIpHour: 100,
      loginPerEmailHour: 30,
      lockThreshold: 10,
      lockBaseMinutes: 15,
      lockMaxMinutes: 240,
    },
  } as unknown as ConsoleConfig;
}

function actor(role: OperatorRole, overrides: { sudo?: boolean } = {}): AuthenticatedSession {
  const now = new Date().toISOString();
  return {
    operator: {
      id: operatorId,
      email: "ops@diogenesmodel.com",
      passwordHash: null,
      role,
      status: "active",
      failedLoginCount: 0,
      lockedUntil: null,
      locked: false,
      passwordUpdatedAt: null,
      lastLoginAt: null,
      createdAt: now,
      updatedAt: now,
    },
    session: {
      id: randomUUID(),
      operatorId,
      sudoUntil: null,
      sudoActive: overrides.sudo ?? true,
      expiresAt: now,
      lastSeenAt: now,
      createdAt: now,
    },
  };
}

const context = { requestId: "req_test", ipHash: "f".repeat(64), userAgent: "vitest" };

function harness() {
  const client: SqlClient = { query: (text, values) => db.query(text, values as unknown[]) };
  const transaction = async <T>(run: (tx: SqlClient) => Promise<T>): Promise<T> => {
    await db.query("BEGIN");
    try {
      const result = await run(client);
      await db.query("COMMIT");
      return result;
    } catch (error) {
      await db.query("ROLLBACK");
      throw error;
    }
  };
  const config = configFor();
  const repository = new PostgresAppRepository(client, transaction);
  const auditSink = {
    ping: async () => undefined,
    recordAudit: async (event: AuditEvent) => {
      auditTrail.push(event);
    },
  } as unknown as ConsoleRepository;
  const admin = new AnnotatorAdminService(repository, auditSink, mailer, config);
  const appService = new AppService(repository, mailer, config, terms);
  const app = buildApp({
    config,
    repository: auditSink,
    browse: {} as BrowseRepository,
    governance: {} as GovernanceRepository,
    preview: { available: false } as PreviewService,
    mailer,
    appService,
    annotatorAdmin: admin,
  });
  return { admin, app, repository };
}

/** Pulls challenge id + token out of a returned claim URL. */
function parseClaim(claimUrl: string): { challengeId: string; token: string } {
  const url = new URL(claimUrl);
  return {
    challengeId: url.searchParams.get("challenge") ?? "",
    token: url.searchParams.get("token") ?? "",
  };
}

const password = "a sufficiently long password";

async function confirmClaim(
  app: ReturnType<typeof harness>["app"],
  claim: { challengeId: string; token: string },
  overrides: Record<string, unknown> = {},
) {
  return app.inject({
    method: "POST",
    url: "/app/v1/claim/confirm",
    payload: {
      challengeId: claim.challengeId,
      token: claim.token,
      password,
      acceptedPolicyVersion: terms.policyVersion,
      ...overrides,
    },
  });
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
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, 'ops@diogenesmodel.com', $2, transaction_timestamp(), 'admin', 'active')`,
    [operatorId, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
  );
}, 180_000);

beforeEach(async () => {
  outbox = [];
  auditTrail = [];
  await db.query("TRUNCATE console_rate_limits");
});

afterAll(async () => {
  await db?.close();
});

describe("operator-side registration", () => {
  it("creates the annotator, profile, and shadow credential, and mails a claim link", async () => {
    const { admin } = harness();
    const email = `reg-${randomUUID().slice(0, 8)}@example.com`;

    const issued = await admin.register(
      actor("operator"),
      {
        email,
        alias: "现场登记",
        note: "线下引导",
        profile: { displayName: "小王", imKind: "wechat", imHandle: "wx_xiaowang" },
      },
      context,
    );

    expect(issued.claimUrl).toContain("/claim-account?challenge=");
    const credential = await db.query<{ status: string; password_hash: string }>(
      `SELECT status, password_hash FROM annotator_credentials WHERE email = $1`,
      [email],
    );
    expect(credential.rows[0]?.status).toBe("pending_verification");
    // A decoy, but a real argon2 string — the NOT NULL and CHECK hold.
    expect(credential.rows[0]?.password_hash).toMatch(/^\$argon2id\$/);

    const profile = await db.query<{ display_name: string }>(
      `SELECT display_name FROM annotator_profiles WHERE annotator_id = $1`,
      [issued.annotatorId],
    );
    expect(profile.rows[0]?.display_name).toBe("小王");

    expect(outbox.at(-1)?.to).toBe(email);
    expect(outbox.at(-1)?.body).toContain(issued.claimUrl);

    const audit = auditTrail.find((event) => event.action === "annotator.credential_invited");
    expect(audit?.details).toMatchObject({ delivery: "mail", note: "线下引导", reissued: false });
    // The URL is returned once and never audited.
    expect(JSON.stringify(auditTrail)).not.toContain(issued.claimUrl);
  });

  it("reports a taken address as a plain conflict", async () => {
    const { admin } = harness();
    const email = `dup-${randomUUID().slice(0, 8)}@example.com`;
    await admin.register(actor("operator"), { email, alias: "first" }, context);
    await expect(
      admin.register(actor("operator"), { email, alias: "second" }, context),
    ).rejects.toMatchObject({ code: "email_in_use" });
  });

  it("refuses a viewer and refuses without sudo", async () => {
    const { admin } = harness();
    const email = `guard-${randomUUID().slice(0, 8)}@example.com`;
    await expect(
      admin.register(actor("viewer"), { email, alias: "x" }, context),
    ).rejects.toBeInstanceOf(ConsoleError);
    await expect(
      admin.register(actor("operator", { sudo: false }), { email, alias: "x" }, context),
    ).rejects.toMatchObject({ code: "sudo_required" });
  });
});

describe("claim confirmation (activate branch)", () => {
  it("activates end to end: preflight shows terms, confirm sets password and consent, login works", async () => {
    const { admin, app } = harness();
    const email = `claim-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "认领人" }, context);
    const claim = parseClaim(issued.claimUrl);

    const preflight = await app.inject({
      method: "GET",
      url: `/app/v1/claim/${claim.challengeId}`,
    });
    expect(preflight.statusCode).toBe(200);
    expect(preflight.json()).toMatchObject({ kind: "activate", email: `${email[0]}***@example.com` });
    expect(preflight.json().terms.body).toBe(terms.body);

    const confirmed = await confirmClaim(app, claim, {
      profile: { displayName: "本人改的" },
    });
    expect(confirmed.statusCode).toBe(204);

    const credential = await db.query<{ status: string; email_verified_at: Date | null }>(
      `SELECT status, email_verified_at FROM annotator_credentials WHERE email = $1`,
      [email],
    );
    expect(credential.rows[0]?.status).toBe("active");
    expect(credential.rows[0]?.email_verified_at).not.toBeNull();

    const consents = await db.query<{ policy_version: string }>(
      `SELECT policy_version FROM consents WHERE annotator_id = $1 AND status = 'active'`,
      [issued.annotatorId],
    );
    expect(consents.rows).toHaveLength(1);
    expect(consents.rows[0]?.policy_version).toBe(terms.policyVersion);

    const profile = await db.query<{ display_name: string }>(
      `SELECT display_name FROM annotator_profiles WHERE annotator_id = $1`,
      [issued.annotatorId],
    );
    expect(profile.rows[0]?.display_name).toBe("本人改的");

    const login = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password },
    });
    expect(login.statusCode).toBe(200);
  });

  it("refuses stale terms, a wrong token, and a password containing the address", async () => {
    const { admin, app } = harness();
    const email = `edge-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "edge" }, context);
    const claim = parseClaim(issued.claimUrl);

    const stale = await confirmClaim(app, claim, { acceptedPolicyVersion: "2020-01-01" });
    expect(stale.statusCode).toBe(400);
    expect(stale.json()).toMatchObject({ code: "terms_out_of_date" });

    const wrongToken = await confirmClaim(app, { ...claim, token: "not-the-token-at-all" });
    expect(wrongToken.statusCode).toBe(401);

    const containsAddress = await confirmClaim(app, claim, {
      password: `xx${email}xx`,
    });
    expect(containsAddress.statusCode).toBe(400);
    expect(containsAddress.json()).toMatchObject({ code: "weak_password" });
  });

  it("is single use: a replayed confirm fails and the preflight goes dark", async () => {
    const { admin, app } = harness();
    const email = `once-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "once" }, context);
    const claim = parseClaim(issued.claimUrl);

    expect((await confirmClaim(app, claim)).statusCode).toBe(204);
    expect((await confirmClaim(app, claim)).statusCode).toBe(401);

    const preflight = await app.inject({ method: "GET", url: `/app/v1/claim/${claim.challengeId}` });
    expect(preflight.statusCode).toBe(404);
  });
});

describe("credential invite for existing annotators", () => {
  it("gives a governance-created annotator a claimable credential", async () => {
    const { admin, app } = harness();
    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, '手工建档')`, [annotatorId]);
    const email = `shell-${randomUUID().slice(0, 8)}@example.com`;

    const issued = await admin.inviteCredential(actor("operator"), annotatorId, email, context);
    expect(issued.annotatorId).toBe(annotatorId);

    expect((await confirmClaim(app, parseClaim(issued.claimUrl))).statusCode).toBe(204);
    const login = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password },
    });
    expect(login.statusCode).toBe(200);
  });

  it("supersedes the previous link on re-issue", async () => {
    const { admin, app } = harness();
    const email = `reissue-${randomUUID().slice(0, 8)}@example.com`;
    const first = await admin.register(actor("operator"), { email, alias: "r" }, context);
    const second = await admin.inviteCredential(
      actor("operator"),
      first.annotatorId,
      email,
      context,
    );

    expect((await confirmClaim(app, parseClaim(first.claimUrl))).statusCode).toBe(401);
    expect((await confirmClaim(app, parseClaim(second.claimUrl))).statusCode).toBe(204);
  });

  it("refuses an active credential, a mismatched address, and an unknown annotator", async () => {
    const { admin, app } = harness();
    const email = `active-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "a" }, context);
    await confirmClaim(app, parseClaim(issued.claimUrl));

    await expect(
      admin.inviteCredential(actor("operator"), issued.annotatorId, email, context),
    ).rejects.toMatchObject({ code: "credential_active" });

    const pending = await admin.register(
      actor("operator"),
      { email: `pend-${randomUUID().slice(0, 8)}@example.com`, alias: "p" },
      context,
    );
    await expect(
      admin.inviteCredential(actor("operator"), pending.annotatorId, "other@example.com", context),
    ).rejects.toMatchObject({ code: "email_mismatch" });

    await expect(
      admin.inviteCredential(actor("operator"), randomUUID(), email, context),
    ).rejects.toMatchObject({ code: "annotator_not_found" });
  });
});

describe("operator-triggered password reset", () => {
  it("resets through the claim page: sessions die, consent is not duplicated", async () => {
    const { admin, app } = harness();
    const email = `reset-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "r" }, context);
    await confirmClaim(app, parseClaim(issued.claimUrl));

    const login = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password },
    });
    const oldToken = login.json().token as string;

    const reset = await admin.triggerPasswordReset(actor("operator"), issued.annotatorId, context);
    const claim = parseClaim(reset.claimUrl);

    const preflight = await app.inject({ method: "GET", url: `/app/v1/claim/${claim.challengeId}` });
    expect(preflight.json()).toMatchObject({ kind: "reset" });
    expect(preflight.json().terms).toBeUndefined();

    const newPassword = "an entirely different secret";
    const confirmed = await confirmClaim(app, claim, {
      password: newPassword,
      acceptedPolicyVersion: undefined,
    });
    expect(confirmed.statusCode).toBe(204);

    // Old sessions are gone, the new password works, and there is still exactly
    // one active consent.
    const stale = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { authorization: `Bearer ${oldToken}` },
    });
    expect(stale.statusCode).toBe(401);
    const relogin = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password: newPassword },
    });
    expect(relogin.statusCode).toBe(200);
    const consents = await db.query(
      `SELECT id FROM consents WHERE annotator_id = $1 AND status = 'active'`,
      [issued.annotatorId],
    );
    expect(consents.rows).toHaveLength(1);
  });

  it("refuses a pending account — that is the invite's job", async () => {
    const { admin } = harness();
    const email = `pendreset-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "p" }, context);
    await expect(
      admin.triggerPasswordReset(actor("operator"), issued.annotatorId, context),
    ).rejects.toMatchObject({ code: "credential_not_active" });
  });
});

describe("operator-side profile edit", () => {
  it("writes the profile and audits field names, never values", async () => {
    const { admin } = harness();
    const email = `prof-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "p" }, context);

    const updated = await admin.updateProfile(
      actor("operator", { sudo: false }),
      issued.annotatorId,
      { displayName: "阿真", phone: "13911112222" },
      context,
    );
    expect(updated).toMatchObject({ displayName: "阿真", phone: "13911112222", imKind: null });

    const audit = auditTrail.find((event) => event.action === "annotator.profile_updated");
    expect(audit?.details?.fields).toEqual(["displayName", "phone"]);
    expect(JSON.stringify(audit)).not.toContain("13911112222");
  });

  it("refuses to write contact data back onto a tombstone", async () => {
    const { admin } = harness();
    const email = `tomb-${randomUUID().slice(0, 8)}@example.com`;
    const issued = await admin.register(actor("operator"), { email, alias: "t" }, context);
    await db.query(
      `UPDATE annotators SET status = 'tombstoned', tombstoned_at = transaction_timestamp()
        WHERE id = $1`,
      [issued.annotatorId],
    );
    await expect(
      admin.updateProfile(actor("operator"), issued.annotatorId, { displayName: "x" }, context),
    ).rejects.toMatchObject({ code: "annotator_tombstoned" });
  });
});

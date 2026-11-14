import { createHash, randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import { generate, normalize } from "./app-invites.js";
import { PostgresAppRepository } from "./app-repository.js";
import { AppService, type AppServiceLog, type TermsSnapshot } from "./app-service.js";
import type { ConsoleConfig } from "./config.js";
import { RecipientNotAllowedError, type ConsoleMail, type Mailer } from "./mailer.js";
import { argon2TestTimeoutMs } from "./testing.js";
import type { BrowseRepository } from "./browse-domain.js";
import type { ConsoleRepository } from "./domain.js";
import type { GovernanceRepository } from "./governance-domain.js";
import type { PreviewService } from "./preview.js";
import type { SqlClient } from "./sql.js";
import { catalogSetEtag, serializeCatalogSet } from "@dataminer/api/catalog-delivery";

// Signup, login, password reset and the lockout ladder all hash for real. See
// argon2TestTimeoutMs for why the budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

// The whole annotator path runs against a real PostgreSQL engine: signup spends
// an invite inside a transaction, verification activates, login mints a session,
// and enrollment issues the credential the ingest gate will later match. Mocking
// the repository here would test the mock.
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
let deviceProfileVersionId: string;
let outbox: ConsoleMail[];

const mailer: Mailer = {
  send: async (mail) => {
    outbox.push(mail);
  },
};

type ConfigOverrides = {
  limits?: Partial<ConsoleConfig["limits"]>;
  trustedProxyHops?: number;
};

function configFor(overrides: ConfigOverrides = {}): ConsoleConfig {
  return {
    environment: "development",
    databaseUrl: "postgresql://example/example",
    host: "127.0.0.1",
    port: 8090,
    publicOrigin: "http://localhost:8090",
    ipHashSalt: "salt-for-tests",
    // One hop: the test injects a socket address, so anything in
    // X-Forwarded-For beyond the rightmost entry is caller-supplied.
    trustedProxyHops: overrides.trustedProxyHops ?? 1,
    mail: { mode: "capture", from: "no-reply@example.com", captureDir: "./.mail", allowlist: [] },
    session: { absoluteMinutes: 720, idleMinutes: 60, sudoMinutes: 15 },
    appSession: { slidingDays: 30 },
    challenge: {
      otpTtlMinutes: 10,
      otpMaxAttempts: 5,
      inviteTtlHours: 24,
      passwordResetTtlMinutes: 30,
      passwordMinLength: 12,
    },
    limits: {
      otpSendPerMinute: 1,
      otpSendPerHour: 5,
      otpSendPerDay: 20,
      // Every registration in this file shares one injected IP; the ceilings
      // only need to be above the number of accounts the suite creates.
      otpSendPerIpHour: 100,
      loginPerIpHour: 100,
      loginPerEmailHour: 30,
      lockThreshold: 10,
      lockBaseMinutes: 15,
      lockMaxMinutes: 240,
      ...overrides.limits,
    },
    r2: null,
    previewTtlSeconds: 300,
    catalogBootstrapChannel: "dev",
  } as unknown as ConsoleConfig;
}

function harness(
  options: ConfigOverrides & {
    mailer?: Mailer;
    log?: AppServiceLog;
    /** Serve a different document, to exercise a consent that has fallen behind. */
    terms?: TermsSnapshot & { body: string };
  } = {},
) {
  const client: SqlClient = { query: (text, values) => db.query(text, values as unknown[]) };
  // PGlite is single-connection, so the "transaction" is the engine's own.
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
  const delivery = options.mailer ?? mailer;
  const config = configFor(options);
  const repository = new PostgresAppRepository(client, transaction);
  const appService = new AppService(repository, delivery, config, options.terms ?? terms, options.log);
  const app = buildApp({
    config,
    repository: { ping: async () => undefined, missingConsoleGrants: async () => [] } as unknown as ConsoleRepository,
    browse: {} as BrowseRepository,
    governance: {} as GovernanceRepository,
    preview: { available: false } as PreviewService,
    mailer: delivery,
    appService,
  });
  return { app, repository, appService };
}

async function insertInvite() {
  const display = generate();
  await db.query(`INSERT INTO invite_codes(code, created_by) VALUES ($1, $2)`, [
    normalize(display),
    operatorId,
  ]);
  return display;
}

function signupBody(code: string, email: string) {
  return {
    inviteCode: code,
    email,
    password: "correct horse battery staple",
    alias: "Tester",
    acceptedPolicyVersion: terms.policyVersion,
  };
}

function lastCode(): string {
  const body = outbox.at(-1)?.body ?? "";
  return /([0-9]{6})/.exec(body)?.[1] ?? "";
}

/** Signup, read the mailed code, verify, log in. Returns the session token. */
async function register(app: ReturnType<typeof harness>["app"], email: string) {
  const code = await insertInvite();
  const signup = await app.inject({ method: "POST", url: "/app/v1/signup", payload: signupBody(code, email) });
  expect(signup.statusCode).toBe(202);
  const verify = await app.inject({
    method: "POST",
    url: "/app/v1/signup/verify",
    payload: { challengeId: signup.json().challengeId, code: lastCode() },
  });
  expect(verify.statusCode).toBe(204);
  const login = await app.inject({
    method: "POST",
    url: "/app/v1/login",
    payload: { email, password: "correct horse battery staple" },
  });
  expect(login.statusCode).toBe(200);
  return login.json().token as string;
}

function enrollBody(clientDeviceId: string) {
  return {
    clientDeviceId,
    deviceProfile: {
      id: "dev-device-profile",
      version: "1",
      contentHash: `sha256:${"a".repeat(64)}`,
    },
    windowsBuild: "10.0.26100",
    gpuPciId: "10DE:2684",
    gpuDriver: "566.36",
    clientVersion: "0.1.0",
    displaySnapshot: {},
    inputDevicesSnapshot: [],
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
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, 'ops@diogenesmodel.com', $2, transaction_timestamp(), 'admin', 'active')`,
    [operatorId, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
  );
  deviceProfileVersionId = randomUUID();
  await db.query(
    `INSERT INTO trusted_catalog_versions(
       id, kind, catalog_id, version, game_id, content_hash, snapshot,
       status, approved_at, approved_by
     ) VALUES ($1, 'device_profile', 'dev-device-profile', '1', NULL, $2, '{}'::jsonb,
               'approved', transaction_timestamp(), 'test')`,
    [deviceProfileVersionId, `sha256:${"a".repeat(64)}`],
  );
}, 180_000);

beforeEach(async () => {
  outbox = [];
  // Rate limit buckets are keyed by hashed address, and every injected request
  // shares one socket address — so without this the per-IP windows accumulate
  // across the file and adding a case anywhere makes an unrelated case 429.
  // Cases that exercise limiting do so within a single test.
  await db.query("TRUNCATE console_rate_limits");
});

afterAll(async () => {
  await db?.close();
});

describe("annotator signup", () => {
  it("spends the invite, records consent, and mails a code", async () => {
    const { app } = harness();
    const code = await insertInvite();

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "first@example.com"),
    });

    expect(response.statusCode).toBe(202);
    expect(outbox).toHaveLength(1);
    expect(outbox[0]?.to).toBe("first@example.com");

    const invite = await db.query<{ used_count: number }>(
      `SELECT used_count FROM invite_codes WHERE code = $1`,
      [normalize(code)],
    );
    expect(invite.rows[0]?.used_count).toBe(1);

    // Consent has to exist from the first moment, or no capture grant can ever
    // be issued for this user (11 §1.6).
    const consent = await db.query<{ policy_version: string; terms_hash: string }>(
      `SELECT c.policy_version, c.terms_hash
         FROM consents c
         JOIN annotator_credentials cr ON cr.annotator_id = c.annotator_id
        WHERE cr.email = 'first@example.com'`,
    );
    expect(consent.rows[0]).toEqual({
      policy_version: terms.policyVersion,
      terms_hash: terms.termsHash,
    });
    await app.close();
  });

  it("refuses a client that is displaying stale terms", async () => {
    const { app } = harness();
    const code = await insertInvite();

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: { ...signupBody(code, "stale@example.com"), acceptedPolicyVersion: "1999-01-01" },
    });

    // Recording consent against what the server currently serves, when the user
    // was shown something else, would be recording a lie.
    expect(response.statusCode).toBe(400);
    expect(response.json()).toMatchObject({ code: "terms_out_of_date" });
    await app.close();
  });

  it("does not tell a caller whether an address is already registered", async () => {
    const { app } = harness();
    await register(app, "taken@example.com");
    outbox = [];

    const code = await insertInvite();
    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "taken@example.com"),
    });

    // Same status and same body shape as a fresh signup.
    expect(response.statusCode).toBe(202);
    expect(response.json()).toHaveProperty("challengeId");

    // But the invite was not spent — probing with a known address must not burn
    // somebody else's code.
    const invite = await db.query<{ used_count: number }>(
      `SELECT used_count FROM invite_codes WHERE code = $1`,
      [normalize(code)],
    );
    expect(invite.rows[0]?.used_count).toBe(0);
    await app.close();
  });

  it("gives one message for unknown, spent, and revoked codes", async () => {
    const { app } = harness();
    const spent = await insertInvite();
    await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(spent, "spender@example.com"),
    });

    const revoked = await insertInvite();
    await db.query(
      `UPDATE invite_codes SET status = 'revoked', revoked_at = transaction_timestamp(),
              revoked_by = $2 WHERE code = $1`,
      [normalize(revoked), operatorId],
    );

    const bodies = await Promise.all(
      [spent, revoked, "DM-ZZZZ-ZZZZ-ZZZZ"].map(async (code, index) => {
        const response = await app.inject({
          method: "POST",
          url: "/app/v1/signup",
          payload: signupBody(code, `probe-${index}@example.com`),
        });
        return { status: response.statusCode, code: response.json().code };
      }),
    );

    for (const body of bodies) {
      expect(body).toEqual({ status: 403, code: "invite_invalid" });
    }
    await app.close();
  });

  // The regression this whole ordering exists for. Checking the address before
  // the code made the two rejections differ — 202 for a registered address, 403
  // for a free one — so a made-up code enumerated accounts at no cost.
  it("answers an unusable code identically for a registered and a free address", async () => {
    const { app } = harness();
    await register(app, "enum-known@example.com");
    outbox = [];

    const probes = await Promise.all(
      ["enum-known@example.com", "enum-nobody@example.com"].map(async (email) => {
        const response = await app.inject({
          method: "POST",
          url: "/app/v1/signup",
          payload: signupBody("DM-ZZZZ-ZZZZ-ZZZZ", email),
        });
        return { status: response.statusCode, code: response.json().code };
      }),
    );

    expect(probes[0]).toEqual(probes[1]);
    expect(probes[0]).toEqual({ status: 403, code: "invite_invalid" });
    // And nothing was mailed, so the mailbox is not a side channel either.
    expect(outbox).toHaveLength(0);
    await app.close();
  });

  // A revoked or exhausted code is just as unusable as an invented one, so the
  // same must hold for those.
  it("answers a spent code identically for a registered and a free address", async () => {
    const { app } = harness();
    await register(app, "enum-holder@example.com");
    const spent = await insertInvite();
    await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(spent, "enum-spender@example.com"),
    });

    const probes = await Promise.all(
      ["enum-holder@example.com", "enum-unseen@example.com"].map(async (email) => {
        const response = await app.inject({
          method: "POST",
          url: "/app/v1/signup",
          payload: signupBody(spent, email),
        });
        return { status: response.statusCode, code: response.json().code };
      }),
    );

    expect(probes[0]).toEqual(probes[1]);
    expect(probes[0]).toEqual({ status: 403, code: "invite_invalid" });
    await app.close();
  });

  // The other half of the same-shape answer: a mail goes out either way, so the
  // presence of a message — and the wall clock spent sending it — cannot
  // separate a taken address from a free one (11 §7.1).
  it("mails the owner when their address is used for a signup attempt", async () => {
    const { app } = harness();
    await register(app, "notice-owner@example.com");
    outbox = [];

    const code = await insertInvite();
    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "notice-owner@example.com"),
    });

    expect(response.statusCode).toBe(202);
    expect(outbox).toHaveLength(1);
    expect(outbox[0]?.to).toBe("notice-owner@example.com");
    // A notice, not a credential: nothing in it can advance a signup.
    expect(outbox[0]?.body).not.toMatch(/[0-9]{6}/);
    await app.close();
  });

  it("accepts the code in the form the user was given it", async () => {
    const { app } = harness();
    const code = await insertInvite();

    // Display form with dashes, lower-cased — exactly what a paste looks like.
    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code.toLowerCase(), "paste@example.com"),
    });

    expect(response.statusCode).toBe(202);
    await app.close();
  });
});

// Mail is sent after the signup transaction commits, so a delivery failure
// arrives when the invite is already spent and the account already exists.
// Before the resend path that state was terminal: the user could not verify
// (no code) and could not sign up again (their own address was taken).
describe("verification mail recovery", () => {
  const failingMailer: Mailer = {
    send: async () => {
      throw new RecipientNotAllowedError();
    },
  };

  it("keeps the signup when delivery fails and records why", async () => {
    const { app } = harness({ mailer: failingMailer });
    const code = await insertInvite();

    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "resend-unreachable@example.com"),
    });

    // The account was created, so the answer is the one a created account gets.
    expect(signup.statusCode).toBe(202);
    const created = await db.query<{ status: string }>(
      `SELECT status FROM annotator_credentials WHERE email = $1`,
      ["resend-unreachable@example.com"],
    );
    expect(created.rows[0]?.status).toBe("pending_verification");

    const audit = await db.query<{ action: string }>(
      `SELECT action FROM audit_events WHERE action = 'annotator.mail_delivery_failed'`,
    );
    expect(audit.rows).toHaveLength(1);
    await app.close();
  });

  it("lets a resend recover an account whose first code never arrived", async () => {
    const broken = harness({ mailer: failingMailer });
    const code = await insertInvite();
    const signup = await broken.app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "resend-recovered@example.com"),
    });
    await broken.app.close();

    outbox = [];
    const { app } = harness();
    const resend = await app.inject({
      method: "POST",
      url: "/app/v1/signup/resend",
      payload: { challengeId: signup.json().challengeId },
    });

    expect(resend.statusCode).toBe(202);
    expect(outbox).toHaveLength(1);

    // The reissued code works, and it is the new challenge it belongs to.
    const verify = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId: resend.json().challengeId, code: lastCode() },
    });
    expect(verify.statusCode).toBe(204);
    await app.close();
  });

  it("retires the previous code so only one is ever live", async () => {
    const { app } = harness();
    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "resend-reissue@example.com"),
    });
    const firstCode = lastCode();

    const resend = await app.inject({
      method: "POST",
      url: "/app/v1/signup/resend",
      payload: { challengeId: signup.json().challengeId },
    });

    const stale = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId: signup.json().challengeId, code: firstCode },
    });
    expect(stale.statusCode).toBe(401);

    const fresh = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId: resend.json().challengeId, code: lastCode() },
    });
    expect(fresh.statusCode).toBe(204);
    await app.close();
  });

  // Keyed on the challenge id rather than the address precisely so it is not a
  // second oracle. An id that resolves to nothing looks like one that does.
  it("answers an unknown challenge like a real one, and mails nothing", async () => {
    const { app } = harness();
    outbox = [];

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup/resend",
      payload: { challengeId: randomUUID() },
    });

    expect(response.statusCode).toBe(202);
    expect(response.json()).toHaveProperty("challengeId");
    expect(outbox).toHaveLength(0);
    await app.close();
  });

  // Every resend returns a *new* challenge id, and the lookup deliberately
  // accepts superseded ones — so keying the limit on the id the caller sent
  // meant following the returned id gave a fresh budget each time and the mail
  // went out forever. The anchor has to be the account.
  it("stops a chain of resends that walks the id it was just handed", async () => {
    const { app } = harness();
    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "resend-chain@example.com"),
    });
    outbox = [];

    let challengeId = signup.json().challengeId as string;
    const statuses: number[] = [];
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const response = await app.inject({
        method: "POST",
        url: "/app/v1/signup/resend",
        payload: { challengeId },
      });
      statuses.push(response.statusCode);
      if (response.statusCode === 202) challengeId = response.json().challengeId;
    }

    expect(statuses).toContain(429);
    // The send budget bounds the mail, not the number of ids the caller minted.
    expect(outbox.length).toBeLessThanOrEqual(configFor().limits.otpSendPerHour);
    await app.close();
  });

  // The route is unauthenticated and the challenge id is caller-chosen, so
  // without an address-anchored limit each random uuid was a brand new bucket:
  // unlimited requests, unlimited rows in console_rate_limits, no 429 ever.
  it("limits unknown challenge ids by caller address", async () => {
    const { app } = harness({ limits: { otpSendPerIpHour: 3 } });

    const statuses: number[] = [];
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const response = await app.inject({
        method: "POST",
        url: "/app/v1/signup/resend",
        payload: { challengeId: randomUUID() },
      });
      statuses.push(response.statusCode);
    }

    expect(statuses.slice(0, 3)).toEqual([202, 202, 202]);
    expect(statuses.slice(3)).toEqual([429, 429]);
    await app.close();
  });

  it("will not re-mail an account that is already verified", async () => {
    const { app } = harness();
    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "resend-done@example.com"),
    });
    await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId: signup.json().challengeId, code: lastCode() },
    });
    outbox = [];

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/signup/resend",
      payload: { challengeId: signup.json().challengeId },
    });

    expect(response.statusCode).toBe(202);
    expect(outbox).toHaveLength(0);
    await app.close();
  });
});

// Everything derived from the client address — rate limit buckets, the
// redemption ledger's ip_hash, the audit trail — is only as trustworthy as the
// hop count. Reading the leftmost X-Forwarded-For entry, as the first cut did,
// let a caller pick its own bucket per request.
describe("client address trust", () => {
  it("ignores forwarded-for entries the caller prepended", async () => {
    const { app } = harness({ limits: { loginPerIpHour: 2 } });

    const statuses: number[] = [];
    for (let attempt = 0; attempt < 4; attempt += 1) {
      const response = await app.inject({
        method: "POST",
        url: "/app/v1/login",
        // A different forged prefix every time; the rightmost entry is what a
        // real front end appends and is the only one that should count.
        headers: { "x-forwarded-for": `10.0.0.${attempt}, 203.0.113.7` },
        payload: { email: `xff-${attempt}@example.com`, password: "correct horse battery staple" },
      });
      statuses.push(response.statusCode);
    }

    // Two allowed (401, unknown address), then the shared bucket is exhausted.
    expect(statuses.slice(0, 2)).toEqual([401, 401]);
    expect(statuses.slice(2)).toEqual([429, 429]);
    await app.close();
  });

  // The case above passes just as well if every caller collapses into one
  // bucket — which is the opposite failure, and just as broken. This pins the
  // other half: distinct real callers must not spend each other's budget.
  it("keeps distinct callers in distinct buckets", async () => {
    const { app } = harness({ limits: { loginPerIpHour: 1 } });

    const first = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      headers: { "x-forwarded-for": "203.0.113.7" },
      payload: { email: "xff-a@example.com", password: "correct horse battery staple" },
    });
    const second = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      headers: { "x-forwarded-for": "198.51.100.4" },
      payload: { email: "xff-b@example.com", password: "correct horse battery staple" },
    });
    const repeat = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      headers: { "x-forwarded-for": "203.0.113.7" },
      payload: { email: "xff-a@example.com", password: "correct horse battery staple" },
    });

    expect(first.statusCode).toBe(401);
    expect(second.statusCode).toBe(401); // a different address, its own budget
    expect(repeat.statusCode).toBe(429); // the first address is spent
    await app.close();
  });
});

describe("verification and login", () => {
  it("activates on the mailed code and rejects a replay of it", async () => {
    const logs: Array<{ fields: Parameters<AppServiceLog>[0]; message: string }> = [];
    const { app } = harness({
      log: (fields, message) => logs.push({ fields, message }),
    });
    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "activate@example.com"),
    });
    const challengeId = signup.json().challengeId;
    const mailed = lastCode();

    expect(
      (await app.inject({ method: "POST", url: "/app/v1/signup/verify", payload: { challengeId, code: mailed } }))
        .statusCode,
    ).toBe(204);
    // Single use: the same code cannot be presented twice.
    const replay = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId, code: mailed },
    });
    expect(replay.statusCode).toBe(401);
    expect(replay.json()).toMatchObject({
      code: "signup_verification_failed",
      retryable: false,
    });
    expect(replay.json().requestId).toEqual(expect.stringMatching(/^req_/));
    expect(logs.at(-1)).toEqual({
      fields: {
        operation: "signup_verify",
        reason: "challenge_consumed",
        requestId: replay.json().requestId,
        severity: "WARNING",
      },
      message: "annotator signup verification rejected",
    });
    await app.close();
  });

  it("keeps signup verification failures public-same while logging safe internal reasons", async () => {
    const logs: Array<{ fields: Parameters<AppServiceLog>[0]; message: string }> = [];
    const { app } = harness({
      log: (fields, message) => logs.push({ fields, message }),
    });
    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "diagnostic-reasons@example.com"),
    });
    const challengeId = signup.json().challengeId as string;
    const mailed = lastCode();

    const wrong = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId, code: "000000" === mailed ? "000001" : "000000" },
    });
    expect(wrong.statusCode).toBe(401);
    expect(wrong.json()).toMatchObject({
      code: "signup_verification_failed",
      retryable: false,
    });
    expect(logs.at(-1)?.fields).toMatchObject({
      operation: "signup_verify",
      reason: "code_mismatch",
      requestId: wrong.json().requestId,
      severity: "WARNING",
    });

    await db.query(
      `UPDATE annotator_email_challenges
          SET created_at = transaction_timestamp() - interval '2 seconds',
              expires_at = transaction_timestamp() - interval '1 second'
        WHERE id = $1`,
      [challengeId],
    );
    const expired = await app.inject({
      method: "POST",
      url: "/app/v1/signup/verify",
      payload: { challengeId, code: mailed },
    });
    expect(expired.statusCode).toBe(401);
    expect(expired.json().code).toBe("signup_verification_failed");
    expect(logs.at(-1)?.fields).toMatchObject({
      operation: "signup_verify",
      reason: "challenge_expired",
      requestId: expired.json().requestId,
    });

    const serializedLogs = JSON.stringify(logs);
    expect(serializedLogs).not.toContain("diagnostic-reasons@example.com");
    expect(serializedLogs).not.toContain(challengeId);
    expect(serializedLogs).not.toContain(mailed);
    await app.close();
  });

  it("refuses login until the address is verified", async () => {
    const { app } = harness();
    const code = await insertInvite();
    await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "unverified@example.com"),
    });

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email: "unverified@example.com", password: "correct horse battery staple" },
    });

    expect(response.statusCode).toBe(401);
    await app.close();
  });

  it("answers a wrong password and an unknown address identically", async () => {
    const { app } = harness();
    await register(app, "real@example.com");

    const wrong = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email: "real@example.com", password: "not the right password" },
    });
    const unknown = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email: "ghost@example.com", password: "not the right password" },
    });

    expect(wrong.statusCode).toBe(unknown.statusCode);
    expect(wrong.json().code).toBe(unknown.json().code);
    expect(wrong.json().message).toBe(unknown.json().message);
    await app.close();
  });

  it("stops an issued session the moment the account is suspended", async () => {
    const { app } = harness();
    const token = await register(app, "suspendable@example.com");
    const headers = { authorization: `Bearer ${token}` };

    expect((await app.inject({ method: "GET", url: "/app/v1/session", headers })).statusCode).toBe(200);

    await db.query(
      `UPDATE annotator_credentials
          SET status = 'suspended', suspended_at = transaction_timestamp()
        WHERE email = 'suspendable@example.com'`,
    );

    // Not "when the token expires" — the next request (10 §12.7).
    expect((await app.inject({ method: "GET", url: "/app/v1/session", headers })).statusCode).toBe(401);
    await app.close();
  });

  it("revokes the session on logout", async () => {
    const { app } = harness();
    const token = await register(app, "logout@example.com");
    const headers = { authorization: `Bearer ${token}` };

    expect((await app.inject({ method: "POST", url: "/app/v1/logout", headers })).statusCode).toBe(204);
    expect((await app.inject({ method: "GET", url: "/app/v1/session", headers })).statusCode).toBe(401);
    await app.close();
  });
});

describe("device enrollment", () => {
  it("issues a credential once and replays without minting a second", async () => {
    const { app } = harness();
    const token = await register(app, "enroller@example.com");
    const headers = { authorization: `Bearer ${token}` };

    const first = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers,
      payload: enrollBody("machine-a"),
    });
    expect(first.statusCode).toBe(201);
    expect(first.json().token).toEqual(expect.any(String));

    const replay = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers,
      payload: enrollBody("machine-a"),
    });
    // 200 with a null token: the machine is enrolled, and its existing
    // credential is untouched.
    expect(replay.statusCode).toBe(200);
    expect(replay.json().token).toBeNull();
    expect(replay.json().deviceId).toBe(first.json().deviceId);
    await app.close();
  });

  it("linearizes concurrent enrollment of the same machine", async () => {
    const { app } = harness();
    const token = await register(app, "concurrent-enroller@example.com");
    const request = () =>
      app.inject({
        method: "POST",
        url: "/app/v1/devices/enroll",
        headers: { authorization: `Bearer ${token}` },
        payload: enrollBody("machine-concurrent"),
      });

    const responses = await Promise.all([request(), request()]);
    expect(responses.map((response) => response.statusCode).sort()).toEqual([200, 201]);
    expect(new Set(responses.map((response) => response.json().deviceId)).size).toBe(1);
    expect(responses.filter((response) => typeof response.json().token === "string")).toHaveLength(1);
    const deviceId = responses[0].json().deviceId;
    const rows = await db.query<{ profile_id: string }>(
      `SELECT profile_id FROM device_profile_bindings WHERE device_id = $1`,
      [deviceId],
    );
    expect(rows.rows).toHaveLength(0);
    await app.close();
  });

  it("accepts a legacy profile field without creating authorization", async () => {
    const { app } = harness();
    const token = await register(app, "multi-profile@example.com");
    const headers = { authorization: `Bearer ${token}` };
    const first = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers,
      payload: enrollBody("machine-multi-profile"),
    });
    expect(first.statusCode).toBe(201);
    const before = await db.query<{
      credential_fingerprint: string;
      certified_profile_id: string | null;
    }>(
      `SELECT credential_fingerprint, certified_profile_id FROM devices WHERE id = $1`,
      [first.json().deviceId],
    );

    const second = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers,
      payload: {
        ...enrollBody("machine-multi-profile"),
        deviceProfile: {
          id: "legacy-client-unknown-profile",
          version: "99",
          contentHash: `sha256:${"b".repeat(64)}`,
        },
      },
    });

    expect(second.statusCode).toBe(200);
    expect(second.json()).toMatchObject({ deviceId: first.json().deviceId, token: null });
    const after = await db.query<{
      credential_fingerprint: string;
      certified_profile_id: string | null;
    }>(
      `SELECT credential_fingerprint, certified_profile_id FROM devices WHERE id = $1`,
      [first.json().deviceId],
    );
    expect(after.rows[0]).toEqual(before.rows[0]);
    const bindings = await db.query<{ profile_id: string; status: string }>(
      `SELECT profile_id, status FROM device_profile_bindings
        WHERE device_id = $1 ORDER BY profile_id`,
      [first.json().deviceId],
    );
    expect(after.rows[0]?.certified_profile_id).toBeNull();
    expect(bindings.rows).toHaveLength(0);
    await app.close();
  });

  it("stores only the hash of the credential it hands out", async () => {
    const { app } = harness();
    const token = await register(app, "hashonly@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${token}` },
      payload: enrollBody("machine-b"),
    });

    const stored = await db.query<{ credential_fingerprint: string }>(
      `SELECT credential_fingerprint FROM devices WHERE id = $1`,
      [response.json().deviceId],
    );
    const fingerprint = stored.rows[0]?.credential_fingerprint ?? "";
    expect(fingerprint).toMatch(/^[0-9a-f]{64}$/);
    expect(fingerprint).not.toBe(response.json().token);
    await app.close();
  });

  it("refuses to enroll a machine id that belongs to someone else", async () => {
    const { app } = harness();
    const owner = await register(app, "owner@example.com");
    await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${owner}` },
      payload: enrollBody("machine-c"),
    });

    const stranger = await register(app, "stranger@example.com");
    const response = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${stranger}` },
      payload: enrollBody("machine-c"),
    });

    expect(response.statusCode).toBe(403);
    await app.close();
  });

  // D4: unbounded per annotator. This must work, not be defended against.
  it("lets one account enroll several machines and lists only its own", async () => {
    const { app } = harness();
    const mine = await register(app, "fleet@example.com");
    const headers = { authorization: `Bearer ${mine}` };
    for (const name of ["fleet-1", "fleet-2", "fleet-3"]) {
      expect(
        (await app.inject({ method: "POST", url: "/app/v1/devices/enroll", headers, payload: enrollBody(name) }))
          .statusCode,
      ).toBe(201);
    }

    const listed = await app.inject({ method: "GET", url: "/app/v1/devices", headers });
    expect(listed.json().devices).toHaveLength(3);

    const other = await register(app, "otherfleet@example.com");
    const otherList = await app.inject({
      method: "GET",
      url: "/app/v1/devices",
      headers: { authorization: `Bearer ${other}` },
    });
    expect(otherList.json().devices).toHaveLength(0);
    await app.close();
  });

  it("enrolls without a device profile and stores no profile authorization", async () => {
    const { app } = harness();
    const token = await register(app, "profilefree@example.com");
    const { deviceProfile: _legacy, ...payload } = enrollBody("machine-profile-free");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${token}` },
      payload,
    });

    expect(response.statusCode).toBe(201);
    const stored = await db.query<{ certified_profile_id: string | null }>(
      `SELECT certified_profile_id FROM devices WHERE id = $1`,
      [response.json().deviceId],
    );
    expect(stored.rows[0]?.certified_profile_id).toBeNull();
    await app.close();
  });

  it("requires a session", async () => {
    const { app } = harness();

    const anonymous = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      payload: enrollBody("machine-d"),
    });
    const bogus = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: "Bearer not-a-real-token" },
      payload: enrollBody("machine-d"),
    });

    expect(anonymous.statusCode).toBe(401);
    expect(bogus.statusCode).toBe(401);
    await app.close();
  });
});

describe("client telemetry", () => {
  /** Registers, enrols, and returns the device credential the reporter uses. */
  async function enrolledDevice(app: ReturnType<typeof harness>["app"], email: string) {
    const session = await register(app, email);
    const enrolled = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${session}` },
      payload: enrollBody(`telemetry-${email.split("@")[0]}`),
    });
    return { deviceToken: enrolled.json().token as string, deviceId: enrolled.json().deviceId as string };
  }

  function event(overrides: Record<string, unknown> = {}) {
    return {
      eventId: randomUUID(),
      occurredAt: "2026-07-01T10:00:00Z",
      kind: "app_started",
      severity: "info",
      clientVersion: "0.1.0",
      details: { schema_version: "dm-client-app-started-1" },
      ...overrides,
    };
  }

  it("accepts a batch authenticated by the device credential", async () => {
    const { app } = harness();
    const { deviceToken, deviceId } = await enrolledDevice(app, "telemetry@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: {
        events: [
          event(),
          event({ kind: "session_started", details: { schema_version: "dm-client-session-1" } }),
        ],
      },
    });

    expect(response.statusCode).toBe(202);
    expect(response.json()).toMatchObject({ accepted: 2 });
    const stored = await db.query<{ kind: string }>(
      `SELECT kind FROM client_events WHERE device_id = $1 ORDER BY kind`,
      [deviceId],
    );
    expect(stored.rows.map((row) => row.kind)).toEqual(["app_started", "session_started"]);
    await app.close();
  });

  // The reporter retries after a timeout without knowing whether the first
  // attempt landed, so a repeat of the same id must not double-count.
  it("ignores a replayed event id", async () => {
    const { app } = harness();
    const { deviceToken, deviceId } = await enrolledDevice(app, "replay@example.com");
    const single = event();

    await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: { events: [single] },
    });
    const replay = await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: { events: [single] },
    });

    expect(replay.statusCode).toBe(202);
    const count = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM client_events WHERE device_id = $1`,
      [deviceId],
    );
    expect(count.rows[0]?.count).toBe("1");
    await app.close();
  });

  it("refuses an absent, unknown, or annotator-session credential", async () => {
    const { app } = harness();
    const annotatorSession = await register(app, "wrongtoken@example.com");

    for (const headers of [
      undefined,
      { authorization: "Bearer not-a-device-credential" },
      // An annotator session is not a device credential: the two token spaces
      // are separate and this endpoint only accepts the device one.
      { authorization: `Bearer ${annotatorSession}` },
    ]) {
      const response = await app.inject({
        method: "POST",
        url: "/app/v1/client-events",
        ...(headers ? { headers } : {}),
        payload: { events: [event()] },
      });
      expect(response.statusCode).toBe(401);
    }
    await app.close();
  });

  it("refuses an event kind that is not on the allowlist", async () => {
    const { app } = harness();
    const { deviceToken } = await enrolledDevice(app, "badkind@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: { events: [event({ kind: "whatever_the_client_invented" })] },
    });

    // Refused loudly rather than stored and forgotten: the console has to be
    // able to render every kind it holds.
    expect(response.statusCode).toBe(400);
    await app.close();
  });

  // 11 §6.4: telemetry has to be purgeable when consent is withdrawn, which is
  // exactly why it does not live in audit_events.
  it("attaches the annotator's active consent so a consent purge can reach it", async () => {
    const { app } = harness();
    const { deviceToken, deviceId } = await enrolledDevice(app, "purgeable@example.com");
    await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: { events: [event()] },
    });

    const stored = await db.query<{ consent_id: string | null; annotator_id: string | null }>(
      `SELECT consent_id, annotator_id FROM client_events WHERE device_id = $1`,
      [deviceId],
    );
    expect(stored.rows[0]?.annotator_id).not.toBeNull();
    expect(stored.rows[0]?.consent_id).not.toBeNull();

    // The sweep a consent-scoped purge job would run.
    const swept = await db.query<{ event_id: string }>(
      `UPDATE client_events
          SET retention_state = 'purged', purged_at = transaction_timestamp(), details = '{}'::jsonb
        WHERE consent_id = $1 AND retention_state <> 'purged'
        RETURNING event_id`,
      [stored.rows[0]?.consent_id],
    );
    expect(swept.rows).toHaveLength(1);
    await app.close();
  });

  // docs/23 §3.2: every kind has a strict details contract with a
  // schema_version literal — unknown fields and unstamped payloads are refused
  // at the boundary, not stored and rendered by surprise.
  it("refuses details that miss the schema_version stamp or smuggle unknown fields", async () => {
    const { app } = harness();
    const { deviceToken } = await enrolledDevice(app, "strictdetails@example.com");

    for (const details of [
      {},
      { schema_version: "dm-client-app-started-1", smuggled: "field" },
      { schema_version: "dm-client-fault-1" },
    ]) {
      const response = await app.inject({
        method: "POST",
        url: "/app/v1/client-events",
        headers: { authorization: `Bearer ${deviceToken}` },
        payload: { events: [event({ details })] },
      });
      expect(response.statusCode).toBe(400);
    }
    await app.close();
  });

  // The motivating RCA shape (docs/23 §0): preflight failures must arrive with
  // the fields the warehouse groups by.
  it("accepts a preflight_failed event with the RCA fields", async () => {
    const { app } = harness();
    const { deviceToken, deviceId } = await enrolledDevice(app, "preflight@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: {
        events: [
          event({
            kind: "preflight_failed",
            severity: "error",
            details: {
              schema_version: "dm-client-preflight-1",
              failureCode: "HARDWARE_PATH_PREFLIGHT_FAILED",
              checkId: "gpu_encoder_path",
              windowsBuild: "22631",
              gpuVendor: "0x10de",
              driverVersion: "537.13",
              retryable: true,
              catalogMode: "dev_non_trainable",
              consentPresent: false,
            },
          }),
        ],
      },
    });

    expect(response.statusCode).toBe(202);
    const stored = await db.query<{ details: { failureCode: string; windowsBuild: string } }>(
      `SELECT details FROM client_events WHERE device_id = $1 AND kind = 'preflight_failed'`,
      [deviceId],
    );
    expect(stored.rows[0]?.details).toMatchObject({
      failureCode: "HARDWARE_PATH_PREFLIGHT_FAILED",
      windowsBuild: "22631",
    });
    await app.close();
  });

  // docs/23 §5: the event insert and its export outbox row are one statement,
  // so a replayed event id produces neither a second event nor a second job.
  it("produces exactly one export outbox row per event, replay included", async () => {
    const { app } = harness();
    const { deviceToken } = await enrolledDevice(app, "outbox@example.com");
    const single = event();

    for (let attempt = 0; attempt < 2; attempt += 1) {
      await app.inject({
        method: "POST",
        url: "/app/v1/client-events",
        headers: { authorization: `Bearer ${deviceToken}` },
        payload: { events: [single] },
      });
    }

    const jobs = await db.query<{ topic: string; payload: { kind: string }; status: string }>(
      `SELECT topic, payload, status FROM outbox_jobs WHERE event_id = $1`,
      [single.eventId],
    );
    expect(jobs.rows).toHaveLength(1);
    expect(jobs.rows[0]).toMatchObject({
      topic: "client_event.recorded",
      status: "pending",
      payload: { kind: "app_started" },
    });
    await app.close();
  });

  it("rejects a batch larger than the cap instead of accepting it unbounded", async () => {
    const { app } = harness();
    const { deviceToken } = await enrolledDevice(app, "toobig@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/client-events",
      headers: { authorization: `Bearer ${deviceToken}` },
      payload: { events: Array.from({ length: 101 }, () => event()) },
    });

    expect(response.statusCode).toBe(400);
    await app.close();
  });
});

describe("surface wiring", () => {
  it("serves no annotator route at all when the service is absent", async () => {
    const app = buildApp({
      config: configFor(),
      repository: { ping: async () => undefined, missingConsoleGrants: async () => [] } as unknown as ConsoleRepository,
      browse: {} as BrowseRepository,
      governance: {} as GovernanceRepository,
      preview: { available: false } as PreviewService,
      mailer,
    });

    const response = await app.inject({ method: "POST", url: "/app/v1/login", payload: {} });
    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(response.statusCode).toBe(404);
    // Reported rather than left ambiguous: a deployment can tell whether the
    // public surface is live.
    expect(ready.json().features).toMatchObject({ annotatorApi: false });
    await app.close();
  });

  it("reports the annotator surface when it is wired", async () => {
    const { app } = harness();
    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(ready.json().features).toMatchObject({ annotatorApi: true });
    await app.close();
  });
});

// Two gaps that made the surface unusable from a real client. Both are about a
// dead end the client could not get out of on its own.
describe("client onboarding", () => {
  it("serves the terms the client has to display and confirm", async () => {
    const { app } = harness();

    const response = await app.inject({ method: "GET", url: "/app/v1/terms" });

    expect(response.statusCode).toBe(200);
    expect(response.json()).toEqual({
      policyVersion: terms.policyVersion,
      purpose: terms.purpose,
      termsHash: terms.termsHash,
      body: terms.body,
      // Served so the signup UI can show whether accepting these terms
      // authorizes trainable use (docs/49 E2); the value is still recorded
      // server-side only.
      allowsTrainableUse: false,
      // docs/108 §3.2: what this document authorizes, so a client can name the
      // difference between what someone signed and what is served now.
      scopes: terms.scopes,
    });
    await app.close();
  });

  // Without this the client would have to hard-code the version, and every
  // published build would break the day the terms change.
  it("serves the version signup will accept, before any account exists", async () => {
    const { app } = harness();
    const served = (await app.inject({ method: "GET", url: "/app/v1/terms" })).json();

    const code = await insertInvite();
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: { ...signupBody(code, "onboarding@example.com"), acceptedPolicyVersion: served.policyVersion },
    });

    expect(signup.statusCode).toBe(202);
    await app.close();
  });

  // Enrollment is idempotent per machine and replays with token: null, so a
  // client that lost its credential had no way back — re-enrolling replays, and
  // operator-side rotation is on a surface it cannot reach.
  it("re-issues a credential to the device's owner", async () => {
    const { app } = harness();
    const token = await register(app, "rotate-owner@example.com");
    const enrolled = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${token}` },
      payload: enrollBody("machine-rotate"),
    });
    const { deviceId, token: first } = enrolled.json();

    const replay = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${token}` },
      payload: enrollBody("machine-rotate"),
    });
    expect(replay.json().token).toBeNull(); // the dead end

    const rotated = await app.inject({
      method: "POST",
      url: `/app/v1/devices/${deviceId}/credential`,
      headers: { authorization: `Bearer ${token}` },
    });

    expect(rotated.statusCode).toBe(201);
    expect(rotated.json().token).not.toBe(first);
    // Only the hash is kept, and it is the new one — the old credential is gone.
    const stored = await db.query<{ credential_fingerprint: string }>(
      `SELECT credential_fingerprint FROM devices WHERE id = $1`,
      [deviceId],
    );
    const digest = createHash("sha256").update(rotated.json().token as string).digest("hex");
    expect(stored.rows[0]?.credential_fingerprint).toBe(digest);
    await app.close();
  });

  it("refuses to rotate a device that belongs to someone else", async () => {
    const { app } = harness();
    const owner = await register(app, "rotate-a@example.com");
    const stranger = await register(app, "rotate-b@example.com");
    const enrolled = await app.inject({
      method: "POST",
      url: "/app/v1/devices/enroll",
      headers: { authorization: `Bearer ${owner}` },
      payload: enrollBody("machine-rotate-owned"),
    });

    const attempt = await app.inject({
      method: "POST",
      url: `/app/v1/devices/${enrolled.json().deviceId}/credential`,
      headers: { authorization: `Bearer ${stranger}` },
    });

    expect(attempt.statusCode).toBe(403);
    expect(attempt.json()).toMatchObject({ code: "device_unavailable" });
    await app.close();
  });

  it("requires a session to rotate", async () => {
    const { app } = harness();
    const response = await app.inject({
      method: "POST",
      url: `/app/v1/devices/${randomUUID()}/credential`,
    });
    expect(response.statusCode).toBe(401);
    await app.close();
  });
});

/**
 * Password reset (11 §7.3 gap). Before this flow existed a forgotten annotator
 * password was terminal: there is no operator-side reset for annotator
 * credentials, so the account and every device enrolled under it were lost.
 *
 * These pin the three properties that make an unauthenticated reset route safe
 * — it cannot be used to discover which addresses are registered, one code buys
 * exactly one password, and the reset does not leave old sessions alive.
 */
describe("annotator password reset", () => {
  const newPassword = "a whole new correct horse";

  it("lets a forgotten password be replaced with a mailed code", async () => {
    const { app } = harness();
    await register(app, "forgot@example.com");

    const request = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "forgot@example.com" },
    });
    expect(request.statusCode).toBe(202);

    const confirm = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/confirm",
      payload: { challengeId: request.json().challengeId, code: lastCode(), password: newPassword },
    });
    expect(confirm.statusCode).toBe(204);

    // The new password works and the old one no longer does.
    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/login",
          payload: { email: "forgot@example.com", password: newPassword },
        })
      ).statusCode,
    ).toBe(200);
    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/login",
          payload: { email: "forgot@example.com", password: "correct horse battery staple" },
        })
      ).statusCode,
    ).toBe(401);
    await app.close();
  });

  it("answers an unregistered address exactly like a registered one", async () => {
    const { app } = harness();
    await register(app, "known@example.com");

    const known = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "known@example.com" },
    });
    const unknown = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "nobody@example.com" },
    });

    // Same status and same body shape: this route must not be an address oracle.
    expect(unknown.statusCode).toBe(known.statusCode);
    expect(Object.keys(unknown.json()).sort()).toEqual(Object.keys(known.json()).sort());
    expect(unknown.json().challengeId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/,
    );
    // …and no mail went to an address that has no account.
    expect(outbox.at(-1)?.to).toBe("known@example.com");
    await app.close();
  });

  it("spends the code once and refuses a wrong one", async () => {
    const { app } = harness();
    await register(app, "once@example.com");
    const request = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "once@example.com" },
    });
    const challengeId = request.json().challengeId;
    const mailed = lastCode();

    const wrong = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/confirm",
      payload: { challengeId, code: mailed === "000000" ? "111111" : "000000", password: newPassword },
    });
    expect(wrong.statusCode).toBe(401);

    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/password-reset/confirm",
          payload: { challengeId, code: mailed, password: newPassword },
        })
      ).statusCode,
    ).toBe(204);
    // Replay of a spent code sets no second password.
    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/password-reset/confirm",
          payload: { challengeId, code: mailed, password: "yet another password here" },
        })
      ).statusCode,
    ).toBe(401);
    await app.close();
  });

  it("revokes sessions minted with the old password", async () => {
    const { app } = harness();
    const token = await register(app, "sessions@example.com");
    // The session works before the reset.
    expect(
      (
        await app.inject({
          method: "GET",
          url: "/app/v1/session",
          headers: { authorization: `Bearer ${token}` },
        })
      ).statusCode,
    ).toBe(200);

    const request = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "sessions@example.com" },
    });
    await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/confirm",
      payload: { challengeId: request.json().challengeId, code: lastCode(), password: newPassword },
    });

    // Whoever held that token no longer holds anything.
    expect(
      (
        await app.inject({
          method: "GET",
          url: "/app/v1/session",
          headers: { authorization: `Bearer ${token}` },
        })
      ).statusCode,
    ).toBe(401);
    await app.close();
  });

  it("refuses to activate an account that never verified its address", async () => {
    const { app } = harness();
    const code = await insertInvite();
    await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: signupBody(code, "pending@example.com"),
    });

    // A pending account gets the oracle-safe answer, and no reset mail: signup
    // verification is the only way to activate, so a reset must not become a
    // second, quieter activation path.
    const request = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "pending@example.com" },
    });
    expect(request.statusCode).toBe(202);
    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/password-reset/confirm",
          payload: { challengeId: request.json().challengeId, code: lastCode(), password: newPassword },
        })
      ).statusCode,
    ).toBe(401);
    await app.close();
  });

  it("clears the lockout so a locked-out user can actually get back in", async () => {
    const { app } = harness({ limits: { loginPerEmailHour: 100 } });
    await register(app, "locked@example.com");
    // Enough failures to trip the lockout.
    for (let attempt = 0; attempt < 12; attempt += 1) {
      await app.inject({
        method: "POST",
        url: "/app/v1/login",
        payload: { email: "locked@example.com", password: "wrong password entirely" },
      });
    }

    const request = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "locked@example.com" },
    });
    await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/confirm",
      payload: { challengeId: request.json().challengeId, code: lastCode(), password: newPassword },
    });

    // A reset that left the account locked would be no recovery at all.
    expect(
      (
        await app.inject({
          method: "POST",
          url: "/app/v1/login",
          payload: { email: "locked@example.com", password: newPassword },
        })
      ).statusCode,
    ).toBe(200);
    await app.close();
  });
});

/**
 * 绑定前的签名 catalog 启动（issue #323）。
 *
 * 这条路存在的唯一理由是打断一个启动循环：绑定要已批准的设备配置 → 设备配置来自
 * 签名 catalog → 签名 catalog 要设备凭据 → 设备凭据要绑定成功。所以这里既要证明
 * 它**能**在没有任何设备凭据时把目录交出去，也要证明它没有因此变成一个谁都能拉的
 * 公开端点。
 */
describe("pre-enrollment catalog bootstrap", () => {
  // 这一组里「有没有发布」本身就是被测状态，所以每个用例都从一张空的发布表开始。
  // 文件级 beforeEach 只清限流表——其余表跨用例累积是这个套件的既有约定。
  beforeEach(async () => {
    await db.query("TRUNCATE catalog_releases CASCADE");
  });

  const envelope = Buffer.from(
    JSON.stringify({ schemaVersion: "dm-signed-catalog-bundle-1", manifest: "e30=" }),
  );
  const envelopeSha256 = `sha256:${createHash("sha256").update(envelope).digest("hex")}`;
  const currentHeaders = {
    "x-dataminer-catalog-schema-max": "2",
    "x-dataminer-client-version": "0.7.0",
  };

  // dev 是唯一渠道（certified 发布侧随 issue #829 收口，docs/100 §11）。这组用例
  // 钉的是下发机制——鉴权、兼容闸、ETag、草稿不外泄——与渠道无关；用真实存在的
  // 那条渠道铸行，免得测试替一条谁都装不上的渠道背书。
  async function publishDevRelease(version = "2026-07-29.1", catalogId = "dataminer-call-of-duty-steam-release-test") {
    await db.query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version, manifest_bytes,
         envelope_bytes, envelope_sha256, signing_key_id, status, created_by,
         published_by, published_at
       ) VALUES ($1, 'dev', $5, $2,
                 $3, $3, $4, 'dm-dev-catalog-delivery-v1', 'published', 'test', 'test', now())`,
      [randomUUID(), version, envelope, envelopeSha256, catalogId],
    );
  }

  it.each([
    ["2026-09-10.0", "0.7.20", "0.7.21"],
    ["2026-09-11.0", "0.7.23", "0.7.24"],
  ])("filters bootstrap catalog %s for %s and delivers it to %s", async (catalogVersion, oldVersion, currentVersion) => {
    const { app } = harness();
    const token = await register(app, `bootstrap-kcd2-${oldVersion.replaceAll(".", "-")}@example.com`);
    const oldHeaders = { authorization: `Bearer ${token}`, ...currentHeaders, "x-dataminer-client-version": oldVersion };
    await publishDevRelease();
    const before = await app.inject({ method: "GET", url: "/app/v1/catalog/bootstrap", headers: oldHeaders });
    expect(before.statusCode).toBe(200);
    await publishDevRelease(catalogVersion, "dataminer-kingdom-come-deliverance-2-steam");
    const after = await app.inject({ method: "GET", url: "/app/v1/catalog/bootstrap", headers: oldHeaders });
    expect(after.statusCode).toBe(200);
    expect(after.rawPayload).toEqual(before.rawPayload);
    expect(after.headers.etag).toBe(before.headers.etag);
    const cached = await app.inject({ method: "GET", url: "/app/v1/catalog/bootstrap", headers: { ...oldHeaders, "if-none-match": before.headers.etag as string } });
    expect(cached.statusCode).toBe(304);
    const current = await app.inject({ method: "GET", url: "/app/v1/catalog/bootstrap", headers: { ...oldHeaders, "x-dataminer-client-version": currentVersion, "if-none-match": before.headers.etag as string } });
    expect(current.statusCode).toBe(200);
    expect(current.json().bundles).toHaveLength(2);
    for (const entry of current.json().bundles) expect(Buffer.from(entry.envelope, "base64")).toEqual(envelope);
    const invalid = await app.inject({ method: "GET", url: "/app/v1/catalog/bootstrap", headers: { ...oldHeaders, "x-dataminer-client-version": "bad-version", "if-none-match": "*" } });
    expect(invalid.statusCode).toBe(409);
    await app.close();
  });

  it("serves the published envelope to a signed-in annotator that has no device yet", async () => {
    const { app } = harness();
    const token = await register(app, "bootstrap@example.com");
    await publishDevRelease();

    // 前提坐实：这个账号一台设备都没有，也就没有任何设备凭据。
    const devices = await app.inject({
      method: "GET",
      url: "/app/v1/devices",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(devices.json().devices).toHaveLength(0);

    const response = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization: `Bearer ${token}`, ...currentHeaders },
    });

    expect(response.statusCode).toBe(200);
    // 集合下发（docs/57 §3.3）。**原样字节**这条要求没变，只是下沉了一层：
    // 集合里嵌的那份 envelope 必须与库里逐字节相同，客户端按它验签。
    const set = response.json();
    expect(set.schemaVersion).toBe("dm-signed-catalog-set-1");
    expect(set.bundles).toHaveLength(1);
    expect(set.bundles[0].catalogId).toBe("dataminer-call-of-duty-steam-release-test");
    expect(set.bundles[0].catalogVersion).toBe("2026-07-29.1");
    expect(Buffer.from(set.bundles[0].envelope, "base64").equals(envelope)).toBe(true);
    expect(response.headers["x-dataminer-catalog-count"]).toBe("1");
    // console 的 onSend 把 cache-control 统一改成 no-store，盖掉下发模块的
    // private, no-cache。钉住实际行为，免得有人对着路由代码找一个不存在的差异。
    expect(response.headers["cache-control"]).toBe("no-store");
    await app.close();
  });

  it("requires a session — it is not a public endpoint", async () => {
    const { app } = harness();
    await publishDevRelease();

    const anonymous = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: currentHeaders,
    });
    expect(anonymous.statusCode).toBe(401);
    expect(anonymous.json()).toMatchObject({ code: "session_required" });

    const forged = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization: "Bearer not-a-real-session", ...currentHeaders },
    });
    expect(forged.statusCode).toBe(401);
    await app.close();
  });

  it("fails closed when nothing is published, rather than inventing an empty catalog", async () => {
    const { app } = harness();
    const token = await register(app, "unpublished@example.com");

    const response = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization: `Bearer ${token}`, ...currentHeaders },
    });

    expect(response.statusCode).toBe(503);
    expect(response.json()).toMatchObject({ code: "catalog_not_published", retryable: true });
    await app.close();
  });

  it("applies the same compatibility gate as the device-credential path", async () => {
    const { app } = harness();
    const token = await register(app, "incompatible@example.com");
    await publishDevRelease();
    const authorization = `Bearer ${token}`;

    const missingHeader = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization, "x-dataminer-client-version": "0.7.0" },
    });
    expect(missingHeader.statusCode).toBe(409);
    expect(missingHeader.json()).toMatchObject({ code: "catalog_incompatible", retryable: false });

    const tooOld = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization, ...currentHeaders, "x-dataminer-client-version": "0.4.9" },
    });
    expect(tooOld.statusCode).toBe(409);
    await app.close();
  });

  it("honours If-None-Match so the post-enrollment refresh keeps its cache", async () => {
    const { app } = harness();
    const token = await register(app, "etag@example.com");
    await publishDevRelease();
    const authorization = `Bearer ${token}`;
    // ETag 现在覆盖**整个集合**的字节，不再是单份 envelope 的 sha256
    // （docs/57 §3.3）。两条下发路共用 catalog-delivery，算法必须同源。
    const etag = catalogSetEtag(
      serializeCatalogSet([
        {
          catalogId: "dataminer-call-of-duty-steam-release-test",
          catalogVersion: "2026-07-29.1",
          envelopeBytes: envelope,
          envelopeSha256,
        },
      ]),
    );

    const notModified = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization, ...currentHeaders, "if-none-match": etag },
    });
    expect(notModified.statusCode).toBe(304);
    expect(notModified.rawPayload).toHaveLength(0);

    // 版本太老的客户端即使 ETag 命中也必须收到 409：否则它会以为手上那份仍然当前。
    const staleClient = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: {
        authorization,
        ...currentHeaders,
        "x-dataminer-client-version": "0.4.9",
        "if-none-match": etag,
      },
    });
    expect(staleClient.statusCode).toBe(409);
    await app.close();
  });

  it("never returns a draft release", async () => {
    const { app } = harness();
    const token = await register(app, "draftrelease@example.com");
    await db.query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version, manifest_bytes,
         envelope_bytes, envelope_sha256, signing_key_id, status, created_by
       ) VALUES ($1, 'dev', 'dataminer-call-of-duty-steam-release-test', '2026-07-29.9',
                 $2, $2, $3, 'dm-dev-catalog-delivery-v1', 'draft', 'test')`,
      [randomUUID(), envelope, envelopeSha256],
    );

    const response = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization: `Bearer ${token}`, ...currentHeaders },
    });
    expect(response.statusCode).toBe(503);
    await app.close();
  });

  it("is read-only: it mints no credential and enrolls no device", async () => {
    const { app } = harness();
    const token = await register(app, "readonly@example.com");
    await publishDevRelease();

    const before = await db.query<{ count: string }>("SELECT count(*)::text AS count FROM devices");
    const response = await app.inject({
      method: "GET",
      url: "/app/v1/catalog/bootstrap",
      headers: { authorization: `Bearer ${token}`, ...currentHeaders },
    });
    expect(response.statusCode).toBe(200);
    // 拿到目录不等于拿到授权：这条路不签发任何凭据，body 里也不该出现 token 字样。
    expect(response.json()).not.toHaveProperty("token");
    const after = await db.query<{ count: string }>("SELECT count(*)::text AS count FROM devices");
    expect(after.rows[0]!.count).toBe(before.rows[0]!.count);
    await app.close();
  });
});

/**
 * Re-signing the current terms (docs/34). A consent used to be captured once at
 * signup and frozen: a new terms version, or the same document with a different
 * deployment answer on trainable use, left the person with no self-service path
 * at all. These pin what makes the fix safe — history is added to, never
 * rewritten, because sessions already captured pin the consent that was live
 * for them.
 */
describe("annotator consent renewal", () => {
  async function activeConsents(annotatorEmail: string) {
    const r = await db.query(
      `SELECT co.id, co.status, co.terms_hash, co.allows_trainable_use, co.revoke_reason,
              co.revoked_at, co.superseded_at, co.superseded_by, co.scopes
         FROM consents co JOIN annotator_credentials cr ON cr.annotator_id = co.annotator_id
        WHERE cr.email = $1 ORDER BY co.created_at`,
      [annotatorEmail],
    );
    return r.rows as Array<{
      id: string; status: string; terms_hash: string;
      allows_trainable_use: boolean; revoke_reason: string | null;
      revoked_at: Date | null; superseded_at: Date | null; superseded_by: string | null;
      scopes: string[];
    }>;
  }

  it("reports whether the stored consent still matches what is served", async () => {
    const { app } = harness();
    const token = await register(app, "consentstate@example.com");

    const response = await app.inject({
      method: "GET",
      url: "/app/v1/consent",
      headers: { authorization: `Bearer ${token}` },
    });

    expect(response.statusCode).toBe(200);
    const body = response.json();
    expect(body.consent.termsHash).toBe(terms.termsHash);
    expect(body.currentPolicyVersion).toBe(terms.policyVersion);
    expect(body.matchesCurrentTerms).toBe(true);
    await app.close();
  });

  it("adds a consent and supersedes the old one rather than revoking or rewriting it", async () => {
    // Signed under one document, then the deployment serves a different one.
    const { app } = harness();
    const token = await register(app, "renew@example.com");
    const before = await activeConsents("renew@example.com");
    expect(before).toHaveLength(1);

    const { app: updated } = harness({
      terms: {
        ...terms,
        policyVersion: "2026-09-01",
        termsHash: "b".repeat(64),
        allowsTrainableUse: true,
        scopes: ["audio", "input", "video"],
      },
    });
    const stale = await updated.inject({
      method: "GET", url: "/app/v1/consent", headers: { authorization: `Bearer ${token}` },
    });
    expect(stale.json().matchesCurrentTerms).toBe(false);
    // The point of re-signing, stated as a difference the caller can render:
    // the new document adds audio to what this person already agreed to.
    expect(stale.json().pendingScopes).toEqual(["audio"]);

    const renewed = await updated.inject({
      method: "POST",
      url: "/app/v1/consent",
      headers: { authorization: `Bearer ${token}` },
      payload: { acceptedPolicyVersion: "2026-09-01" },
    });
    expect(renewed.statusCode).toBe(201);

    const after = await activeConsents("renew@example.com");
    expect(after).toHaveLength(2);
    // The original row is intact apart from being superseded — its terms_hash
    // and its recorded answer are exactly what they were.
    const original = after.find((row) => row.id === before[0]!.id)!;
    expect(original.terms_hash).toBe(before[0]!.terms_hash);
    expect(original.allows_trainable_use).toBe(before[0]!.allows_trainable_use);
    expect(original.scopes).toEqual(before[0]!.scopes);
    // docs/108 D5: superseding is not withdrawal. Recording it as `revoked`
    // made every session captured under the old document read as
    // consent-withdrawn the moment its owner re-signed to widen authorization,
    // and left purge unable to tell a real withdrawal from a version bump.
    expect(original.status).toBe("expired");
    expect(original.revoked_at).toBeNull();
    expect(original.revoke_reason).toBeNull();
    expect(original.superseded_at).not.toBeNull();
    // The new one carries the current document, and the old row names it.
    const fresh = after.find((row) => row.status === "active")!;
    expect(fresh.terms_hash).toBe("b".repeat(64));
    expect(fresh.allows_trainable_use).toBe(true);
    expect(fresh.scopes).toEqual(["audio", "input", "video"]);
    expect(original.superseded_by).toBe(fresh.id);

    // Nothing left to prompt about once it is signed.
    const current = await updated.inject({
      method: "GET", url: "/app/v1/consent", headers: { authorization: `Bearer ${token}` },
    });
    expect(current.json().resignRequired).toBe(false);
    expect(current.json().pendingScopes).toEqual([]);
    await app.close();
    await updated.close();
  });

  it("is a no-op when the stored consent is already current", async () => {
    const { app } = harness();
    const token = await register(app, "noop@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/consent",
      headers: { authorization: `Bearer ${token}` },
      payload: { acceptedPolicyVersion: terms.policyVersion },
    });

    // 200, not 201: nothing was written, and re-signing the identical document
    // must not pile up rows.
    expect(response.statusCode).toBe(200);
    expect(response.json().renewed).toBe(false);
    expect(await activeConsents("noop@example.com")).toHaveLength(1);
    await app.close();
  });

  it("refuses a client that is displaying stale terms", async () => {
    const { app } = harness();
    const token = await register(app, "staleview@example.com");

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/consent",
      headers: { authorization: `Bearer ${token}` },
      payload: { acceptedPolicyVersion: "some-version-the-server-does-not-serve" },
    });

    // Recording consent against a document the user was not shown would be
    // recording a lie — the same guard signup uses.
    expect(response.statusCode).toBe(400);
    expect(response.json().code).toBe("terms_out_of_date");
    expect(await activeConsents("staleview@example.com")).toHaveLength(1);
    await app.close();
  });

  it("requires a session", async () => {
    const { app } = harness();

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/consent",
      payload: { acceptedPolicyVersion: terms.policyVersion },
    });

    expect(response.statusCode).toBe(401);
    await app.close();
  });
});

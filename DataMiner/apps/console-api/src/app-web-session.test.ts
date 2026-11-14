import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import { generate, normalize } from "./app-invites.js";
import { PostgresAppRepository } from "./app-repository.js";
import { AppService, type TermsSnapshot } from "./app-service.js";
import type { ConsoleConfig } from "./config.js";
import type { ConsoleRepository } from "./domain.js";
import type { BrowseRepository } from "./browse-domain.js";
import type { GovernanceRepository } from "./governance-domain.js";
import type { ConsoleMail, Mailer } from "./mailer.js";
import type { PreviewService } from "./preview.js";
import type { SqlClient } from "./sql.js";
import { argon2TestTimeoutMs, StubBrowseRepository } from "./testing.js";

// Every case here signs in, and the password-change cases hash again. See
// argon2TestTimeoutMs for why the budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

// The two-transport authentication matrix of docs/34 §6.1: the desktop bearer
// path must be byte-for-byte unchanged (no CSRF, no cookies), while the browser
// cookie path gets the console's full CSRF discipline plus sliding renewal.
// All of it runs against a real PostgreSQL engine.
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

const SLIDING_DAYS = 30;
const WINDOW_SECONDS = SLIDING_DAYS * 86_400;

let db: PGlite;
let operatorId: string;
let outbox: ConsoleMail[];

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
    appSession: { slidingDays: SLIDING_DAYS },
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
  const appService = new AppService(repository, mailer, config, terms);
  // 自视图路由（/app/v1/contribution、/app/v1/readiness 的授权灯）走 browse——
  // 这里给 stub：它记录调用主体，正好用来断言自视图只以会话本人为查询主体。
  const browse: BrowseRepository = new StubBrowseRepository();
  const app = buildApp({
    config,
    repository: { ping: async () => undefined } as unknown as ConsoleRepository,
    browse,
    governance: {} as GovernanceRepository,
    preview: { available: false } as PreviewService,
    mailer,
    appService,
  });
  return { app, repository, browse: browse as StubBrowseRepository };
}

const password = "correct horse battery staple";

function lastCode(): string {
  const body = outbox.at(-1)?.body ?? "";
  return /([0-9]{6})/.exec(body)?.[1] ?? "";
}

/** Signup + verify through the public endpoints; returns the address. */
async function register(app: ReturnType<typeof harness>["app"]) {
  const display = generate();
  await db.query(`INSERT INTO invite_codes(code, created_by) VALUES ($1, $2)`, [
    normalize(display),
    operatorId,
  ]);
  const email = `web-${randomUUID().slice(0, 8)}@example.com`;
  const signup = await app.inject({
    method: "POST",
    url: "/app/v1/signup",
    payload: {
      inviteCode: display,
      email,
      password,
      alias: "Web Tester",
      acceptedPolicyVersion: terms.policyVersion,
    },
  });
  expect(signup.statusCode).toBe(202);
  const verify = await app.inject({
    method: "POST",
    url: "/app/v1/signup/verify",
    payload: { challengeId: signup.json().challengeId, code: lastCode() },
  });
  expect(verify.statusCode).toBe(204);
  return email;
}

const webHeaders = { "x-dma-csrf": "1" };

async function webLogin(app: ReturnType<typeof harness>["app"], email: string) {
  const login = await app.inject({
    method: "POST",
    url: "/app/v1/web/login",
    headers: webHeaders,
    payload: { email, password },
  });
  expect(login.statusCode).toBe(200);
  const cookie = String(login.headers["set-cookie"]);
  const token = decodeURIComponent(cookie.slice(cookie.indexOf("=") + 1, cookie.indexOf(";")));
  return { cookieHeader: `__Host-dma_session=${encodeURIComponent(token)}`, token, raw: cookie };
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
  await db.query("TRUNCATE console_rate_limits");
});

afterAll(async () => {
  await db?.close();
});

describe("web login cookie", () => {
  it("sets an HttpOnly host-scoped cookie and never puts the token in the body", async () => {
    const { app } = harness();
    const email = await register(app);

    const login = await app.inject({
      method: "POST",
      url: "/app/v1/web/login",
      headers: webHeaders,
      payload: { email, password },
    });
    expect(login.statusCode).toBe(200);
    const cookie = String(login.headers["set-cookie"]);
    expect(cookie).toContain("__Host-dma_session=");
    expect(cookie).toContain("HttpOnly");
    expect(cookie).toContain("SameSite=Strict");
    expect(cookie).toContain(`Max-Age=${WINDOW_SECONDS}`);
    expect(login.json()).toEqual({ expiresInSeconds: WINDOW_SECONDS });
    const token = decodeURIComponent(cookie.slice(cookie.indexOf("=") + 1, cookie.indexOf(";")));
    expect(login.body).not.toContain(token);
  });

  it("refuses the web sign-in without the CSRF header", async () => {
    const { app } = harness();
    const email = await register(app);
    const login = await app.inject({
      method: "POST",
      url: "/app/v1/web/login",
      payload: { email, password },
    });
    expect(login.statusCode).toBe(403);
    expect(login.json()).toMatchObject({ code: "csrf_header_missing" });
  });

  it("authenticates reads by cookie and signs out to a dead cookie", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const session = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(session.statusCode).toBe(200);
    expect(session.json().annotatorId).toBeTruthy();

    const logout = await app.inject({
      method: "POST",
      url: "/app/v1/web/logout",
      headers: { ...webHeaders, cookie: cookieHeader },
    });
    expect(logout.statusCode).toBe(204);
    expect(String(logout.headers["set-cookie"])).toContain("Max-Age=0");

    const after = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(after.statusCode).toBe(401);
  });
});

describe("cookie writes carry the console CSRF discipline; bearer writes do not", () => {
  it("refuses a cookie write without the header and accepts it with the header", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const bare = await app.inject({
      method: "PUT",
      url: "/app/v1/profile",
      headers: { cookie: cookieHeader },
      payload: { displayName: "网页用户" },
    });
    expect(bare.statusCode).toBe(403);
    expect(bare.json()).toMatchObject({ code: "csrf_header_missing" });

    const withHeader = await app.inject({
      method: "PUT",
      url: "/app/v1/profile",
      headers: { ...webHeaders, cookie: cookieHeader },
      payload: { displayName: "网页用户" },
    });
    expect(withHeader.statusCode).toBe(200);
    expect(withHeader.json().profile).toMatchObject({ displayName: "网页用户" });
  });

  it("refuses a cross-site cookie write even with the header", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const crossSite = await app.inject({
      method: "PUT",
      url: "/app/v1/profile",
      headers: { ...webHeaders, cookie: cookieHeader, "sec-fetch-site": "cross-site" },
      payload: { displayName: "attacker" },
    });
    expect(crossSite.statusCode).toBe(403);
    expect(crossSite.json()).toMatchObject({ code: "cross_site_request" });
  });

  it("keeps the bearer transport exactly as it was: no CSRF header required", async () => {
    const { app } = harness();
    const email = await register(app);
    const login = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password },
    });
    const token = login.json().token as string;

    const update = await app.inject({
      method: "PUT",
      url: "/app/v1/profile",
      headers: { authorization: `Bearer ${token}` },
      payload: { displayName: "桌面客户端" },
    });
    expect(update.statusCode).toBe(200);
  });

  it("rejects a browser-evidenced cross-site post to a shared public endpoint", async () => {
    const { app } = harness();
    const crossSite = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      headers: { "sec-fetch-site": "cross-site" },
      payload: { email: "anyone@example.com" },
    });
    expect(crossSite.statusCode).toBe(403);

    // The same request without browser headers is the desktop client's shape.
    const desktop = await app.inject({
      method: "POST",
      url: "/app/v1/password-reset/request",
      payload: { email: "anyone@example.com" },
    });
    expect(desktop.statusCode).toBe(202);
  });
});

describe("sliding renewal (docs/34 §6.1a)", () => {
  it("pushes expiry back out and refreshes the cookie once half the window is spent", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader, token } = await webLogin(app, email);

    // Age the session artificially past the renewal threshold.
    await db.query(
      `UPDATE annotator_sessions
          SET expires_at = transaction_timestamp() + interval '1 day'`,
    );

    const renewed = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(renewed.statusCode).toBe(200);
    const refreshed = String(renewed.headers["set-cookie"]);
    expect(refreshed).toContain("__Host-dma_session=");
    expect(refreshed).toContain(`Max-Age=${WINDOW_SECONDS}`);
    // Same token, later expiry — a renewal, not a rotation.
    expect(decodeURIComponent(refreshed).replace(/;.*/, "")).toContain(token);

    const row = await db.query<{ seconds: number }>(
      `SELECT EXTRACT(EPOCH FROM (expires_at - transaction_timestamp()))::int AS seconds
         FROM annotator_sessions ORDER BY created_at DESC LIMIT 1`,
    );
    expect(row.rows[0]!.seconds).toBeGreaterThan(WINDOW_SECONDS - 60);
  });

  it("does not renew a fresh session, and never renews a revoked one", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const fresh = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(fresh.statusCode).toBe(200);
    expect(fresh.headers["set-cookie"]).toBeUndefined();

    await db.query(
      `UPDATE annotator_sessions SET revoked_at = transaction_timestamp(), revoke_reason = 'test'`,
    );
    const dead = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(dead.statusCode).toBe(401);
  });
});

describe("signed-in password change (docs/34 §6.3)", () => {
  it("changes with the current password, keeps the caller's session, kills the others", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);
    const otherLogin = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password },
    });
    const otherToken = otherLogin.json().token as string;

    const newPassword = "an entirely different secret";
    const change = await app.inject({
      method: "POST",
      url: "/app/v1/password",
      headers: { ...webHeaders, cookie: cookieHeader },
      payload: { currentPassword: password, newPassword },
    });
    expect(change.statusCode).toBe(204);

    // The caller keeps working; the other session is gone; only the new
    // password signs in.
    const self = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(self.statusCode).toBe(200);
    const other = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { authorization: `Bearer ${otherToken}` },
    });
    expect(other.statusCode).toBe(401);
    const relogin = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      payload: { email, password: newPassword },
    });
    expect(relogin.statusCode).toBe(200);
  });

  it("refuses a wrong current password and counts it toward the lockout ladder", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const wrong = await app.inject({
      method: "POST",
      url: "/app/v1/password",
      headers: { ...webHeaders, cookie: cookieHeader },
      payload: { currentPassword: "not the password", newPassword: "another long password" },
    });
    expect(wrong.statusCode).toBe(401);

    const failures = await db.query<{ failed_login_count: number }>(
      `SELECT failed_login_count FROM annotator_credentials WHERE email = $1`,
      [email],
    );
    expect(failures.rows[0]!.failed_login_count).toBe(1);
  });

  it("refuses a new password that contains the address", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const weak = await app.inject({
      method: "POST",
      url: "/app/v1/password",
      headers: { ...webHeaders, cookie: cookieHeader },
      payload: { currentPassword: password, newPassword: `xx${email}xx` },
    });
    expect(weak.statusCode).toBe(400);
    expect(weak.json()).toMatchObject({ code: "weak_password" });
  });
});

describe("signup carries the optional profile (docs/34 A12)", () => {
  it("persists profile fields submitted by the web form", async () => {
    const { app } = harness();
    const display = generate();
    await db.query(`INSERT INTO invite_codes(code, created_by) VALUES ($1, $2)`, [
      normalize(display),
      operatorId,
    ]);
    const email = `profiled-${randomUUID().slice(0, 8)}@example.com`;
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: {
        inviteCode: display,
        email,
        password,
        alias: "有资料的人",
        acceptedPolicyVersion: terms.policyVersion,
        profile: { displayName: "小李", region: "杭州" },
      },
    });
    expect(signup.statusCode).toBe(202);

    const row = await db.query<{ display_name: string; region: string }>(
      `SELECT p.display_name, p.region
         FROM annotator_profiles p
         JOIN annotator_credentials c ON c.annotator_id = p.annotator_id
        WHERE c.email = $1`,
      [email],
    );
    expect(row.rows[0]).toMatchObject({ display_name: "小李", region: "杭州" });
  });

  it("refuses a signup password containing the address", async () => {
    const { app } = harness();
    const display = generate();
    await db.query(`INSERT INTO invite_codes(code, created_by) VALUES ($1, $2)`, [
      normalize(display),
      operatorId,
    ]);
    const email = `weakpw-${randomUUID().slice(0, 8)}@example.com`;
    const signup = await app.inject({
      method: "POST",
      url: "/app/v1/signup",
      payload: {
        inviteCode: display,
        email,
        password: `xx${email}xx`,
        alias: "weak",
        acceptedPolicyVersion: terms.policyVersion,
      },
    });
    expect(signup.statusCode).toBe(400);
    expect(signup.json()).toMatchObject({ code: "weak_password" });
  });
});

// docs/66 批次 C：标注员自视图。四条断言分别钉住——身份摘要（J-45）、条款回执
// （J-44）、贡献查询的主体永远是会话本人（J-42）、就绪五灯的键集与初始明灭
// （J-14）。贡献与授权数走 StubBrowseRepository，身份/回执/设备走真库。
describe("annotator self-view (docs/66 batch C)", () => {
  it("returns an identity summary, not just the bare id", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const session = await app.inject({
      method: "GET",
      url: "/app/v1/session",
      headers: { cookie: cookieHeader },
    });
    expect(session.statusCode).toBe(200);
    expect(session.json()).toMatchObject({
      alias: "Web Tester",
      email,
      status: "active",
    });
    expect(session.json().annotatorId).toBeTruthy();
  });

  it("lists the signup consent as a receipt", async () => {
    const { app } = harness();
    const email = await register(app);
    const { cookieHeader } = await webLogin(app, email);

    const consents = await app.inject({
      method: "GET",
      url: "/app/v1/consents",
      headers: { cookie: cookieHeader },
    });
    expect(consents.statusCode).toBe(200);
    const receipts = consents.json().consents;
    expect(receipts).toHaveLength(1);
    expect(receipts[0]).toMatchObject({
      policyVersion: terms.policyVersion,
      termsHash: terms.termsHash,
      allowsTrainableUse: terms.allowsTrainableUse,
      revokedAt: null,
    });
  });

  it("scopes contribution to the session owner", async () => {
    const value = harness();
    const email = await register(value.app);
    const { cookieHeader } = await webLogin(value.app, email);
    const annotatorId = (
      await value.app.inject({
        method: "GET",
        url: "/app/v1/session",
        headers: { cookie: cookieHeader },
      })
    ).json().annotatorId;

    const contribution = await value.app.inject({
      method: "GET",
      url: "/app/v1/contribution?days=7",
      headers: { cookie: cookieHeader },
    });
    expect(contribution.statusCode).toBe(200);
    expect(contribution.json().totals).toBeDefined();
    expect(value.browse.calls).toContain(`contributionForAnnotator:${annotatorId}`);
  });

  it("renders the readiness lights from the annotator's own state", async () => {
    const value = harness();
    const email = await register(value.app);
    const { cookieHeader } = await webLogin(value.app, email);

    const readiness = await value.app.inject({
      method: "GET",
      url: "/app/v1/readiness",
      headers: { cookie: cookieHeader },
    });
    expect(readiness.statusCode).toBe(200);
    const steps = readiness.json().steps as { key: string; ok: boolean }[];
    // 「运营签发授权」那盏灯随 capture grant 车道退役（docs/100）。它留着只会是
    // 一盏永远红的灯——录制不再需要逐次授权凭证，那盏灯会告诉每一位标注员他们
    // 还不能开始采集，而这是假的。
    expect(steps.map((step) => step.key)).toEqual([
      "account",
      "consent",
      "device",
      "client_seen",
    ]);
    const byKey = new Map(steps.map((step) => [step.key, step.ok]));
    // 刚注册：账号与条款已亮，设备与客户端可见两盏还灭着。
    expect(byKey.get("account")).toBe(true);
    expect(byKey.get("consent")).toBe(true);
    expect(byKey.get("device")).toBe(false);
    expect(byKey.get("client_seen")).toBe(false);
  });
});

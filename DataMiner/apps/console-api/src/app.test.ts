import { randomUUID } from "node:crypto";

import { beforeAll, describe, expect, it, vi } from "vitest";

import { buildApp, errorDiagnostics } from "./app.js";
import { PreviewService } from "./preview.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import { sessionCookieName } from "./http.js";
import { hashPassword, hashSessionToken } from "./secrets.js";
import {
  argon2TestTimeoutMs,
  MemoryConsoleRepository,
  MemoryMailer,
  StubBrowseRepository,
  StubGovernanceRepository,
} from "./testing.js";

// Every signed-in case drives a real password login over HTTP, and the operator
// administration cases add a sudo re-verify. See argon2TestTimeoutMs for why the
// budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

const password = "correct horse battery staple";
let passwordHash = "";

// Pinned to the two-step flow: signedInCookie() below drives password → OTP, so
// this shared harness must stay in password_otp even though the config default is
// now password_only. The single-factor path has its own harness at the foot of
// this file.
const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "e".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
  CONSOLE_LOGIN_MODE: "password_otp",
});

const writeHeaders = { "content-type": "application/json", "x-dmc-csrf": "1" };

beforeAll(async () => {
  passwordHash = await hashPassword(password);
});

async function harness(
  role: "viewer" | "operator" | "admin" = "operator",
  catalogPublisher?: Parameters<typeof buildApp>[0]["catalogPublisher"],
  deploymentRevision?: string,
) {
  const repository = new MemoryConsoleRepository();
  const mailer = new MemoryMailer();
  const browse = new StubBrowseRepository();
  const governance = new StubGovernanceRepository();
  // No object credentials in the harness: preview must report itself unavailable
  // rather than half-working.
  const preview = new PreviewService(
    { query: async () => ({ rows: [] }) },
    null,
    config,
  );
  const app = buildApp({
    config,
    repository,
    browse,
    governance,
    preview,
    mailer,
    ...(deploymentRevision ? { deploymentRevision } : {}),
    ...(catalogPublisher ? { catalogPublisher } : {}),
  });
  const operator = await repository.createOperator({
    email: "ops@diogenesmodel.com",
    role,
    createdBy: null,
  });
  await repository.activateOperatorWithPassword(operator.id, passwordHash);
  return { app, repository, mailer, browse, governance, operatorId: operator.id };
}

async function signedInCookie(value: Awaited<ReturnType<typeof harness>>): Promise<string> {
  const login = await value.app.inject({
    method: "POST",
    url: "/console/v1/auth/login",
    headers: writeHeaders,
    payload: { email: "ops@diogenesmodel.com", password },
  });
  const verify = await value.app.inject({
    method: "POST",
    url: "/console/v1/auth/otp/verify",
    headers: writeHeaders,
    payload: { challengeId: login.json().challengeId, code: value.mailer.lastCode() },
  });
  const cookie = verify.headers["set-cookie"];
  if (typeof cookie !== "string") throw new Error("no session cookie was issued");
  const token = cookie.slice(cookie.indexOf("=") + 1, cookie.indexOf(";"));
  return `${sessionCookieName}=${token}`;
}

describe("console proxy-token channel gate", () => {
  const gatedConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "e".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_PROXY_TOKEN: "p".repeat(48),
  });

  function gatedApp() {
    const preview = new PreviewService({ query: async () => ({ rows: [] }) }, null, gatedConfig);
    return buildApp({
      config: gatedConfig,
      repository: new MemoryConsoleRepository(),
      browse: new StubBrowseRepository(),
      governance: new StubGovernanceRepository(),
      preview,
      mailer: new MemoryMailer(),
    });
  }

  it("answers 404, not 401, when a console request arrives without the proxy token", async () => {
    const app = gatedApp();
    const response = await app.inject({ method: "GET", url: "/console/v1/auth/session" });
    // 404 not 401: a direct caller must not be able to tell the route exists.
    expect(response.statusCode).toBe(404);
    await app.close();
  });

  it("answers 404 when the proxy token is wrong", async () => {
    const app = gatedApp();
    const response = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { "x-console-proxy-token": "wrong" },
    });
    expect(response.statusCode).toBe(404);
    await app.close();
  });

  it("passes the request through once the proxy token matches", async () => {
    const app = gatedApp();
    const response = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { "x-console-proxy-token": "p".repeat(48) },
    });
    // Past the gate, the ordinary session requirement applies unchanged.
    expect(response.statusCode).toBe(401);
    expect(response.json()).toMatchObject({ code: "session_required" });
    await app.close();
  });

  // The sign-in screen needs the login mode before a session exists, but the
  // endpoint must still sit behind the channel gate like every other
  // /console/v1 path.
  it("serves meta without a session but still behind the proxy gate", async () => {
    const app = gatedApp();

    const ungated = await app.inject({ method: "GET", url: "/console/v1/meta" });
    expect(ungated.statusCode).toBe(404);

    const gated = await app.inject({
      method: "GET",
      url: "/console/v1/meta",
      headers: { "x-console-proxy-token": "p".repeat(48) },
    });
    expect(gated.statusCode).toBe(200);
    expect(gated.json()).toMatchObject({ environment: "development", loginMode: "password_only" });
    await app.close();
  });

  it("leaves the Cloud Run health probes reachable without the token", async () => {
    const app = gatedApp();
    expect((await app.inject({ method: "GET", url: "/healthz" })).statusCode).toBe(200);
    expect((await app.inject({ method: "GET", url: "/readyz" })).statusCode).toBe(200);
    await app.close();
  });
});

describe("console HTTP boundary", () => {
  it("reports process identity and database readiness", async () => {
    const { app, repository } = await harness();

    const health = await app.inject({ method: "GET", url: "/healthz" });
    const ready = await app.inject({ method: "GET", url: "/readyz" });
    repository.pingFails = true;
    const degraded = await app.inject({ method: "GET", url: "/readyz" });

    expect(health.json()).toEqual({
      status: "ok",
      service: "dataminer-console-api",
      version: "0.1.0",
    });
    expect(ready.statusCode).toBe(200);
    expect(degraded.statusCode).toBe(503);
    expect(degraded.json()).toMatchObject({ code: "dependency_unavailable" });
    await app.close();
  });

  it("identifies the serving Cloud Run revision in readiness", async () => {
    const { app } = await harness(
      "operator",
      undefined,
      "dataminer-console-api-staging-00042-abc",
    );

    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({
      deployment: { revision: "dataminer-console-api-staging-00042-abc" },
    });
    await app.close();
  });

  it("requires a session for every non-public console route", async () => {
    const { app } = await harness();

    const response = await app.inject({ method: "GET", url: "/console/v1/auth/session" });

    expect(response.statusCode).toBe(401);
    expect(response.json()).toMatchObject({ code: "session_required" });
    await app.close();
  });

  it("rejects a state changing request without the console CSRF header", async () => {
    const { app } = await harness();

    const response = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: { "content-type": "application/json" },
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(response.statusCode).toBe(403);
    expect(response.json()).toMatchObject({ code: "csrf_header_missing" });
    await app.close();
  });

  it("rejects a state changing request from another origin", async () => {
    const { app } = await harness();

    const response = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: { ...writeHeaders, origin: "https://attacker.example" },
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(response.statusCode).toBe(403);
    expect(response.json()).toMatchObject({ code: "cross_site_request" });
    await app.close();
  });

  it("rejects a cross-site fetch even when the header is forged", async () => {
    const { app } = await harness();

    const response = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: { ...writeHeaders, "sec-fetch-site": "cross-site" },
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(response.statusCode).toBe(403);
    await app.close();
  });

  it("issues a host-prefixed, http-only, strict session cookie", async () => {
    const value = await harness();
    const login = await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password },
    });

    const verify = await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/otp/verify",
      headers: writeHeaders,
      payload: { challengeId: login.json().challengeId, code: value.mailer.lastCode() },
    });

    expect(login.statusCode).toBe(202);
    expect(verify.statusCode).toBe(200);
    const cookie = String(verify.headers["set-cookie"]);
    expect(cookie).toMatch(new RegExp(`^${sessionCookieName}=`));
    expect(cookie).toContain("HttpOnly");
    expect(cookie).toContain("Secure");
    expect(cookie).toContain("SameSite=Strict");
    expect(cookie).toContain("Path=/");
    expect(cookie).not.toContain("Domain=");
    await value.app.close();
  });

  it("never returns the password hash or lock state in an operator payload", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { cookie },
    });

    const body = response.json();
    expect(body.operator).toEqual({
      id: value.operatorId,
      email: "ops@diogenesmodel.com",
      role: "operator",
      status: "active",
      lastLoginAt: expect.any(String),
      createdAt: expect.any(String),
    });
    expect(JSON.stringify(body)).not.toContain("argon2");
    await value.app.close();
  });

  it("reports the capabilities the signed-in role actually has", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { cookie },
    });

    expect(response.json().capabilities).toEqual(["console.read"]);
    await value.app.close();
  });

  it("refuses operator administration to a non-admin session", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const list = await value.app.inject({
      method: "GET",
      url: "/console/v1/operators",
      headers: { cookie },
    });
    const invite = await value.app.inject({
      method: "POST",
      url: "/console/v1/operators",
      headers: { ...writeHeaders, cookie },
      payload: { email: "qa@diogenesmodel.com", role: "viewer" },
    });

    expect(list.statusCode).toBe(403);
    expect(invite.statusCode).toBe(403);
    expect(invite.json()).toMatchObject({ code: "insufficient_role" });
    await value.app.close();
  });

  it("lets an admin list and invite operators", async () => {
    const value = await harness("admin");
    const cookie = await signedInCookie(value);
    await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/sudo",
      headers: { ...writeHeaders, cookie },
      payload: { password },
    });

    const invite = await value.app.inject({
      method: "POST",
      url: "/console/v1/operators",
      headers: { ...writeHeaders, cookie },
      payload: { email: "qa@diogenesmodel.com", role: "viewer" },
    });
    const list = await value.app.inject({
      method: "GET",
      url: "/console/v1/operators",
      headers: { cookie },
    });

    expect(invite.statusCode).toBe(201);
    expect(invite.json().operator).toMatchObject({ email: "qa@diogenesmodel.com", status: "invited" });
    expect(list.json().operators).toHaveLength(2);
    await value.app.close();
  });

  // Creating and revoking governance access is itself a governance write, so it
  // sits behind the same step-up as purge and credential issuance. Listing does
  // not — reading the roster changes nothing.
  it("refuses operator administration without a step-up, but still allows listing", async () => {
    const value = await harness("admin");
    const cookie = await signedInCookie(value);

    const invite = await value.app.inject({
      method: "POST",
      url: "/console/v1/operators",
      headers: { ...writeHeaders, cookie },
      payload: { email: "qa@diogenesmodel.com", role: "viewer" },
    });
    const list = await value.app.inject({
      method: "GET",
      url: "/console/v1/operators",
      headers: { cookie },
    });

    expect(invite.statusCode).toBe(403);
    expect(invite.json()).toMatchObject({ code: "sudo_required" });
    expect(list.statusCode).toBe(200);
    await value.app.close();
  });

  it("clears the cookie and kills the session on logout", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const logout = await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/logout",
      headers: { "x-dmc-csrf": "1", cookie },
    });
    const afterwards = await value.app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { cookie },
    });

    expect({ status: logout.statusCode, body: logout.body }).toMatchObject({ status: 200 });
    expect(String(logout.headers["set-cookie"])).toContain("Max-Age=0");
    expect(afterwards.statusCode).toBe(401);
    await value.app.close();
  });

  it("rejects a forged session cookie", async () => {
    const value = await harness();

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { cookie: `${sessionCookieName}=${"f".repeat(43)}` },
    });

    expect(response.statusCode).toBe(401);
    expect(await value.repository.findSessionByTokenHash(hashSessionToken("f".repeat(43)), 3_600)).toMatchObject(
      { outcome: "missing" },
    );
    await value.app.close();
  });

  it("returns the same status for an unknown address and a wrong password", async () => {
    const { app } = await harness();

    const unknown = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "nobody@diogenesmodel.com", password },
    });
    const wrong = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password: "wrong password entirely" },
    });

    expect(unknown.statusCode).toBe(wrong.statusCode);
    expect(Object.keys(unknown.json()).sort()).toEqual(Object.keys(wrong.json()).sort());
    await app.close();
  });

  it("surfaces retry-after when a limit is hit", async () => {
    const { app } = await harness();

    await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password },
    });
    const limited = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(limited.statusCode).toBe(429);
    expect(Number(limited.headers["retry-after"])).toBeGreaterThan(0);
    await app.close();
  });

  it("rejects a body that is not json", async () => {
    const { app } = await harness();

    const response = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: { "content-type": "text/plain", "x-dmc-csrf": "1" },
      payload: "email=ops@diogenesmodel.com",
    });

    expect(response.statusCode).toBe(400);
    await app.close();
  });

  it("sets no-store and request id headers on every response", async () => {
    const { app } = await harness();

    const response = await app.inject({ method: "GET", url: "/healthz" });

    expect(response.headers["cache-control"]).toBe("no-store");
    expect(response.headers["x-request-id"]).toMatch(/^req_/);
    expect(response.headers["x-content-type-options"]).toBe("nosniff");
    await app.close();
  });
});

// docs/11 §4.3: the gate is a route-prefix gate, not a service gate. The desktop
// client reaches /app/v1 without the Worker, so gating the whole service would
// lock out the annotator surface.
describe("worker proxy gate", () => {
  const proxyToken = "p".repeat(48);
  const gatedConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "e".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_PROXY_TOKEN: proxyToken,
  });

  async function gatedHarness() {
    const repository = new MemoryConsoleRepository();
    const preview = new PreviewService({ query: async () => ({ rows: [] }) }, null, gatedConfig);
    return buildApp({
      config: gatedConfig,
      repository,
      browse: new StubBrowseRepository(),
      governance: new StubGovernanceRepository(),
      preview,
      mailer: new MemoryMailer(),
    });
  }

  it("answers 404 when a console request arrives without the proxy header", async () => {
    const app = await gatedHarness();

    const response = await app.inject({ method: "GET", url: "/console/v1/auth/session" });

    expect(response.statusCode).toBe(404);
    expect(response.json()).toMatchObject({ code: "not_found" });
    await app.close();
  });

  it("answers 404 when the presented token is wrong", async () => {
    const app = await gatedHarness();

    const response = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { "x-console-proxy-token": "q".repeat(48) },
    });

    expect(response.statusCode).toBe(404);
    await app.close();
  });

  // A token of a different length must not take a different code path from a
  // token of the right length: secretEquals compares lengths first, so this pins
  // the observable behaviour rather than the comparison's internals.
  it("answers 404 for a token of the wrong length", async () => {
    const app = await gatedHarness();

    const response = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { "x-console-proxy-token": "p" },
    });

    expect(response.statusCode).toBe(404);
    await app.close();
  });

  // The whole point of 404 over 401: an unauthenticated login attempt and a
  // probe for the route must be indistinguishable from outside.
  it("hides the login route itself behind the gate", async () => {
    const app = await gatedHarness();

    const response = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(response.statusCode).toBe(404);
    await app.close();
  });

  it("still serves the console once the header matches", async () => {
    const app = await gatedHarness();

    const response = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { "x-console-proxy-token": proxyToken },
    });

    // 401 rather than 404: the route exists and is simply unauthenticated, which
    // is exactly the distinction the gate hides from an unproxied caller.
    expect(response.statusCode).toBe(401);
    expect(response.json()).toMatchObject({ code: "session_required" });
    await app.close();
  });

  it("leaves the Cloud Run health probes ungated", async () => {
    const app = await gatedHarness();

    const health = await app.inject({ method: "GET", url: "/healthz" });
    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(health.statusCode).toBe(200);
    expect(ready.statusCode).toBe(200);
    await app.close();
  });

  // The annotator surface is reached directly by the desktop client. If the gate
  // ever leaks onto /app/v1, every client in the field stops being able to log in.
  // This harness serves no annotator surface, so /app/v1/login 404s either way —
  // what separates the two is the body: the gate sends the console error shape,
  // Fastify's own not-found handler does not.
  it("does not apply the gate to the annotator surface", async () => {
    const app = await gatedHarness();

    const response = await app.inject({
      method: "POST",
      url: "/app/v1/login",
      headers: writeHeaders,
      payload: { email: "annotator@diogenesmodel.com", password },
    });

    expect(response.statusCode).toBe(404);
    expect(response.json().code).toBeUndefined();
    await app.close();
  });
});

describe("browsing routes", () => {
  const browsePaths = [
    "/console/v1/annotators",
    "/console/v1/devices",
    "/console/v1/sessions",
    "/console/v1/clips",
    "/console/v1/games",
    // Catalog state is read at console.read like the rest of the browse
    // surface: content hashes are public commitments and snapshots are what
    // gets served to clients, so there is nothing here a viewer must not see.
    "/console/v1/catalog-versions",
    "/console/v1/catalog-releases",
    "/console/v1/audit-events",
    "/console/v1/stats/overview",
    "/console/v1/stats/output?from=2026-08-01&to=2026-08-14",
    "/console/v1/stats/game-distribution",
  ];

  it("requires a session for every browsing route", async () => {
    const { app } = await harness();

    for (const url of browsePaths) {
      const response = await app.inject({ method: "GET", url });
      expect({ url, status: response.statusCode }).toEqual({ url, status: 401 });
    }
    await app.close();
  });

  it("lets a viewer read every browsing route", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    for (const url of browsePaths) {
      const response = await value.app.inject({ method: "GET", url, headers: { cookie } });
      expect({ url, status: response.statusCode }).toEqual({ url, status: 200 });
    }
    await value.app.close();
  });

  it("passes the three identity filters through to the repository", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/clips?gameId=yuanshen&annotatorId=none&pageSize=10",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(200);
    expect(response.json()).toEqual({ items: [], nextCursor: null });
    expect(value.browse.calls).toContain("listClips");
    await value.app.close();
  });

  it("rejects a filter value outside the enum instead of ignoring it", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/clips?verificationState=not-a-state",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(400);
    expect(response.json()).toMatchObject({ code: "invalid_request" });
    await value.app.close();
  });

  it("rejects an annotator id that is neither a uuid nor the unattached marker", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/sessions?annotatorId=alpha",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(400);
    await value.app.close();
  });

  it("caps the page size instead of letting a caller ask for everything", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/clips?pageSize=5000",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(400);
    await value.app.close();
  });

  it("returns 404 for a detail route whose row does not exist", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/clips/00000000-0000-4000-8000-000000000001",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(404);
    expect(response.json()).toMatchObject({ code: "clip_not_found" });
    await value.app.close();
  });

  it("rejects a detail route id that is not a uuid", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/devices/not-a-uuid",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(400);
    await value.app.close();
  });
});

describe("catalog release publishing", () => {
  it("requires governance capability plus sudo and forwards an explicit roster", async () => {
    const publish = vi.fn(async (input: {
      channel: "dev";
      catalogId: string;
      catalogVersion: string;
      actor: string;
      trustedCatalogVersionIds: string[];
    }) => ({
      releaseId: randomUUID(),
      catalogId: input.catalogId,
      catalogVersion: input.catalogVersion,
      signingKeyId: "dm-dev-catalog-delivery-v1",
      etag: `"${"a".repeat(64)}"`,
    }));
    const value = await harness("operator", { publish });
    const cookie = await signedInCookie(value);
    const payload = {
      channel: "dev",
      catalogId: "dataminer-dev-console-test",
      catalogVersion: "2026-07-28.console-test",
      trustedCatalogVersionIds: Array.from({ length: 5 }, () => randomUUID()),
    };

    const blocked = await value.app.inject({
      method: "POST",
      url: "/console/v1/catalog-releases",
      headers: { ...writeHeaders, cookie, "idempotency-key": "publish-console-test" },
      payload,
    });
    expect(blocked.statusCode).toBe(403);
    expect(blocked.json().code).toBe("sudo_required");
    expect(publish).not.toHaveBeenCalled();

    const sudo = await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/sudo",
      headers: { ...writeHeaders, cookie },
      payload: { password },
    });
    expect(sudo.statusCode).toBe(200);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/catalog-releases",
      headers: { ...writeHeaders, cookie, "idempotency-key": "publish-console-test" },
      payload,
    });
    expect(response.statusCode).toBe(201);
    expect(publish).toHaveBeenCalledWith({
      ...payload,
      actor: "ops@diogenesmodel.com",
    });
    await value.app.close();
  }, 20_000);

  // issue #829 的验收：certified 发布侧退役后（docs/100 §11），显式发
  // `channel: "certified"` 必须拿到明确的 4xx 且**指名道姓**，不能静默当 dev
  // 发出去。这条路本来就发得出一份没有任何客户端装得上的目录——客户端只钉了
  // `DEV_CATALOG_ANCHOR` 一把公钥，certified 信封的验签必然失败。
  it("refuses an explicit certified publish by name and never reaches the publisher", async () => {
    const publish = vi.fn();
    const value = await harness("operator", { publish });
    const cookie = await signedInCookie(value);
    await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/sudo",
      headers: { ...writeHeaders, cookie },
      payload: { password },
    });

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/catalog-releases",
      headers: { ...writeHeaders, cookie, "idempotency-key": "publish-certified-retired" },
      payload: {
        channel: "certified",
        catalogId: "dataminer-certified-cod-steam",
        catalogVersion: "2026-08-20.retired-channel",
        trustedCatalogVersionIds: Array.from({ length: 5 }, () => randomUUID()),
      },
    });

    expect(response.statusCode).toBe(400);
    expect(response.json().code).toBe("invalid_request");
    const issues = response.json().details.issues as Array<{ path: string; message: string }>;
    expect(issues).toContainEqual({
      path: "channel",
      message: "the certified catalog channel was retired (docs/100); dev is the only channel",
    });
    expect(publish).not.toHaveBeenCalled();
    await value.app.close();
  }, 20_000);

  // 读侧同一条纪律：`?channel=certified` 也不静默退化成「列出 dev」。
  it("refuses a certified filter on the release listing", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/catalog-releases?channel=certified",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(400);
    const issues = response.json().details.issues as Array<{ path: string; message: string }>;
    expect(issues[0]!.message).toMatch(/certified catalog channel was retired/);
    await value.app.close();
  }, 20_000);
});

describe("governance routes", () => {
  const writeWithKey = { ...writeHeaders, "idempotency-key": "console-write-key-1" };

  it("requires an idempotency key on every write", async () => {
    const value = await harness();
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/annotators",
      headers: { ...writeHeaders, cookie },
      payload: { alias: "alpha" },
    });

    expect(response.statusCode).toBe(400);
    expect(value.governance.calls).not.toContain("createAnnotator");
    await value.app.close();
  });

  it("refuses a write to a viewer session", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/annotators",
      headers: { ...writeWithKey, cookie },
      payload: { alias: "alpha" },
    });

    expect(response.statusCode).toBe(403);
    expect(response.json()).toMatchObject({ code: "insufficient_role" });
    await value.app.close();
  });

  it("creates an annotator for an operator session", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/annotators",
      headers: { ...writeWithKey, cookie },
      payload: { alias: "alpha" },
    });

    expect(response.statusCode).toBe(201);
    expect(response.json().annotator).toMatchObject({ alias: "alpha", status: "active" });
    await value.app.close();
  });

  it("demands sudo for a purge and accepts it once granted", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);
    const clipId = "00000000-0000-4000-8000-000000000009";

    const denied = await value.app.inject({
      method: "POST",
      url: `/console/v1/clips/${clipId}/purge`,
      headers: { ...writeWithKey, cookie },
      payload: { reason: "operator requested" },
    });
    await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/sudo",
      headers: { ...writeHeaders, cookie },
      payload: { password },
    });
    const allowed = await value.app.inject({
      method: "POST",
      url: `/console/v1/clips/${clipId}/purge`,
      headers: { ...writeHeaders, "idempotency-key": "console-write-key-2", cookie },
      payload: { reason: "operator requested" },
    });

    expect(denied.statusCode).toBe(403);
    expect(denied.json()).toMatchObject({ code: "sudo_required" });
    expect(allowed.statusCode).toBe(202);
    expect(allowed.json()).toMatchObject({ enqueued: true });
    await value.app.close();
  });

  it("refuses a trainable consent with policy_blocked", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/consents",
      headers: { ...writeWithKey, cookie },
      payload: {
        annotatorId: "00000000-0000-4000-8000-00000000000a",
        policyVersion: "v1",
        purpose: "training",
        termsHash: "a".repeat(64),
        acceptedAt: new Date().toISOString(),
        allowsTrainableUse: true,
      },
    });

    expect(response.statusCode).toBe(422);
    expect(response.json()).toMatchObject({ code: "policy_blocked" });
    expect(value.governance.calls).not.toContain("createConsent");
    await value.app.close();
  });

  it("registers a device and returns the token exactly once", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);
    await value.app.inject({
      method: "POST",
      url: "/console/v1/auth/sudo",
      headers: { ...writeHeaders, cookie },
      payload: { password },
    });
    const payload = {
      clientDeviceId: "dev-console-1",
      windowsBuild: "10.0.26100",
      gpuPciId: "PCI\\VEN_10DE",
      gpuDriver: "566.36",
      clientVersion: "0.1.0",
      displaySnapshot: { width: 1920, height: 1080 },
      inputDevicesSnapshot: [{ kind: "keyboard" }],
    };

    const first = await value.app.inject({
      method: "POST",
      url: "/console/v1/devices",
      headers: { ...writeWithKey, cookie },
      payload,
    });
    const replay = await value.app.inject({
      method: "POST",
      url: "/console/v1/devices",
      headers: { ...writeWithKey, cookie },
      payload,
    });

    expect(first.statusCode).toBe(201);
    expect(first.json().token).toHaveLength(43);
    expect(replay.json().token).toBeNull();
    await value.app.close();
  });

  it("rejects an idempotency key that is not url safe", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/annotators",
      headers: { ...writeHeaders, "idempotency-key": "key with spaces", cookie },
      payload: { alias: "alpha" },
    });

    expect(response.statusCode).toBe(400);
    await value.app.close();
  });

  it("routes both clip sub-resources without colliding", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);
    const clipId = "00000000-0000-4000-8000-00000000000c";

    const quality = await value.app.inject({
      method: "POST",
      url: `/console/v1/clips/${clipId}/quality`,
      headers: { ...writeWithKey, cookie },
      payload: { result: "passed" },
    });

    expect(quality.statusCode).toBe(200);
    expect(quality.json()).toMatchObject({ qualityState: "passed" });
    expect(value.governance.calls).toContain("recordClipQuality");
    await value.app.close();
  });

  it("routes the session-level quality decision and reports the affected clips", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);
    const sessionId = "00000000-0000-4000-8000-00000000000d";

    const quality = await value.app.inject({
      method: "POST",
      url: `/console/v1/sessions/${sessionId}/quality`,
      headers: { ...writeWithKey, cookie },
      payload: { result: "passed" },
    });

    expect(quality.statusCode).toBe(200);
    expect(quality.json()).toMatchObject({
      sessionId,
      qualityState: "passed",
      decidedClipCount: 3,
      skippedClipCount: 1,
    });
    expect(value.governance.calls).toContain("recordSessionQuality");
    await value.app.close();
  });

  it("refuses the session-level quality decision to a viewer", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const quality = await value.app.inject({
      method: "POST",
      url: "/console/v1/sessions/00000000-0000-4000-8000-00000000000d/quality",
      headers: { ...writeWithKey, cookie },
      payload: { result: "passed" },
    });

    expect(quality.statusCode).toBe(403);
    expect(value.governance.calls).not.toContain("recordSessionQuality");
    await value.app.close();
  });
});

describe("preview and pipeline routes", () => {
  it("reports whether preview is available on the readiness probe", async () => {
    const { app } = await harness();

    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(ready.json()).toMatchObject({ features: { clipPreview: false } });
    await app.close();
  });

  it("refuses preview to a viewer before it even consults the object store", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/clips/00000000-0000-4000-8000-00000000000d/preview-url",
      headers: { "x-dmc-csrf": "1", cookie },
    });

    expect(response.statusCode).toBe(403);
    expect(response.json()).toMatchObject({ code: "insufficient_role" });
    await value.app.close();
  });

  it("tells an operator that preview is not configured rather than failing opaquely", async () => {
    const value = await harness("operator");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "POST",
      url: "/console/v1/clips/00000000-0000-4000-8000-00000000000d/preview-url",
      headers: { "x-dmc-csrf": "1", cookie },
    });

    expect(response.statusCode).toBe(409);
    expect(response.json()).toMatchObject({ code: "preview_not_configured" });
    await value.app.close();
  });

  it("serves pipeline health to any reader", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/stats/pipeline?days=7",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(200);
    expect(value.browse.calls).toContain("pipelineHealth");
    await value.app.close();
  });

  it("serves daily intake to any reader, with defaults echoed to the repository", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const defaulted = await value.app.inject({
      method: "GET",
      url: "/console/v1/stats/daily",
      headers: { cookie },
    });
    const explicit = await value.app.inject({
      method: "GET",
      url: "/console/v1/stats/daily?days=7&timeBasis=registered&tz=UTC",
      headers: { cookie },
    });
    const badZone = await value.app.inject({
      method: "GET",
      url: "/console/v1/stats/daily?tz=Not/AZone",
      headers: { cookie },
    });

    expect(defaulted.statusCode).toBe(200);
    expect(value.browse.calls).toContain("dailyIntake:30:default:Asia/Shanghai");
    expect(explicit.statusCode).toBe(200);
    expect(value.browse.calls).toContain("dailyIntake:7:registered:UTC");
    expect(badZone.statusCode).toBe(400);
    await value.app.close();
  });

  it("产出报表：区间必填、分组有序、可重复筛选参数原样传到仓储（docs/145）", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);
    const get = (url: string) => value.app.inject({ method: "GET", url, headers: { cookie } });

    const ok = await get(
      "/console/v1/stats/output?from=2026-08-01&to=2026-08-14&groupBy=annotator,game" +
        "&gameId=cod&gameId=ets2&annotatorId=none",
    );
    // 区间是必填的：这条路由服务的是一张显式选了区间的表，没有「缺省最近 N 天」。
    const missingRange = await get("/console/v1/stats/output");
    const backwards = await get("/console/v1/stats/output?from=2026-08-14&to=2026-08-01");
    // 按日一个季度：越界返回 400，不静默换粒度，也不截断行。
    const tooLong = await get("/console/v1/stats/output?from=2026-01-01&to=2026-06-01");
    const asMonth = await get(
      "/console/v1/stats/output?from=2026-01-01&to=2026-06-01&bucket=month",
    );
    const unknownDimension = await get(
      "/console/v1/stats/output?from=2026-08-01&to=2026-08-14&groupBy=device",
    );

    expect(ok.statusCode).toBe(200);
    expect(value.browse.calls).toContain(
      "outputReport:2026-08-01:2026-08-14:day:annotator+game:Asia/Shanghai",
    );
    expect(ok.json()).toMatchObject({ groupBy: ["annotator", "game"] });
    expect(missingRange.statusCode).toBe(400);
    expect(backwards.statusCode).toBe(400);
    expect(tooLong.statusCode).toBe(400);
    expect(asMonth.statusCode).toBe(200);
    expect(unknownDimension.statusCode).toBe(400);
    await value.app.close();
  });

  it("游戏分布普查：无参数直达仓储（docs/153）", async () => {
    const value = await harness("viewer");
    const cookie = await signedInCookie(value);

    const response = await value.app.inject({
      method: "GET",
      url: "/console/v1/stats/game-distribution",
      headers: { cookie },
    });

    expect(response.statusCode).toBe(200);
    expect(value.browse.calls).toContain("gameDistribution");
    expect(response.json()).toMatchObject({ rows: [] });
    await value.app.close();
  });
});

describe("console login mode: password_only", () => {
  const singleFactorConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "f".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_LOGIN_MODE: "password_only",
  });

  async function singleFactorApp() {
    const repository = new MemoryConsoleRepository();
    const mailer = new MemoryMailer();
    const browse = new StubBrowseRepository();
    const governance = new StubGovernanceRepository();
    const preview = new PreviewService({ query: async () => ({ rows: [] }) }, null, singleFactorConfig);
    const app = buildApp({ config: singleFactorConfig, repository, browse, governance, preview, mailer });
    const operator = await repository.createOperator({
      email: "ops@diogenesmodel.com",
      role: "operator",
      createdBy: null,
    });
    await repository.activateOperatorWithPassword(operator.id, passwordHash);
    return { app, repository, mailer };
  }

  it("signs in with one step: a correct password returns 200 with a session cookie and no code", async () => {
    const { app, mailer } = await singleFactorApp();

    const login = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password },
    });

    expect(login.statusCode).toBe(200);
    expect(login.json()).toMatchObject({ operator: { email: "ops@diogenesmodel.com" } });
    const cookie = login.headers["set-cookie"];
    expect(typeof cookie).toBe("string");
    expect(cookie).toContain(sessionCookieName);
    expect(mailer.outbox).toHaveLength(0);

    // The cookie is a working session on the spot — no second round trip.
    const token = (cookie as string).slice(
      (cookie as string).indexOf("=") + 1,
      (cookie as string).indexOf(";"),
    );
    const session = await app.inject({
      method: "GET",
      url: "/console/v1/auth/session",
      headers: { cookie: `${sessionCookieName}=${token}` },
    });
    expect(session.statusCode).toBe(200);
    await app.close();
  });

  it("rejects a wrong password with 401 and sets no cookie", async () => {
    const { app } = await singleFactorApp();

    const login = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password: "not the password" },
    });

    expect(login.statusCode).toBe(401);
    expect(login.json()).toMatchObject({ code: "invalid_credentials" });
    expect(login.headers["set-cookie"]).toBeUndefined();
    await app.close();
  });
});

describe("console login mode: cf_access", () => {
  const cfConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "f".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_LOGIN_MODE: "cf_access",
    CONSOLE_CF_ACCESS_TEAM_DOMAIN: "hakkoai.cloudflareaccess.com",
    CONSOLE_CF_ACCESS_AUD: "a".repeat(64),
    CONSOLE_CF_ACCESS_ADMIN_EMAILS: "ops@diogenesmodel.com",
  });
  const fakeVerifier = {
    async verify(assertion: string) {
      if (assertion === "good") return { email: "ops@diogenesmodel.com" };
      throw new Error("invalid assertion");
    },
  };

  async function cfApp() {
    const repository = new MemoryConsoleRepository();
    const mailer = new MemoryMailer();
    const browse = new StubBrowseRepository();
    const governance = new StubGovernanceRepository();
    const preview = new PreviewService({ query: async () => ({ rows: [] }) }, null, cfConfig);
    const app = buildApp({
      config: cfConfig,
      repository,
      browse,
      governance,
      preview,
      mailer,
      cfAccessVerifier: fakeVerifier,
    });
    return { app };
  }

  it("exchanges an edge Access assertion for a session, and offers no password login", async () => {
    const { app } = await cfApp();

    const password = await app.inject({
      method: "POST",
      url: "/console/v1/auth/login",
      headers: writeHeaders,
      payload: { email: "ops@diogenesmodel.com", password: "a sufficiently long password" },
    });
    expect(password.statusCode).toBe(404); // no password step exists in cf_access

    const login = await app.inject({
      method: "POST",
      url: "/console/v1/auth/cf-access/login",
      headers: { "x-dmc-csrf": "1", "cf-access-jwt-assertion": "good" },
    });
    expect(login.statusCode).toBe(200);
    expect(login.json()).toMatchObject({ operator: { email: "ops@diogenesmodel.com", role: "admin" } });
    expect(login.headers["set-cookie"]).toContain(sessionCookieName);
    await app.close();
  });

  it("rejects cf-access login when the assertion is missing", async () => {
    const { app } = await cfApp();
    const res = await app.inject({
      method: "POST",
      url: "/console/v1/auth/cf-access/login",
      headers: { "x-dmc-csrf": "1" },
    });
    expect(res.statusCode).toBe(401);
    expect(res.json()).toMatchObject({ code: "invalid_credentials" });
    await app.close();
  });

  it("404s cf-access login when the console is not in cf_access mode", async () => {
    const app = buildApp({
      config, // module-level password_otp config
      repository: new MemoryConsoleRepository(),
      browse: new StubBrowseRepository(),
      governance: new StubGovernanceRepository(),
      preview: new PreviewService({ query: async () => ({ rows: [] }) }, null, config),
      mailer: new MemoryMailer(),
    });
    const res = await app.inject({
      method: "POST",
      url: "/console/v1/auth/cf-access/login",
      headers: { "x-dmc-csrf": "1", "cf-access-jwt-assertion": "good" },
    });
    expect(res.statusCode).toBe(404);
    await app.close();
  });
});

describe("errorDiagnostics", () => {
  // The 2026-07-28 grant incident: two routes 500'd and the log said only
  // {"errorName":"error"}. These pin both halves of the fix — enough detail to
  // name the failure, not enough to leak a row.
  function pgError(fields: Record<string, unknown>): Error {
    return Object.assign(new Error(String(fields.message ?? "boom")), fields);
  }

  it("names a missing GRANT well enough to act on without reading the code", () => {
    const diagnostics = errorDiagnostics(
      pgError({
        message: "permission denied for table review_verdicts",
        code: "42501",
        table: "review_verdicts",
        schema: "public",
        routine: "aclcheck_error",
      }),
    );
    expect(diagnostics.errorMessage).toBe("permission denied for table review_verdicts");
    expect(diagnostics.pgCode).toBe("42501");
    expect(diagnostics.pgTable).toBe("review_verdicts");
    expect(diagnostics.stack).toContain("app.test");
  });

  it("omits the postgres fields that embed row values", () => {
    const diagnostics = errorDiagnostics(
      pgError({
        message: 'duplicate key value violates unique constraint "operators_email_key"',
        code: "23505",
        constraint: "operators_email_key",
        // Each of these carries actual column data and must never be logged.
        detail: "Key (email)=(someone@example.com) already exists.",
        where: "PL/pgSQL function audit() line 3",
        internalQuery: "SELECT 'someone@example.com'",
      }),
    );
    expect(diagnostics.pgConstraint).toBe("operators_email_key");
    const serialized = JSON.stringify(diagnostics);
    expect(serialized).not.toContain("someone@example.com");
    expect(diagnostics).not.toHaveProperty("detail");
    expect(diagnostics).not.toHaveProperty("where");
    expect(diagnostics).not.toHaveProperty("internalQuery");
  });

  it("degrades safely on a non-Error throw", () => {
    expect(errorDiagnostics("just a string")).toEqual({ errorName: "UnknownError" });
  });
});

describe("readyz grant canary", () => {
  it("reports ok when the console role holds the canary privileges", async () => {
    const { app } = await harness();
    const ready = await app.inject({ method: "GET", url: "/readyz" });
    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({ grants: { status: "ok", missing: [] } });
    await app.close();
  });

  it("names the missing privileges but stays 200 — a skipped grants step must not unhealth a serving revision", async () => {
    const { app, repository } = await harness();
    repository.missingGrants = ["review_verdicts.SELECT", "hf_release_items.DELETE"];

    const ready = await app.inject({ method: "GET", url: "/readyz" });

    // 200 on purpose: the routes that do not touch these tables keep serving,
    // and Cloud Run must not roll back a revision over this. The deploy script
    // is what turns it into a loud failure.
    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({
      status: "ready",
      grants: { status: "incomplete", missing: ["review_verdicts.SELECT", "hf_release_items.DELETE"] },
    });
    await app.close();
  });

  it("degrades to unknown rather than failing readyz when the probe itself errors", async () => {
    const { app, repository } = await harness();
    repository.missingConsoleGrants = async () => {
      throw new Error("catalog unavailable");
    };
    const ready = await app.inject({ method: "GET", url: "/readyz" });
    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({ grants: { status: "unknown" } });
    await app.close();
  });
});

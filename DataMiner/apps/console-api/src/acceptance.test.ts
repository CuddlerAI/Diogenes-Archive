import { randomUUID } from "node:crypto";

import { beforeAll, describe, expect, it, vi } from "vitest";

import { AcceptanceService, type AcceptanceStorage } from "./acceptance.js";
import { buildApp } from "./app.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import { sessionCookieName } from "./http.js";
import { PreviewService } from "./preview.js";
import { hashPassword } from "./secrets.js";
import {
  argon2TestTimeoutMs,
  MemoryConsoleRepository,
  MemoryMailer,
  StubBrowseRepository,
  StubGovernanceRepository,
} from "./testing.js";

// The signed-in cases drive a real password login over HTTP; see
// argon2TestTimeoutMs for why the budget is this large.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

const password = "correct horse battery staple";
let passwordHash = "";

beforeAll(async () => {
  passwordHash = await hashPassword(password);
});

// password_only：验收路由只关心 console.read 已通过，单因子登录让每个用例
// 少打一轮 OTP。
const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "e".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
  CONSOLE_LOGIN_MODE: "password_only",
});

const bucket = "diogenes-model-training";
const laneKey = (lane: "non-trainable" | "attested-trainable", session: string) =>
  `raw/v1/${lane}/dataminer-api-production/sessions/${session}/acceptance/dm-session-acceptance-1.json`;
// issue #855 的中性摄取车道；2026-08-29 起生产唯一的落桶车道（docs/167 §3.2）。
const v2Key = (session: string) =>
  `raw/v2/dataminer-api-production/sessions/${session}/acceptance/dm-session-acceptance-1.json`;

class FakeAcceptanceStorage implements AcceptanceStorage {
  readonly objects = new Map<string, Buffer>();
  readonly reads: string[] = [];

  put(key: string, body: string): void {
    this.objects.set(`${bucket}/${key}`, Buffer.from(body, "utf8"));
  }

  async getObject(bucketName: string, key: string): Promise<Buffer | null> {
    this.reads.push(`${bucketName}/${key}`);
    return this.objects.get(`${bucketName}/${key}`) ?? null;
  }
}

// A realistic dm-session-acceptance/1 document (accept.py build_report shape).
const sessionId = "5dc08f50-e709-45ff-b3aa-875d90662ed6";
const report = {
  schema: "dm-session-acceptance/1",
  session: sessionId,
  generated_by: "tools/session-acceptance/accept.py",
  clips_total: 12,
  clips_complete: 12,
  audio_present_clips: 12,
  game_id: "halo_infinite",
  games_seen: { halo_infinite: 12 },
  mixed_session: false,
  hours: 0.9333,
  fps_seen: { "60/1": 12 },
  health: {
    duplicate_frames: 0,
    queue_drops: 0,
    input_overflow: 0,
    empty_events_clips: 0,
    meta_unreadable: 0,
    manifest_mismatch: 0,
  },
  qc: {
    ran: true,
    tier: "floor",
    passed: true,
    failed_checks: [],
    checks: [
      { id: "key_coverage", reading: 0.41, passed: true, enabled: true, source: "vocabulary" },
      { id: "mouse_channel", reading: "present", passed: true, enabled: false, source: "policy" },
    ],
  },
  findings: [],
  passed: true,
};

async function harness(acceptance: AcceptanceService) {
  const repository = new MemoryConsoleRepository();
  const mailer = new MemoryMailer();
  const app = buildApp({
    config,
    repository,
    browse: new StubBrowseRepository(),
    governance: new StubGovernanceRepository(),
    preview: new PreviewService({ query: async () => ({ rows: [] }) }, null, config),
    acceptance,
    mailer,
  });
  const operator = await repository.createOperator({
    email: "ops@diogenesmodel.com",
    role: "viewer",
    createdBy: null,
  });
  await repository.activateOperatorWithPassword(operator.id, passwordHash);
  const login = await app.inject({
    method: "POST",
    url: "/console/v1/auth/login",
    headers: { "content-type": "application/json", "x-dmc-csrf": "1" },
    payload: { email: "ops@diogenesmodel.com", password },
  });
  const cookie = login.headers["set-cookie"];
  if (typeof cookie !== "string") throw new Error("no session cookie was issued");
  const token = cookie.slice(cookie.indexOf("=") + 1, cookie.indexOf(";"));
  return { app, cookie: `${sessionCookieName}=${token}` };
}

describe("acceptance configuration", () => {
  const base = {
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "e".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
  };

  it("refuses a partial ACCEPTANCE_R2_* configuration", () => {
    expect(() =>
      loadConfig({ ...base, ACCEPTANCE_R2_ACCOUNT_ID: "account" }),
    ).toThrow(/every ACCEPTANCE_R2_\* credential or none/);
  });

  it("defaults the bucket to diogenes-model-training", () => {
    const loaded = loadConfig({
      ...base,
      ACCEPTANCE_R2_ACCOUNT_ID: "account",
      ACCEPTANCE_R2_ACCESS_KEY_ID: "key",
      ACCEPTANCE_R2_SECRET_ACCESS_KEY: "secret",
    });
    expect(loaded.acceptance).toEqual({
      accountId: "account",
      accessKeyId: "key",
      secretAccessKey: "secret",
      bucket: "diogenes-model-training",
    });
  });

  it("stays null when no ACCEPTANCE_R2_* credential is set", () => {
    expect(loadConfig(base).acceptance).toBeNull();
  });
});

describe("AcceptanceService lane resolution", () => {
  it("prefers the non-trainable lane as the authoritative source", async () => {
    const storage = new FakeAcceptanceStorage();
    storage.put(laneKey("non-trainable", sessionId), JSON.stringify(report));
    storage.put(
      laneKey("attested-trainable", sessionId),
      JSON.stringify({ ...report, passed: false }),
    );
    const service = new AcceptanceService(storage, bucket);
    expect(await service.report(sessionId)).toMatchObject({ passed: true });
    // The authoritative lane answered; the fallback lane was never read.
    expect(storage.reads).toEqual([`${bucket}/${laneKey("non-trainable", sessionId)}`]);
  });

  it("falls back to the attested-trainable lane when the earlier ones are empty", async () => {
    const storage = new FakeAcceptanceStorage();
    storage.put(laneKey("attested-trainable", sessionId), JSON.stringify(report));
    const service = new AcceptanceService(storage, bucket);
    expect(await service.report(sessionId)).toMatchObject({ session: sessionId });
    expect(storage.reads).toEqual([
      `${bucket}/${laneKey("non-trainable", sessionId)}`,
      `${bucket}/${v2Key(sessionId)}`,
      `${bucket}/${laneKey("attested-trainable", sessionId)}`,
    ]);
  });

  it("finds the report in the raw/v2 ingest lane", async () => {
    // 2026-08-29 起新 session 只落 v2。少了这条车道，它们的「验收」栏恒空——
    // 不是「未验收」，是根本没去看（docs/167 §3.2）。
    const storage = new FakeAcceptanceStorage();
    storage.put(v2Key(sessionId), JSON.stringify(report));
    const service = new AcceptanceService(storage, bucket);
    expect(await service.report(sessionId)).toMatchObject({ session: sessionId });
    // 命中即停：attested 车道不该被读到。
    expect(storage.reads).toEqual([
      `${bucket}/${laneKey("non-trainable", sessionId)}`,
      `${bucket}/${v2Key(sessionId)}`,
    ]);
  });

  it("refuses an unparseable report instead of passing garbage through", async () => {
    const storage = new FakeAcceptanceStorage();
    storage.put(laneKey("non-trainable", sessionId), "{truncated");
    const service = new AcceptanceService(storage, bucket);
    await expect(service.report(sessionId)).rejects.toMatchObject({
      code: "malformed_acceptance_report",
      statusCode: 409,
    });
  });
});

describe("GET /console/v1/sessions/:id/acceptance", () => {
  it("passes the stored report through verbatim", async () => {
    const storage = new FakeAcceptanceStorage();
    storage.put(laneKey("non-trainable", sessionId), JSON.stringify(report));
    const { app, cookie } = await harness(new AcceptanceService(storage, bucket));

    const response = await app.inject({
      method: "GET",
      url: `/console/v1/sessions/${sessionId}/acceptance`,
      headers: { cookie },
    });
    expect(response.statusCode).toBe(200);
    // 原样透传：包括服务端自己从不读的字段（generated_by、fps_seen）。
    expect(response.json()).toEqual(report);
  });

  it("answers 404 {error:'not_accepted'} when no lane holds a report", async () => {
    const { app, cookie } = await harness(
      new AcceptanceService(new FakeAcceptanceStorage(), bucket),
    );
    const response = await app.inject({
      method: "GET",
      url: `/console/v1/sessions/${randomUUID()}/acceptance`,
      headers: { cookie },
    });
    expect(response.statusCode).toBe(404);
    expect(response.json()).toMatchObject({ error: "not_accepted" });
  });

  it("answers 503, never 404, when acceptance storage is unconfigured", async () => {
    const { app, cookie } = await harness(new AcceptanceService(null, bucket));
    const response = await app.inject({
      method: "GET",
      url: `/console/v1/sessions/${sessionId}/acceptance`,
      headers: { cookie },
    });
    expect(response.statusCode).toBe(503);
    expect(response.json()).toMatchObject({ error: "acceptance_storage_unconfigured" });
  });

  it("rejects a non-uuid session id with 400", async () => {
    const { app, cookie } = await harness(
      new AcceptanceService(new FakeAcceptanceStorage(), bucket),
    );
    const response = await app.inject({
      method: "GET",
      url: "/console/v1/sessions/not-a-uuid/acceptance",
      headers: { cookie },
    });
    expect(response.statusCode).toBe(400);
  });

  it("requires a console session", async () => {
    const storage = new FakeAcceptanceStorage();
    storage.put(laneKey("non-trainable", sessionId), JSON.stringify(report));
    const { app } = await harness(new AcceptanceService(storage, bucket));
    const response = await app.inject({
      method: "GET",
      url: `/console/v1/sessions/${sessionId}/acceptance`,
    });
    expect(response.statusCode).toBe(401);
  });

  it("reports availability on /readyz", async () => {
    const { app } = await harness(new AcceptanceService(null, bucket));
    const ready = await app.inject({ method: "GET", url: "/readyz" });
    expect(ready.json()).toMatchObject({ features: { acceptance: false } });
  });
});

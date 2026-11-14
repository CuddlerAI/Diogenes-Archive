import { createHash } from "node:crypto";
import { Writable } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import type { AppConfig } from "./config.js";
import type { IngestionRepository } from "./domain.js";
import { environmentProfile, primaryFamily } from "./environment.js";
import type { ObjectStorage } from "./storage.js";

/**
 * docs/101 行动项 D 的回归：**被拒绝的请求必须说得出是哪台设备**。
 *
 * 背景：404/409 抛在写库之前，`sessions` / `audit_events` / `idempotency_requests`
 * 三处都不留行（docs/101 §3），唯一痕迹是应用日志。而它此前只有 requestId，
 * 所以 2026-08-05/06 那 31 次拒绝只能靠时间戳猜是谁（§3.2 的归属限度）。
 */

const token = "device-credential-with-at-least-32-bytes";
const tokenFingerprint = createHash("sha256").update(token).digest("hex");
const deviceId = "11111111-1111-4111-8111-111111111111";
const annotatorId = "22222222-2222-4222-8222-222222222222";
const proxyToken = "p".repeat(48);

const family = primaryFamily(environmentProfile("staging"));
const config: AppConfig = {
  environment: "staging",
  profile: environmentProfile("staging"),
  admittedFamilies: [{ family, prefix: "raw-staging/v1/non-trainable/dataminer-api-staging" }],
  databaseUrl: "postgresql://example:example@localhost/dataminer",
  proxyToken,
  host: "127.0.0.1",
  port: 8080,
  r2: {
    accountId: "account",
    bucket: "bucket",
    accessKeyId: "access",
    secretAccessKey: "secret",
    prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
    presignTtlSeconds: 900,
    partSizeBytes: 64 * 1024 * 1024,
    requestTimeoutSeconds: 120,
  },
  worker: {
    batchSize: 4,
    leaseSeconds: 900,
    purgeBatchSize: 4,
    purgeLeaseSeconds: 900,
    multipartCompletionLeaseSeconds: 300,
    orphanMultipartGraceSeconds: 1_200,
  },
  retentionDays: 30,
  logsExport: { enabled: false, bucket: null, batchSize: 200, dryRun: false },
  unconsentedTelemetryTtlDays: 30,
};

/** 收集 pino 写出的 NDJSON，只留 setErrorHandler 那条。 */
function loggingHarness(overrides: Partial<IngestionRepository> = {}) {
  const lines: Record<string, unknown>[] = [];
  const stream = new Writable({
    write(chunk, _encoding, callback) {
      for (const line of String(chunk).split("\n")) {
        if (line.trim() === "") continue;
        try {
          lines.push(JSON.parse(line) as Record<string, unknown>);
        } catch {
          // pino 的非 JSON 行（不该有）直接忽略，不让测试因为噪声变绿或变红。
        }
      }
      callback();
    },
  });
  const repository = {
    ping: vi.fn(async () => undefined),
    authenticateDevice: vi.fn(async (credentialFingerprint: string) =>
      credentialFingerprint === tokenFingerprint
        ? { id: deviceId, annotatorId, credentialFingerprint }
        : null,
    ),
    ...overrides,
  } as unknown as IngestionRepository;
  const app = buildApp({
    config,
    repository,
    storage: { ping: vi.fn(async () => undefined) } as unknown as ObjectStorage,
    logger: { level: "warn", stream },
  });
  const errorLines = () => lines.filter((line) => line.msg === "handled request error");
  return { app, errorLines };
}

const authHeaders = {
  authorization: `Bearer ${token}`,
  "x-dataminer-proxy-token": proxyToken,
};

describe("拒绝路径的身份归属（docs/101 行动项 D）", () => {
  it("已认证请求被拒时，日志带 deviceId 与 annotatorId", async () => {
    // 这里落在 `policy_blocked`（缺 classification.sourceFamily，在 Zod 之前就被拦）。
    // 它与 wangharp 那 31 次的 `catalog_version_not_found` **走的是同一条
    // ApiError 分支**——`principalLogFields` 就挂在那条分支上，所以覆盖其一即覆盖该路径。
    const { app, errorLines } = loggingHarness();

    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...authHeaders, "idempotency-key": "k".repeat(24) },
      payload: {},
    });

    expect(response.statusCode).toBeGreaterThanOrEqual(400);
    const [entry] = errorLines();
    expect(entry).toBeDefined();
    // 钉住分支，否则这条测试可能在别的路径上空过。
    expect(entry!.errorCode).toBe("policy_blocked");
    expect(entry!.deviceId).toBe(deviceId);
    expect(entry!.annotatorId).toBe(annotatorId);
    await app.close();
  });

  /**
   * 边缘令牌闸跑在设备查询**之前**（app.ts 的 onRequest），所以那一层的 404
   * 本来就没有身份可记。断言它为空是刻意的：那正是「这一次连是谁都不知道」
   * 的真实记录，不该被伪造成某台设备。
   */
  it("鉴权之前的拒绝不伪造身份", async () => {
    const { app, errorLines } = loggingHarness();

    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}` }, // 缺 proxy token
      payload: {},
    });

    expect(response.statusCode).toBe(404);
    const [entry] = errorLines();
    expect(entry).toBeDefined();
    expect(entry!.errorCode).toBe("not_found");
    expect(entry).not.toHaveProperty("deviceId");
    expect(entry).not.toHaveProperty("annotatorId");
    await app.close();
  });

  it("凭据无效时同样不记身份——认证没通过就没有身份", async () => {
    const { app, errorLines } = loggingHarness();

    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...authHeaders, authorization: "Bearer not-the-registered-credential" },
      payload: {},
    });

    expect(response.statusCode).toBe(401);
    const [entry] = errorLines();
    expect(entry!.errorCode).toBe("invalid_device_credential");
    expect(entry).not.toHaveProperty("deviceId");
    await app.close();
  });

  /** 身份只进日志，绝不进响应体——响应契约一个字节都不该变。 */
  it("身份不泄进响应体", async () => {
    const { app } = loggingHarness();
    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...authHeaders, authorization: "Bearer not-the-registered-credential" },
      payload: {},
    });
    expect(response.body).not.toContain(deviceId);
    expect(response.body).not.toContain(annotatorId);
    await app.close();
  });
});

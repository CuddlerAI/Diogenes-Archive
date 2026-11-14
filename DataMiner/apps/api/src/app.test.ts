import { createHash } from "node:crypto";

import { describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import type { AppConfig } from "./config.js";
import { environmentProfile, primaryFamily, type AdmissionFamily } from "./environment.js";
import type { ClipRecord, IngestionRepository, SessionRecord } from "./domain.js";
import type { ObjectStorage } from "./storage.js";

// A device credential, not a fleet-wide secret: the harness registers exactly
// this token's fingerprint below, so a different token authenticates as nobody.
const token = "device-credential-with-at-least-32-bytes";
const tokenFingerprint = createHash("sha256").update(token).digest("hex");
const deviceId = "11111111-1111-4111-8111-111111111111";
const annotatorId = "22222222-2222-4222-8222-222222222222";

const config: AppConfig = {
  environment: "staging",
  profile: environmentProfile("staging"),
  admittedFamilies: [
    {
      family: primaryFamily(environmentProfile("staging")),
      prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
    },
  ],
  databaseUrl: "postgresql://example:example@localhost/dataminer",
  // Staging may run without the edge gate; production may not (config.ts).
  proxyToken: null,
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

function harness(
  overrides: Partial<IngestionRepository> = {},
  configOverrides: Partial<AppConfig> = {},
) {
  const repository = {
    ping: vi.fn(async () => undefined),
    authenticateDevice: vi.fn(async (credentialFingerprint: string) =>
      credentialFingerprint === tokenFingerprint
        ? { id: deviceId, annotatorId, credentialFingerprint }
        : null,
    ),
    ...overrides,
  } as unknown as IngestionRepository;
  const storage = { ping: vi.fn(async () => undefined) } as unknown as ObjectStorage;
  return {
    app: buildApp({
      config: { ...config, ...configOverrides },
      repository,
      storage,
    }),
    repository,
    storage,
  };
}

function successfulIdempotencyRepository(): Partial<IngestionRepository> {
  return {
    getIdempotency: vi.fn(async () => null),
    reserveIdempotency: vi.fn(async () => "reserved" as const),
    finishIdempotency: vi.fn(async () => undefined),
    releaseIdempotency: vi.fn(async () => undefined),
  };
}

describe("staging API boundary", () => {
  it("returns the live server admission roster to an authenticated device", async () => {
    const listGameAdmissions = vi.fn(async () => [
      { gameId: "ready-game", admission: "admitted" as const, reason: null },
      {
        gameId: "paused-game",
        admission: "onboarding" as const,
        reason: "capture_not_allowed" as const,
      },
    ]);
    const { app } = harness({ listGameAdmissions });
    const response = await app.inject({
      method: "GET",
      url: "/v1/game-admissions",
      headers: { authorization: `Bearer ${token}` },
    });

    expect(response.statusCode).toBe(200);
    expect(response.json()).toEqual({
      schemaVersion: "dm-game-admission-1",
      games: [
        { gameId: "ready-game", admission: "admitted", reason: null },
        { gameId: "paused-game", admission: "onboarding", reason: "capture_not_allowed" },
      ],
    });
    expect(listGameAdmissions).toHaveBeenCalledOnce();
    await app.close();
  });

  it("reports process identity and both readiness dependencies", async () => {
    const { app } = harness();
    const health = await app.inject({ method: "GET", url: "/healthz" });
    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(health.statusCode).toBe(200);
    expect(health.json()).toEqual({
      status: "ok",
      service: "dataminer-api-staging",
      version: "0.1.0",
    });
    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({
      status: "ready",
      environment: "staging",
      dependencies: {
        cloudSql: { status: "ready" },
        r2ControlPlane: { status: "ready" },
      },
    });
    await app.close();
  });

  // The edge gate (docs/06 §3, mirroring the console's CONSOLE_PROXY_TOKEN).
  // Cloud Run has to admit `allUsers` for the Cloudflare Worker to reach it, so
  // these tests pin the behaviour that keeps the bare `*.run.app` URL useless.
  describe("edge proxy gate", () => {
    const proxyToken = "e".repeat(48);

    it("answers /v1/* with 404 when the edge header is absent", async () => {
      const { app, repository } = harness({}, { proxyToken });
      const response = await app.inject({
        method: "POST",
        url: "/v1/sessions",
        headers: { authorization: `Bearer ${token}` },
        payload: {},
      });

      // 404, not 401: a probe that reached Cloud Run around the Worker must not
      // learn that an ingestion API is here, and must not be told that its
      // otherwise-valid device credential was the part that was fine.
      expect(response.statusCode).toBe(404);
      expect(response.json()).toMatchObject({ code: "not_found" });
      // The gate runs before the device lookup, so a bypass costs no query.
      expect(repository.authenticateDevice).not.toHaveBeenCalled();
      await app.close();
    });

    it("answers /v1/* with 404 when the edge header is wrong", async () => {
      const { app, repository } = harness({}, { proxyToken });
      const response = await app.inject({
        method: "POST",
        url: "/v1/sessions",
        headers: {
          authorization: `Bearer ${token}`,
          // Same length as the real token: a mismatch must not be decidable
          // from the response, and must not short-circuit on length alone.
          "x-dataminer-proxy-token": "f".repeat(48),
        },
        payload: {},
      });

      expect(response.statusCode).toBe(404);
      expect(repository.authenticateDevice).not.toHaveBeenCalled();
      await app.close();
    });

    it("lets a request carrying the edge header through to device authentication", async () => {
      const { app, repository } = harness({}, { proxyToken });
      const response = await app.inject({
        method: "POST",
        url: "/v1/sessions",
        headers: { "x-dataminer-proxy-token": proxyToken },
        payload: {},
      });

      // Past the gate, the ordinary device credential rules apply again: no
      // bearer token is still a 401, and the gate did not authenticate anyone.
      expect(response.statusCode).toBe(401);
      expect(response.json()).toMatchObject({ code: "invalid_device_credential" });
      expect(repository.authenticateDevice).not.toHaveBeenCalled();
      await app.close();
    });

    it("leaves /healthz and /readyz reachable without the edge header", async () => {
      // Cloud Run's own probes and any uptime check hit the origin directly and
      // carry no edge header. Gating them would make the service look down.
      const { app } = harness({}, { proxyToken });
      const health = await app.inject({ method: "GET", url: "/healthz" });
      const ready = await app.inject({ method: "GET", url: "/readyz" });

      expect(health.statusCode).toBe(200);
      expect(ready.statusCode).toBe(200);
      await app.close();
    });
  });

  it("reports its own environment from /readyz rather than a hardcoded label", async () => {
    // An operator tells production from staging with this probe, so the label
    // has to come from the deployment's profile.
    const { app } = harness({}, { environment: "production", profile: environmentProfile("production") });
    const ready = await app.inject({ method: "GET", url: "/readyz" });

    expect(ready.statusCode).toBe(200);
    expect(ready.json()).toMatchObject({ status: "ready", environment: "production" });
    await app.close();
  });

  it("authenticates the bearer principal without trusting a caller-supplied device id", async () => {
    const { app, repository } = harness();
    const response = await app.inject({ method: "POST", url: "/v1/sessions", payload: {} });

    expect(response.statusCode).toBe(401);
    expect(response.json()).toMatchObject({ code: "invalid_device_credential" });
    // A missing credential must not cost a query — the gate short-circuits
    // before the database so unauthenticated scanners are cheap.
    expect(repository.authenticateDevice).not.toHaveBeenCalled();
    await app.close();
  });

  it("rejects a well-formed bearer token that resolves to no live device", async () => {
    const { app, repository } = harness();
    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: "Bearer some-other-devices-credential" },
      payload: {},
    });

    expect(response.statusCode).toBe(401);
    // Same code and message as an absent credential: the caller learns that it
    // cannot proceed, never whether the credential exists but is suspended.
    expect(response.json()).toMatchObject({
      code: "invalid_device_credential",
      message: "a registered, active device credential is required",
    });
    expect(repository.authenticateDevice).toHaveBeenCalledWith(
      createHash("sha256").update("some-other-devices-credential").digest("hex"),
    );
    await app.close();
  });

  it("stops a credential whose device row went away between requests", async () => {
    // Revoking a device is a status change, not a secret rotation: the very
    // next request from a token that used to work must fail closed.
    const authenticateDevice = vi
      .fn<IngestionRepository["authenticateDevice"]>()
      .mockResolvedValueOnce({ id: deviceId, annotatorId, credentialFingerprint: tokenFingerprint })
      .mockResolvedValueOnce(null);
    const { app } = harness({ authenticateDevice });

    const first = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "revocation-key-1" },
      payload: {},
    });
    const second = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "revocation-key-2" },
      payload: {},
    });

    expect(first.statusCode).not.toBe(401);
    expect(second.statusCode).toBe(401);
    expect(second.json()).toMatchObject({ code: "invalid_device_credential" });
    await app.close();
  });

  it("returns policy_blocked for an explicit trainable claim before structural validation", async () => {
    const { app } = harness();
    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "request-key-1" },
      payload: { classification: { isTrainable: true } },
    });

    expect(response.statusCode).toBe(422);
    expect(response.json()).toMatchObject({ code: "policy_blocked", retryable: false });
    expect(response.headers["x-request-id"]).toBeTruthy();
    await app.close();
  });

  // 世代不是策略问题：admission 信封只钉基础代，音频代由 family.audio 声明
  // （#365 / docs/63 §3）。策略闸按单值比对曾把音频世代一刀切死——结构校验根本
  // 轮不到出场。这里钉住「音频代通过策略闸、落在结构校验」：响应是 400
  // invalid_request（缺其余字段），绝不能是 422 policy_blocked。
  it("lets the audio generation through the policy gate", async () => {
    const productionProfile = environmentProfile("production");
    const { app } = harness(
      {},
      {
        environment: "production",
        profile: productionProfile,
        admittedFamilies: [
          {
            family: primaryFamily(productionProfile),
            prefix: "raw/v1/non-trainable/dataminer-api-production",
          },
        ],
      },
    );
    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: {
        authorization: `Bearer ${token}`,
        "idempotency-key": "audio-dm-local-dev",
      },
      payload: {
        classification: {
          sourceFamily: "dm-local-dev",
          artifactSchemaVersion: "dm-local-dev-recording-2",
        },
      },
    });
    expect(response.statusCode).toBe(400);
    expect(response.json()).toMatchObject({ code: "invalid_request" });

    // 退役的 certified 家族（docs/100）仍然是策略拒绝，不是形状错误——一个
    // 还在发 dm-certified 的旧客户端必须拿到明确的 422，不能被当成 dev 收下。
    const retired = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "audio-retired" },
      payload: {
        classification: {
          sourceFamily: "dm-certified",
          artifactSchemaVersion: "dm-certified-recording-1",
        },
      },
    });
    expect(retired.statusCode).toBe(422);
    expect(retired.json()).toMatchObject({ code: "policy_blocked" });
    await app.close();
  });

  it("registers custom-verb routes with literal colons", async () => {
    const sessionId = "00000000-0000-4000-8000-000000000001";
    const sealedSession: SessionRecord = {
      id: sessionId,
      deviceId,
      localSessionId: "dev-1785811351848-0000",
      state: "sealed",
      sessionSchemaVersion: "dm-local-dev-session-2",
      createdAt: "2026-08-04T02:42:31.848Z",
      updatedAt: "2026-08-04T02:44:19.974Z",
    };
    const sealSession = vi.fn(async () => sealedSession);
    const { app } = harness({
      ...successfulIdempotencyRepository(),
      sealSession,
    });
    const response = await app.inject({
      method: "POST",
      url: `/v1/sessions/${sessionId}:seal`,
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "request-key-2" },
      payload: {
        endedAt: "2026-08-04T02:43:15.000Z",
        localFinalState: "completed",
        stopReason: "foreground_lost",
      },
    });

    expect(response.statusCode).toBe(200);
    expect(response.json()).toMatchObject({ sessionId, state: "sealed" });
    expect(sealSession).toHaveBeenCalledWith(tokenFingerprint, sessionId, {
      endedAt: "2026-08-04T02:43:15.000Z",
      localFinalState: "completed",
      stopReason: "foreground_lost",
    });
    await app.close();
  });

  it("registers the clip purge custom verb without treating the colon as a parameter", async () => {
    const clipId = "00000000-0000-4000-8000-000000000001";
    const clip: ClipRecord = {
      id: clipId,
      sessionId: "00000000-0000-4000-8000-000000000002",
      runId: "00000000-0000-4000-8000-000000000003",
      segmentIndex: 0,
      revision: 1,
      state: "verified",
      storageState: "stored",
      verificationState: "verified",
      retentionState: "purge_requested",
      retentionDeadline: "2027-08-04T02:44:19.974Z",
      artifacts: [],
      isTrainable: false,
      trainabilityBlockers: ["non_trainable_source"],
      createdAt: "2026-08-04T02:43:17.878Z",
      updatedAt: "2026-08-04T02:44:19.974Z",
    };
    const requestClipPurge = vi.fn(async () => undefined);
    const { app } = harness({
      ...successfulIdempotencyRepository(),
      requestClipPurge,
      getClip: vi.fn(async () => clip),
    });
    const response = await app.inject({
      method: "POST",
      url: `/v1/clips/${clipId}:purge`,
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "request-key-3" },
      payload: { reason: "device_principal_delete" },
    });

    expect(response.statusCode).toBe(202);
    expect(response.json()).toMatchObject({ clipId, retentionState: "purge_requested" });
    expect(requestClipPurge).toHaveBeenCalledWith(tokenFingerprint, clipId);
    await app.close();
  });
});

// docs/49 E7: lane selection. The request's classification names its lane; the
// deployment's admitted families decide whether that lane exists here at all.
// docs/49 E7 的 first-party 车道随 docs/57 §3.3 退役，生产 profile 只剩一条
// 家族。原来这一组测的是「声明哪条车道就路由到哪条」；车道没了，能测也该测的
// 就变成一件事：**旧客户端仍然会声明它，服务端必须挡住**。
describe("retired first-party lane", () => {
  function retiredLanePayload() {
    const digest = `sha256:${"a".repeat(64)}`;
    const profileRef = (id: string) => ({ id, version: "1", contentHash: digest });
    return {
      localSessionId: "fp-1784791023860-0000",
      sessionSchemaVersion: "dm-local-dev-session-2",
      startedAt: "2026-07-25T00:00:00.000Z",
      localState: "completed",
      classification: {
        sourceFamily: "dm-first-party-release",
        contractVersion: "dm-1",
        artifactSchemaVersion: "dm-local-dev-recording-1",
        environment: "production",
        storageNamespace: "raw",
        nonTrainable: false,
        isTrainable: false,
        productionCertified: false,
      },
      declaredConsentId: "33333333-3333-4333-8333-333333333333",
      clientVersion: "0.5.1",
      catalog: {
        id: "dataminer-some-catalog",
        version: "1",
        mode: "first_party_release",
        contentHash: digest,
      },
      game: { id: "first-party-game", version: "1", contentHash: digest, executableSha256: digest },
      deviceProfile: profileRef("fp-device"),
      recordingProfile: {
        ...profileRef("fp-recording"),
        width: 1920,
        height: 1080,
        fpsNum: 60,
        fpsDen: 1,
        captureEngine: "native_wgc_window_v1",
      },
      mapping: profileRef("fp-mapping"),
    };
  }

  it("refuses a first-party declaration on every deployment", async () => {
    const { app } = harness();
    const response = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { authorization: `Bearer ${token}`, "idempotency-key": "fp-key-retired" },
      payload: retiredLanePayload(),
    });

    // 形状不再属于任何已准入家族，所以在 schema 层就落地——不是 policy_blocked。
    expect(response.statusCode).toBeGreaterThanOrEqual(400);
    expect(response.statusCode).toBeLessThan(500);
    await app.close();
  });
});

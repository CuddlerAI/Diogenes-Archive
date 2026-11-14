import { createHash, randomUUID, timingSafeEqual } from "node:crypto";

import Fastify, {
  type FastifyReply,
  type FastifyRequest,
  type FastifyServerOptions,
} from "fastify";
import { ZodError, type ZodType } from "zod";

import {
  CATALOG_CHANNEL_HEADER,
  resolveCatalogDelivery,
  type CatalogChannel,
} from "./catalog-delivery.js";
import type { AdmittedFamily, AppConfig } from "./config.js";
import { primaryFamily, type AdmissionFamily } from "./environment.js";
import {
  clipDeclarationPathParamsSchema,
  clipPathParamsSchema,
  completeUploadIntentSchema,
  createSessionSchemaFor,
  type CreateSessionInput,
  createUploadIntentSchema,
  idempotencyKeySchema,
  putClipSchemaFor,
  putRunSchemaFor,
  putSessionArtifactsSchema,
  requestClipPurgeSchema,
  runPathParamsSchema,
  sealSessionSchema,
  sessionArtifactPathParamsSchema,
  sessionPathParamsSchema,
  signPartsSchema,
  signSessionArtifactSchema,
  uploadIntentPathParamsSchema,
} from "./contracts.js";
import type { ClipRecord, IngestionRepository } from "./domain.js";
import { ApiError, badRequest, notFound, policyBlocked, unauthorized } from "./errors.js";
import { runIdempotent, runResignableIdempotent } from "./idempotency.js";
import { IngestionService } from "./service.js";
import { storageErrorDiagnostics, type ObjectStorage } from "./storage.js";

export type AppDependencies = {
  config: AppConfig;
  repository: IngestionRepository;
  storage: ObjectStorage;
  /**
   * 原为 `boolean`。放宽到 Fastify 自己的类型是为了让**结构化日志字段可被断言**——
   * 拒绝路径的 deviceId 是 docs/101 行动项 D 的全部意义，不能只靠人读日志确认。
   * 运行时行为不变：这个值一直是直接传给 `Fastify({ logger })` 的。
   */
  logger?: FastifyServerOptions["logger"];
};

/**
 * 已认证请求的设备身份，只为让 `setErrorHandler` 能把它写进结构化日志。
 *
 * 为什么需要：被拒绝的请求**在数据库里不留任何行**——404/409 抛在写入之前，
 * `sessions` / `audit_events` / `idempotency_requests` 三处皆无（docs/101 §3）。
 * 唯一的痕迹是应用日志，而它此前只有 `requestId`，没有设备身份，所以
 * 2026-08-05/06 那 31 次拒绝只能靠时间戳去猜是哪台机器（docs/101 §3.2 的归属限度）。
 *
 * 用 WeakMap 而不是 `decorateRequest`：这份身份**只能出现在日志里**，不进任何
 * 响应体，也不需要给 FastifyRequest 做全局类型增广。请求对象回收时自动清掉。
 */
const authenticatedPrincipal = new WeakMap<
  FastifyRequest,
  { deviceId: string; annotatorId: string | null }
>();

/**
 * 鉴权之前的拒绝（边缘令牌 404、空 Bearer 401）本来就没有身份可记，
 * 返回空对象即可——那正是「这一次连是谁都不知道」的真实记录。
 */
function principalLogFields(
  request: FastifyRequest,
): { deviceId: string; annotatorId: string | null } | Record<string, never> {
  return authenticatedPrincipal.get(request) ?? {};
}

export function buildApp(dependencies: AppDependencies) {
  // Bound once from the deployment's own admitted families: a request declaring
  // another environment or an unadmitted lane fails here, it does not fall
  // through to a shared shape.
  const profile = dependencies.config.profile;
  // 目录渠道按部署推导。certified 车道废除后（docs/100 §4.3）请求级**路由**退役：
  // 只剩一条渠道时，「客户端说自己要哪条」没有第二个答案。新客户端不再发这个头
  // （catalog_delivery.rs），旧客户端不带头时本来就走部署渠道，两者行为一致。
  //
  // 但**不静默**（docs/100 §7 的负面用例）：一个仍然声明 `certified` 的旧客户端，
  // 它 pin 的是那把已退役的 KMS 锚——把 dev 字节发给它，它会以一句
  // "signature verification failed" 倒下，排障方向完全错了。明确 400 指名道姓，
  // 让"你要的那条渠道没有了"变成一句服务端说得出口的话。
  const catalogChannel: CatalogChannel = "dev";
  function assertRequestedChannelIsServable(request: FastifyRequest): void {
    const raw = request.headers[CATALOG_CHANNEL_HEADER];
    const value = Array.isArray(raw) ? (raw.length === 1 ? raw[0] : undefined) : raw;
    if (value === undefined || value === catalogChannel) return;
    throw badRequest(
      "invalid_catalog_channel",
      `${CATALOG_CHANNEL_HEADER} names a channel this deployment does not serve; the certified channel was retired (docs/100). Omit the header.`,
    );
  }
  const admitted = dependencies.config.admittedFamilies.map((entry) => ({
    ...entry,
    createSessionSchema: createSessionSchemaFor(entry.family),
    putClipSchema: putClipSchemaFor(entry.family),
  }));
  const putRunSchema = putRunSchemaFor(admitted.map((entry) => entry.family.preflightResult));
  const app = Fastify({
    logger: dependencies.logger ?? false,
    bodyLimit: 2 * 1024 * 1024,
    genReqId: () => `req_${randomUUID()}`,
  });
  const service = new IngestionService(
    dependencies.repository,
    dependencies.storage,
    dependencies.config.r2.bucket,
    dependencies.config.admittedFamilies,
    dependencies.config.r2.partSizeBytes,
    dependencies.config.r2.presignTtlSeconds,
    dependencies.config.worker.multipartCompletionLeaseSeconds,
  );

  // The lane a request means is the one its classification names. An absent or
  // unknown sourceFamily is refused as policy before structural validation, the
  // same way a contradicting classification key is.
  function admittedFamily(body: unknown): (typeof admitted)[number] {
    const declared =
      typeof body === "object" && body !== null && "classification" in body
        ? (body as { classification?: { sourceFamily?: unknown } }).classification?.sourceFamily
        : undefined;
    const entry = admitted.find((candidate) => candidate.family.sourceFamily === declared);
    if (!entry) throw policyBlocked();
    return entry;
  }

  app.setErrorHandler((error, request, reply) => {
    if (error instanceof ZodError) {
      request.log.warn(
        {
          requestId: request.id,
          statusCode: 400,
          errorCode: "invalid_request",
          issuePaths: error.issues.map((issue) => issue.path.join(".")),
          ...principalLogFields(request),
        },
        "handled request error",
      );
      return reply.code(400).send({
        code: "invalid_request",
        message: `request did not match the ${profile.environment} ingestion contract`,
        requestId: request.id,
        retryable: false,
        details: {
          issues: error.issues.map((issue) => ({ path: issue.path.join("."), message: issue.message })),
        },
      });
    }
    if (error instanceof ApiError) {
      request.log.warn(
        {
          requestId: request.id,
          statusCode: error.statusCode,
          errorCode: error.code,
          retryable: error.retryable,
          ...(error.diagnostics ?? {}),
          ...principalLogFields(request),
        },
        "handled request error",
      );
      return reply.code(error.statusCode).send({
        code: error.code,
        message: error.message,
        requestId: request.id,
        retryable: error.retryable,
      });
    }
    const databaseCode = (error as { code?: unknown }).code;
    if (databaseCode === "23505") {
      request.log.warn(
        {
          requestId: request.id,
          statusCode: 409,
          errorCode: "resource_identity_conflict",
          databaseCode,
          ...principalLogFields(request),
        },
        "handled request error",
      );
      return reply.code(409).send({
        code: "resource_identity_conflict",
        message: "the immutable resource identity is already in use",
        requestId: request.id,
        retryable: false,
      });
    }
    if (databaseCode === "23514" || databaseCode === "22003") {
      request.log.warn(
        {
          requestId: request.id,
          statusCode: 400,
          errorCode: "invalid_request",
          databaseCode,
          ...principalLogFields(request),
        },
        "handled request error",
      );
      return reply.code(400).send({
        code: "invalid_request",
        message: `request values violate the ${profile.environment} ingestion contract`,
        requestId: request.id,
        retryable: false,
      });
    }
    const transportStatus = (error as { statusCode?: unknown }).statusCode;
    if (typeof transportStatus === "number" && transportStatus >= 400 && transportStatus < 500) {
      const errorCode = transportStatus === 413 ? "request_too_large" : "invalid_request";
      request.log.warn(
        {
          requestId: request.id,
          statusCode: transportStatus,
          errorCode,
          ...principalLogFields(request),
        },
        "handled request error",
      );
      return reply.code(transportStatus).send({
        code: errorCode,
        message: transportStatus === 413 ? "request body exceeds the metadata limit" : "request could not be parsed",
        requestId: request.id,
        retryable: false,
      });
    }
    request.log.error(
      {
        requestId: request.id,
        errorName: safeErrorName(error),
        ...storageErrorDiagnostics(error),
        ...(typeof databaseCode === "string" ? { databaseCode } : {}),
        ...principalLogFields(request),
      },
      "unhandled request error",
    );
    return reply.code(500).send({
      code: "internal_error",
      message: "request failed",
      requestId: request.id,
      retryable: true,
    });
  });

  // Every `/v1/*` caller authenticates as a device: the bearer token is hashed
  // and matched against `devices.credential_fingerprint`, which the console
  // issues (10 §6.6). The previous single shared staging secret is gone — it
  // could not distinguish one device from another, so the fleet was capped at
  // the one row whose fingerprint happened to be sha256 of that secret.
  //
  // The per-transaction `deviceForPrincipal` lookup downstream stays: this hook
  // is the gate, that one is the invariant inside the write's own transaction.
  app.addHook("onRequest", async (request) => {
    if (!request.url.startsWith("/v1/")) return;

    // The edge Worker at api.diogenesmodel.com is the only sanctioned way in.
    // The Cloud Run origin behind it must admit `allUsers` — a desktop client
    // authenticates with a per-device bearer token, not a Google identity — so
    // the bare `*.run.app` URL would otherwise be an unguarded bypass of the
    // Cloudflare edge. Anything arriving without the header the Worker injects
    // is answered as if the route did not exist: 404, not 401, because a probe
    // must not learn that an ingestion API lives here. This runs before the
    // device lookup so the answer cannot vary with what the caller sent, and
    // before any database work so a scanner costs one string compare.
    // `/healthz` and `/readyz` never reach this branch, so Cloud Run's own
    // probes and uptime checks stay unauthenticated.
    if (dependencies.config.proxyToken !== null) {
      const presented = request.headers["x-dataminer-proxy-token"];
      if (typeof presented !== "string" || !secretEquals(presented, dependencies.config.proxyToken)) {
        throw notFound("not_found", "the requested resource does not exist");
      }
    }

    const authorization = request.headers.authorization;
    const token = authorization?.startsWith("Bearer ") ? authorization.slice(7) : "";
    // Short-circuit before touching the database so unauthenticated scanners
    // cost a string compare rather than a query.
    if (token.length === 0) {
      throw unauthorized(
        "invalid_device_credential",
        "a registered, active device credential is required",
      );
    }
    const device = await dependencies.repository.authenticateDevice(requestPrincipal(request));
    if (!device) {
      // Deliberately one message for unknown, retired, suspended, and
      // annotator-suspended credentials: the caller learns that it cannot
      // proceed, not which of those it is.
      throw unauthorized(
        "invalid_device_credential",
        "a registered, active device credential is required",
      );
    }
    // 从这里往后的任何拒绝都能说出是哪台设备。这不改变准入判定，只是让
    // 「一次拒绝」不再是一条无主的日志（docs/101 行动项 D）。
    authenticatedPrincipal.set(request, {
      deviceId: device.id,
      annotatorId: device.annotatorId,
    });
  });
  app.addHook("onSend", async (request, reply, payload) => {
    reply.header("x-request-id", request.id);
    return payload;
  });

  app.get("/healthz", async () => ({
    status: "ok",
    service: `dataminer-api-${profile.environment}`,
    version: "0.1.0",
  }));
  app.get("/readyz", async (request, reply) => {
    const cloudSql = await readiness(() => dependencies.repository.ping());
    const r2ControlPlane = await readiness(() => dependencies.storage.ping());
    if (cloudSql.status !== "ready" || r2ControlPlane.status !== "ready") {
      return reply.code(503).send({
        code: "dependency_unavailable",
        message: `one or more ${profile.environment} dependencies are unavailable`,
        requestId: request.id,
        retryable: true,
        details: { cloudSql, r2ControlPlane },
      });
    }
    return {
      status: "ready",
      // Reported from the deployment's own profile. This was hardcoded to
      // "staging", which made a production readiness probe answer "staging" —
      // exactly the check an operator uses to tell the two apart.
      environment: profile.environment,
      dependencies: { cloudSql, r2ControlPlane },
    };
  });

  app.post("/v1/sessions", async (request, reply) => {
    const lane = admittedFamily(request.body);
    enforceDeclaredClassification(lane.family, request.body);
    // The conditional declaredConsentId field defeats static inference over the
    // per-family schema union; the runtime shape is exactly CreateSessionInput.
    const body = parse(lane.createSessionSchema, request.body) as CreateSessionInput;
    const deviceId = requestPrincipal(request);
    return sendIdempotent(dependencies.repository, request, reply, deviceId, body, 201, async () => {
      const session = await service.createSession(deviceId, body, lane.family);
      return {
        sessionId: session.id,
        localSessionId: session.localSessionId,
        state: session.state,
        classification: declaredClassification(lane.family),
        ...sessionEligibility(lane.family),
        createdAt: session.createdAt,
        updatedAt: session.updatedAt,
      };
    }, (session) => ({ location: `/v1/sessions/${session.sessionId}` }));
  });

  app.put("/v1/sessions/:sessionId/runs/:localRunId", async (request, reply) => {
    const params = parse(runPathParamsSchema, request.params);
    const body = parse(putRunSchema, request.body);
    const deviceId = requestPrincipal(request);
    return sendIdempotent(dependencies.repository, request, reply, deviceId, { params, body }, 200, async () => {
      const run = await service.putRun(deviceId, params.sessionId, params.localRunId, body);
      return {
        runId: run.id,
        sessionId: run.sessionId,
        localRunId: run.localRunId,
        sequence: run.sequence,
        state: run.state,
        createdAt: run.createdAt,
        updatedAt: run.updatedAt,
      };
    });
  });

  app.put(
    "/v1/sessions/:sessionId/runs/:localRunId/clips/:segmentIndex",
    async (request, reply) => {
      const params = parse(clipDeclarationPathParamsSchema, request.params);
      const lane = admittedFamily(request.body);
      enforceDeclaredClassification(lane.family, request.body);
      const body = parse(lane.putClipSchema, request.body);
      const deviceId = requestPrincipal(request);
      return sendIdempotent(
        dependencies.repository,
        request,
        reply,
        deviceId,
        { params, body },
        200,
        async () => {
          const clip = await service.putClip(
            deviceId,
            params.sessionId,
            params.localRunId,
            params.segmentIndex,
            body,
            lane.family,
          );
          return {
            clipId: clip.id,
            sessionId: clip.sessionId,
            runId: clip.runId,
            segmentIndex: clip.segmentIndex,
            revision: clip.revision,
            storageState: clip.storageState,
            verificationState: clip.verificationState,
            eligibilityState: clip.isTrainable ? "eligible" : "blocked",
            classification: declaredClassification(lane.family),
            createdAt: clip.createdAt,
            updatedAt: clip.updatedAt,
          };
        },
      );
    },
  );

  app.post("/v1/clips/:clipId/upload-intents", async (request, reply) => {
    const params = parse(clipPathParamsSchema, request.params);
    const body = parse(createUploadIntentSchema, request.body);
    const deviceId = requestPrincipal(request);
    return sendIdempotent(
      dependencies.repository,
      request,
      reply,
      deviceId,
      { params, body },
      201,
      () => service.createUploadIntent(deviceId, params.clipId, body),
    );
  });

  app.post("/v1/upload-intents/:uploadIntentId/parts::sign", async (request, reply) => {
    const params = parse(uploadIntentPathParamsSchema, request.params);
    const body = parse(signPartsSchema, request.body);
    const deviceId = requestPrincipal(request);
    const result = await runResignableIdempotent(
      dependencies.repository,
      `${deviceId}:${request.method}:${request.routeOptions.url}`,
      requestIdempotencyKey(request),
      { params, body },
      () => service.signParts(deviceId, params.uploadIntentId, body),
    );
    reply.header("idempotency-replayed", String(result.replayed));
    return reply.code(200).send(result.value);
  });

  app.post("/v1/upload-intents/:uploadIntentId/complete", async (request, reply) => {
    const params = parse(uploadIntentPathParamsSchema, request.params);
    const body = parse(completeUploadIntentSchema, request.body);
    const deviceId = requestPrincipal(request);
    return sendIdempotent(
      dependencies.repository,
      request,
      reply,
      deviceId,
      { params, body },
      202,
      () => service.completeUploadIntent(deviceId, params.uploadIntentId, body),
    );
  });

  app.get("/v1/clips/:clipId/status", async (request) => {
    const params = parse(clipPathParamsSchema, request.params);
    const clip = await service.getClip(requestPrincipal(request), params.clipId);
    return clipStatus(clip);
  });

  // find-my-way only treats `::` as a literal colon after a dynamic parameter
  // when the parameter has an explicit boundary. Without the regexp it names
  // the parameter `clipId::purge` and leaves `params.clipId` undefined.
  app.post("/v1/clips/:clipId(^[^:]+)::purge", async (request, reply) => {
    const params = parse(clipPathParamsSchema, request.params);
    const body = parse(requestClipPurgeSchema, request.body);
    const deviceId = requestPrincipal(request);
    return sendIdempotent(
      dependencies.repository,
      request,
      reply,
      deviceId,
      { params, body },
      202,
      async () => clipStatus(await service.requestClipPurge(deviceId, params.clipId)),
    );
  });

  app.post("/v1/sessions/:sessionId(^[^:]+)::seal", async (request, reply) => {
    const params = parse(sessionPathParamsSchema, request.params);
    const body = parse(sealSessionSchema, request.body);
    const deviceId = requestPrincipal(request);
    return sendIdempotent(
      dependencies.repository,
      request,
      reply,
      deviceId,
      { params, body },
      200,
      async () => {
        const session = await service.sealSession(deviceId, params.sessionId, body);
        // The seal body carries no classification block; the echo comes from the
        // lane the session was admitted under, resolved from the persisted
        // `sessions.schema_version` like every other lane decision (issue #855
        // §B). Falling back to the primary family only matters for a lane later
        // unconfigured, and is echo-only.
        const family =
          profile.families.find(
            (candidate) =>
              candidate.sessionSchemaVersion === session.sessionSchemaVersion ||
              candidate.audio?.sessionSchemaVersion === session.sessionSchemaVersion,
          ) ?? profile.families[0]!;
        return {
          sessionId: session.id,
          localSessionId: session.localSessionId,
          state: session.state,
          classification: declaredClassification(family),
          ...sessionEligibility(family),
          createdAt: session.createdAt,
          updatedAt: session.updatedAt,
        };
      },
    );
  });

  // docs/47: session-root artifacts. Naturally idempotent (immutable declare,
  // deterministic verify), so no idempotency-key machinery. Auth is the /v1/* hook.
  app.put("/v1/sessions/:sessionId/artifacts", async (request, reply) => {
    const params = parse(sessionPathParamsSchema, request.params);
    const body = parse(putSessionArtifactsSchema, request.body);
    const result = await service.declareSessionArtifacts(
      requestPrincipal(request),
      params.sessionId,
      body,
    );
    return reply.code(200).send(result);
  });

  app.get("/v1/sessions/:sessionId/artifacts", async (request) => {
    const params = parse(sessionPathParamsSchema, request.params);
    return service.getSessionArtifacts(requestPrincipal(request), params.sessionId);
  });

  app.post("/v1/sessions/:sessionId/artifacts/:kind/sign", async (request, reply) => {
    const params = parse(sessionArtifactPathParamsSchema, request.params);
    const body = parse(signSessionArtifactSchema, request.body);
    const result = await service.signSessionArtifact(
      requestPrincipal(request),
      params.sessionId,
      params.kind,
      body.contentMd5,
    );
    return reply.code(200).send(result);
  });

  app.post("/v1/sessions/:sessionId/artifacts/:kind/complete", async (request, reply) => {
    const params = parse(sessionArtifactPathParamsSchema, request.params);
    const result = await service.completeSessionArtifact(
      requestPrincipal(request),
      params.sessionId,
      params.kind,
    );
    return reply.code(200).send(result);
  });

  // D3: the GET path is read-only over one immutable published release. Signing
  // and client-source validation happened once in D2, before the current pointer
  // moved; polling can therefore never call KMS or reconstruct JSONB.
  app.get("/v1/catalog", async (request, reply) => {
    assertRequestedChannelIsServable(request);
    const releases = await dependencies.repository.currentCatalogRelease(catalogChannel);
    // 响应形状由共享的 catalog-delivery 决定，绑定前的 console 通道用的是同一个
    // 函数（docs/37 §Bootstrap）。这条路自己只负责一件事：调它的必须是一台已登记
    // 的设备——那道闸在 `/v1/*` 的 onRequest 钩子里，本次改动一个字都没动。
    const delivery = resolveCatalogDelivery(releases, request.headers);
    for (const [name, value] of Object.entries(delivery.headers)) {
      reply.header(name, value);
    }
    switch (delivery.kind) {
      case "not_published":
        return reply.code(503).send({
          code: "catalog_not_published",
          message: `no ${catalogChannel} catalog release is currently published`,
          requestId: request.id,
          retryable: true,
        });
      // 消息要说清是**哪一项**不合格。缺一个请求头和"客户端版本太老"在契约上
      // 都是 409 fail-closed（docs/37 §Request），但排障动作完全不同：前者改调用
      // 方，后者升级客户端。旧文案统一说"目录与客户端不兼容"，把一次漏带请求头
      // 读成了服务端发错了东西。code 不变，只让 message 可诊断。
      case "incompatible":
        return reply.code(409).send({
          code: "catalog_incompatible",
          message: delivery.reason,
          requestId: request.id,
          retryable: false,
        });
      case "not_modified":
        return reply.code(304).send();
      case "ok":
        return reply.type("application/json; charset=utf-8").send(delivery.body);
    }
  });

  // Live ingestion admission is deliberately separate from the signed catalog
  // bytes: registering or pausing a game takes effect on the next client list
  // refresh without publishing a new catalog or shipping a new installer.
  app.get("/v1/game-admissions", async () => ({
    schemaVersion: "dm-game-admission-1",
    games: await dependencies.repository.listGameAdmissions(),
  }));

  return app;
}

function parse<T>(schema: ZodType<T>, value: unknown): T {
  return schema.parse(value);
}

// Constant time in the length-equal case, which is the only one an attacker can
// steer towards: an unequal length is already a mismatch and says nothing about
// the bytes. timingSafeEqual itself throws on differing lengths.
function secretEquals(left: string, right: string): boolean {
  const leftBytes = Buffer.from(left, "utf8");
  const rightBytes = Buffer.from(right, "utf8");
  if (leftBytes.length !== rightBytes.length) return false;
  return timingSafeEqual(leftBytes, rightBytes);
}

function requestPrincipal(request: FastifyRequest): string {
  const authorization = request.headers.authorization;
  const token = authorization?.startsWith("Bearer ") ? authorization.slice(7) : "";
  return createHash("sha256").update(token).digest("hex");
}

function requestIdempotencyKey(request: FastifyRequest): string {
  return idempotencyKeySchema.parse(request.headers["idempotency-key"]);
}

async function sendIdempotent<T>(
  repository: IngestionRepository,
  request: FastifyRequest,
  reply: FastifyReply,
  deviceId: string,
  fingerprint: unknown,
  statusCode: number,
  operation: () => Promise<T>,
  responseHeaders?: (body: T) => Record<string, string>,
) {
  const response = await runIdempotent(
    repository,
    `${deviceId}:${request.method}:${request.routeOptions.url}`,
    requestIdempotencyKey(request),
    fingerprint,
    async () => {
      const body = await operation();
      return {
        statusCode,
        body,
        ...(responseHeaders ? { headers: responseHeaders(body) } : {}),
      };
    },
  );
  for (const [name, value] of Object.entries(response.headers ?? {})) {
    reply.header(name, value);
  }
  reply.header("idempotency-replayed", String(response.replayed));
  return reply.code(response.statusCode).send(response.body);
}

function clipStatus(clip: ClipRecord) {
  return {
    clipId: clip.id,
    storageState: clip.storageState,
    verificationState: clip.verificationState,
    retentionState: clip.retentionState,
    retentionDeadline: clip.retentionDeadline,
    // Echoed from `clip_trainability`, the sole trainability decision. Before
    // multi-family admission this was hardcoded blocked/non_trainable_source,
    // which was accurate for the dev-only stage but would misreport the
    // consented lane.
    eligibilityState: clip.isTrainable ? "eligible" : "blocked",
    ...(clip.isTrainable
      ? {}
      : { eligibilityReason: clip.trainabilityBlockers[0] ?? "non_trainable_source" }),
    artifacts: clip.artifacts.map((artifact) => ({
          artifactType: artifact.kind,
          state:
            artifact.state === "verified" ||
            artifact.state === "rejected" ||
            artifact.state === "purging" ||
            artifact.state === "purged"
              ? artifact.state
              : artifact.state === "declared"
                ? "pending"
                : artifact.state === "uploading"
                  ? "uploading"
                  : "stored",
          byteSize: artifact.expectedBytes,
          expectedSha256: artifact.expectedSha256,
          ...(artifact.rejectionCode !== undefined ? { rejectionCode: artifact.rejectionCode } : {}),
        })),
    ...(clip.state === "rejected"
      ? { rejectionCode: clip.artifacts.find((artifact) => artifact.rejectionCode)?.rejectionCode ?? "verification_rejected" }
      : {}),
    updatedAt: clip.updatedAt,
  };
}

// Echoed back so a client can confirm the server agrees with what it declared.
function declaredClassification(family: AdmissionFamily) {
  return family.admission;
}

// Session-level eligibility echo. The dev lane is blocked at the source; the
// consented lane's verdict is per-clip (`clip_trainability`), so a session-level
// echo can only say the derivation is pending, not that it passed.
function sessionEligibility(family: AdmissionFamily) {
  return family.bestClassification === "training_candidate"
    ? { eligibilityState: "pending", eligibilityReason: "trainability_derived_per_clip" }
    : { eligibilityState: "blocked", eligibilityReason: "non_trainable_source" };
}

function safeErrorName(error: unknown): string {
  return error instanceof Error && error.name ? error.name : "unknown_error";
}

async function readiness(operation: () => Promise<void>) {
  const started = performance.now();
  try {
    await operation();
    return { status: "ready" as const, latencyMs: Math.max(0, Math.round(performance.now() - started)) };
  } catch {
    return { status: "unavailable" as const, latencyMs: Math.max(0, Math.round(performance.now() - started)) };
  }
}

// Runs before structural validation so a declaration that contradicts the
// admitted lane is refused as policy, not reported as a shape error.
function enforceDeclaredClassification(family: AdmissionFamily, value: unknown): void {
  if (typeof value !== "object" || value === null || !("classification" in value)) return;
  const classification = (value as { classification?: unknown }).classification;
  if (typeof classification !== "object" || classification === null) return;
  const declared = classification as Record<string, unknown>;
  // artifactSchemaVersion 是家族的世代**枚举**（基础代 | 音频代），不是单值：
  // admission 信封只钉基础代，音频代由 family.audio 声明（#365 / docs/63 §3）。
  // 按 admission 单值比对会把整条车道的音频世代在策略层一刀切死——合同层
  // （classificationSchemaFor 的 enum）根本轮不到出场。
  const allowedArtifactVersions: readonly string[] = family.audio
    ? [family.artifactSchemaVersion, family.audio.artifactSchemaVersion]
    : [family.artifactSchemaVersion];
  if (
    "artifactSchemaVersion" in declared &&
    !allowedArtifactVersions.includes(declared["artifactSchemaVersion"] as string)
  ) {
    throw policyBlocked();
  }
  // 中性契约里已退役的键（issue #855 §B）出现即拒，取值是什么都不看：新契约
  // "不再重复携带互相可能打架的字段"必须是一条能执行的断言，而不是一句注释。
  // 拒在这里而不是留给 `.strict()`，是为了让它答成 422 policy_blocked——"这条车道
  // 不收这个词"是策略，不是请求形状写错了。
  for (const key of family.retiredClassificationKeys) {
    if (key in declared) throw policyBlocked();
  }
  const expected: Record<string, unknown> = { ...family.admission };
  delete expected["artifactSchemaVersion"];
  for (const [key, expectedValue] of Object.entries(expected)) {
    if (key in declared && declared[key] !== expectedValue) {
      throw policyBlocked();
    }
  }
}

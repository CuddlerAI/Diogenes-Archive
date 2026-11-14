import { createHash, randomUUID } from "node:crypto";

import Fastify, { type FastifyReply, type FastifyRequest } from "fastify";
import { ZodError } from "zod";

import { resolveCatalogDelivery } from "@dataminer/api/catalog-delivery";

import {
  adminProfileUpdateSchema,
  annotatorCredentialInviteSchema,
  annotatorRegistrationSchema,
  appLoginSchema,
  appPasswordResetConfirmSchema,
  appPasswordResetRequestSchema,
  consentRenewalSchema,
  changePasswordSchema,
  claimConfirmSchema,
  claimParamsSchema,
  clientEventBatchSchema,
  createInviteSchema,
  deviceIdParamsSchema,
  enrollDeviceSchema,
  inviteCodeParamsSchema,
  inviteListQuerySchema,
  profileSchema,
  signupResendSchema,
  signupSchema,
  signupVerifySchema,
} from "./app-contracts.js";
import type { AppService } from "./app-service.js";
import type { AnnotatorAdminService } from "./annotator-admin.js";
import { InviteAdminService, type InviteAdminRepository } from "./invite-admin.js";
import { AuthService, type RequestContext } from "./auth.js";
import { CfAccessVerifier, type CfAccessIdentityVerifier } from "./cf-access.js";
import {
  annotatorListQuerySchema,
  auditQuerySchema,
  clientEventQuerySchema,
  clipListQuerySchema,
  contributionQuerySchema,
  deviceListQuerySchema,
  idParamsSchema,
  sessionListQuerySchema,
  statsDailyQuerySchema,
  statsOutputQuerySchema,
} from "./browse-contracts.js";
import type { BrowseRepository } from "./browse-domain.js";
import type { AcceptanceService } from "./acceptance.js";
import type { ConsoleConfig } from "./config.js";
import {
  catalogQuerySchema,
  catalogReleaseQuerySchema,
  clipPurgeSchema,
  clipQualitySchema,
  reviewVerdictQuerySchema,
  reviewVerdictSchema,
  createAnnotatorSchema,
  createConsentSchema,
  createDeviceSchema,
  idempotencyKeySchema,
  publishCatalogReleaseSchema,
  revokeSchema,
  sessionQualitySchema,
  updateAnnotatorSchema,
  updateDeviceSchema,
} from "./governance-contracts.js";
import type { GovernanceRepository } from "./governance-domain.js";
import { GovernanceService } from "./governance.js";
import {
  acceptInviteSchema,
  createOperatorSchema,
  loginSchema,
  operatorIdParamsSchema,
  otpResendSchema,
  otpVerifySchema,
  passwordResetConfirmSchema,
  passwordResetRequestSchema,
  sudoSchema,
  updateOperatorSchema,
} from "./contracts.js";
import type { AuthenticatedSession, ConsoleRepository, OperatorRecord } from "./domain.js";
import { ConsoleError, forbidden, notFound, unauthorized } from "./errors.js";
import {
  appCsrfHeaderName,
  appSessionCookieName,
  assertBrowserSameOriginWrite,
  assertSameOriginWrite,
  clearAppSessionCookie,
  clearSessionCookie,
  clientAddress,
  readCookie,
  requireJsonBody,
  sessionCookieName,
  setAppSessionCookie,
  setSessionCookie,
  truncatedUserAgent,
} from "./http.js";
import type { Mailer } from "./mailer.js";
import type { PreviewService } from "./preview.js";
import {
  hfMarkSchema,
  hfReconcileSchema,
  hfStatusQuerySchema,
  reviewObjectQuerySchema,
  reviewSignVideoSchema,
  reviewSignVizSchema,
  reviewTreeQuerySchema,
} from "./review-contracts.js";
import type { ReviewService } from "./review.js";
import type { HfService } from "./hf.js";
import { capabilitiesFor, requireCapability } from "./rbac.js";
import { hashSessionToken, hashSubject, secretEquals } from "./secrets.js";

export type ConsoleDependencies = {
  config: ConsoleConfig;
  repository: ConsoleRepository;
  browse: BrowseRepository;
  governance: GovernanceRepository;
  /** Invite administration. Optional so existing test harnesses stay valid; when
   * absent the `/console/v1/invites` routes are simply not registered. */
  invites?: InviteAdminRepository;
  preview: PreviewService;
  /** The data-review surface (docs/33). Optional so existing test harnesses
   * stay valid; when absent the `/console/v1/review` routes are simply not
   * registered and `/readyz` reports the feature off. */
  review?: ReviewService;
  /** HF release reconciliation (docs/33 §6). Same optionality contract. */
  hf?: HfService;
  /**
   * 会话验收报告读取（docs/136 §4）。Optional so existing test harnesses stay
   * valid；absent 时验收路由不注册。注意与 review 的一处不同：index.ts **总是**
   * 传入本服务（即便 ACCEPTANCE_R2_* 未配置）——路由必须存在并答 503「存储未
   * 配置」，而不是让未配置的部署伪装成 404。
   */
  acceptance?: AcceptanceService;
  mailer: Mailer;
  /**
   * The annotator-facing surface (docs/11 §4.2). Optional so a deployment can
   * serve the governance console alone; when it is absent no `/app/v1` route
   * exists at all, and `/readyz` says so rather than leaving it ambiguous.
   *
   * It arrives already bound to its own pool and database role. Nothing in this
   * file may hand it the console repository.
   */
  appService?: AppService;
  catalogPublisher?: {
    publish(input: {
      channel: "dev";
      catalogId: string;
      catalogVersion: string;
      actor: string;
      trustedCatalogVersionIds: string[];
    }): Promise<{
      releaseId: string;
      catalogId: string;
      catalogVersion: string;
      signingKeyId: string;
      etag: string;
    }>;
  };
  /**
   * Operator-side administration of annotator credentials (docs/34 §5.2). Its
   * handlers sit on the governance surface, but the service writes through the
   * app pool — see annotator-admin.ts. Optional alongside appService: without
   * an annotator surface there are no credentials to administer.
   */
  annotatorAdmin?: AnnotatorAdminService;
  /**
   * Overrides the Cloudflare Access verifier used by cf_access login mode. In a
   * real deployment buildApp constructs one from `config.cfAccess`; tests inject
   * a fake so they need no live JWKS endpoint.
   */
  cfAccessVerifier?: CfAccessIdentityVerifier;
  /** Cloud Run supplies K_REVISION. Kept injectable so readiness tests can
   * prove the deployed-artifact identity contract without mutating process env. */
  deploymentRevision?: string | null;
  logger?: boolean;
};

declare module "fastify" {
  interface FastifyRequest {
    consoleSession?: AuthenticatedSession;
    annotatorId?: string;
    annotatorTokenHash?: string;
  }
}

// The desktop client is not a browser: it has no cookie jar and no origin, so
// these carry a bearer token instead. That also means the console's CSRF header
// and same-origin checks do not apply here — they defend a cookie-authenticated
// surface, and applying them to a native client would be theatre.
const appPublicPaths = new Set([
  "/app/v1/signup",
  "/app/v1/signup/verify",
  "/app/v1/signup/resend",
  "/app/v1/login",
  // A forgotten password is exactly the case where the caller cannot present a
  // session, so both reset steps have to sit outside the session hook.
  "/app/v1/password-reset/request",
  "/app/v1/password-reset/confirm",
  // A claim link is its own credential (docs/34 §5.2): the holder has no
  // session yet — redeeming the link is how they get an account to sign into.
  // The GET preflight is matched by prefix in the hook, this is the POST.
  "/app/v1/claim/confirm",
  // The browser sign-in. Public like /app/v1/login, but the hook still applies
  // the full CSRF discipline to everything under /app/v1/web/.
  "/app/v1/web/login",
  // Asked for before an account exists, and the same document for everyone.
  "/app/v1/terms",
  // Not unauthenticated — authenticated differently. Telemetry carries a device
  // credential, not an annotator session, so the session hook has to step aside
  // and let the route do its own check.
  "/app/v1/client-events",
]);

const publicPaths = new Set([
  "/healthz",
  "/readyz",
  // Asked for before anyone is signed in: the SPA cannot render the right sign-in
  // form without knowing the login mode. Still behind the proxy-token gate, so a
  // direct caller sees the same 404 as every other /console/v1 path.
  "/console/v1/meta",
  "/console/v1/auth/login",
  "/console/v1/auth/otp/verify",
  "/console/v1/auth/otp/resend",
  "/console/v1/auth/password-reset/request",
  "/console/v1/auth/password-reset/confirm",
  "/console/v1/auth/invite/accept",
  "/console/v1/auth/cf-access/login",
]);

export function buildApp(dependencies: ConsoleDependencies) {
  const { config, repository, browse, mailer, appService, annotatorAdmin } = dependencies;
  const deploymentRevision = dependencies.deploymentRevision ?? null;
  // The Access verifier exists only in cf_access mode; buildApp derives it from
  // config unless a test injected one. Its team domain and audience come from the
  // same config the mode validated at startup.
  const cfAccessVerifier =
    dependencies.cfAccessVerifier ??
    (config.cfAccess ? new CfAccessVerifier({ teamDomain: config.cfAccess.teamDomain, aud: config.cfAccess.aud }) : null);
  const auth = new AuthService(repository, mailer, config, cfAccessVerifier);
  const governance = new GovernanceService(dependencies.governance, repository, config);
  const invites = dependencies.invites
    ? new InviteAdminService(dependencies.invites, repository)
    : null;
  const preview = dependencies.preview;

  const app = Fastify({
    logger: dependencies.logger ?? false,
    bodyLimit: 64 * 1024,
    genReqId: () => `req_${randomUUID()}`,
    // A hop count, never `true`. `true` trusts the entire X-Forwarded-For chain
    // including whatever the caller prepended. `/app/v1` is public and may be
    // reached directly or through the production Worker; see
    // CONSOLE_TRUSTED_PROXY_HOPS.
    trustProxy: config.trustedProxyHops,
  });

  app.setErrorHandler((error, request, reply) => {
    if (error instanceof ZodError) {
      return reply.code(400).send({
        code: "invalid_request",
        message: "request did not match the console contract",
        requestId: request.id,
        retryable: false,
        details: {
          issues: error.issues.map((issue) => ({
            path: issue.path.join("."),
            message: issue.message,
          })),
        },
      });
    }
    if (error instanceof ConsoleError) {
      if (error.retryAfterSeconds !== null) {
        reply.header("retry-after", String(error.retryAfterSeconds));
      }
      return reply.code(error.statusCode).send({
        code: error.code,
        message: error.message,
        requestId: request.id,
        retryable: error.retryable,
      });
    }
    const databaseCode = (error as { code?: unknown }).code;
    if (databaseCode === "23505") {
      return reply.code(409).send({
        code: "resource_identity_conflict",
        message: "the resource identity is already in use",
        requestId: request.id,
        retryable: false,
      });
    }
    const transportStatus = (error as { statusCode?: unknown }).statusCode;
    if (typeof transportStatus === "number" && transportStatus >= 400 && transportStatus < 500) {
      return reply.code(transportStatus).send({
        code: transportStatus === 413 ? "request_too_large" : "invalid_request",
        message: "request could not be processed",
        requestId: request.id,
        retryable: false,
      });
    }
    request.log.error(
      { requestId: request.id, ...errorDiagnostics(error) },
      "unhandled console request error",
    );
    return reply.code(500).send({
      code: "internal_error",
      message: "request failed",
      requestId: request.id,
      retryable: true,
    });
  });

  app.addHook("onRequest", async (request, reply) => {
    if (request.url.startsWith("/app/")) {
      if (!appService) return;
      const path = request.url.split("?")[0] ?? request.url;

      // Two transports share this surface (docs/34 §6.1). The bearer token is
      // the desktop client's: no cookie jar, nothing a cross-site page can
      // ride, so no CSRF checks. The cookie is the browser's, and everything a
      // browser's ambient state enables is answered here: the /web/ sub-surface
      // and every cookie-authenticated write carry the console's full CSRF
      // discipline, and even the public shared endpoints reject a caller whose
      // own browser headers say it is another site.
      if (path.startsWith("/app/v1/web/")) {
        assertSameOriginWrite(request, config.publicOrigin, appCsrfHeaderName);
      }
      if (appPublicPaths.has(path) || path.startsWith("/app/v1/claim/")) {
        if (!path.startsWith("/app/v1/web/")) {
          assertBrowserSameOriginWrite(request, config.publicOrigin);
        }
        return;
      }

      const authorization = request.headers.authorization;
      if (authorization?.startsWith("Bearer ")) {
        const token = authorization.slice(7);
        if (token.length === 0) throw unauthorized("session_required", "a session is required");
        const session = await appService.authenticate(token);
        if (!session) throw unauthorized("session_required", "a session is required");
        request.annotatorId = session.annotatorId;
        request.annotatorTokenHash = hashSessionToken(token);
        return;
      }

      const token = readCookie(request.headers.cookie, appSessionCookieName);
      if (!token) throw unauthorized("session_required", "a session is required");
      assertSameOriginWrite(request, config.publicOrigin, appCsrfHeaderName);
      const session = await appService.authenticate(token);
      if (!session) throw unauthorized("session_required", "a session is required");
      request.annotatorId = session.annotatorId;
      request.annotatorTokenHash = hashSessionToken(token);
      // The server just slid the expiry (docs/34 §6.1a); the cookie's Max-Age
      // follows, so browser and server forget the session together.
      if (session.renewed) {
        setAppSessionCookie(reply, token, appService.sessionWindowSeconds);
      }
      return;
    }
    if (!request.url.startsWith("/console/")) return;

    // The Worker is the only sanctioned way in (docs/11 §4.3). Anything that
    // reached Cloud Run around it is answered as if the route did not exist —
    // 404, not 401, because a probe must not learn that a console lives here.
    // This runs before every other console check so the answer cannot vary with
    // what the caller sent: same status for a bad origin, a missing cookie, or a
    // login attempt. `/healthz` and `/readyz` never reach this branch, so Cloud
    // Run's own probes stay unauthenticated.
    if (config.proxyToken !== null) {
      const presented = request.headers["x-console-proxy-token"];
      if (typeof presented !== "string" || !secretEquals(presented, config.proxyToken)) {
        throw notFound("not_found", "the requested resource does not exist");
      }
    }

    assertSameOriginWrite(request, config.publicOrigin);

    const path = request.url.split("?")[0] ?? request.url;
    if (publicPaths.has(path)) return;

    const token = readCookie(request.headers.cookie, sessionCookieName);
    if (!token) throw unauthorized("session_required", "a console session is required");
    const session = await auth.authenticate(token);
    if (!session) throw unauthorized("session_required", "a console session is required");
    request.consoleSession = session;
  });

  app.addHook("onSend", async (request, reply, payload) => {
    reply.header("x-request-id", request.id);
    reply.header("cache-control", "no-store");
    reply.header("x-content-type-options", "nosniff");
    reply.header("referrer-policy", "no-referrer");
    return payload;
  });

  app.get("/healthz", async () => ({
    status: "ok",
    service: "dataminer-console-api",
    version: "0.1.0",
  }));

  app.get("/readyz", async (request, reply) => {
    const cloudSql = await readiness(() => repository.ping());
    if (cloudSql.status !== "ready") {
      return reply.code(503).send({
        code: "dependency_unavailable",
        message: "the console database is unavailable",
        requestId: request.id,
        retryable: true,
        details: { cloudSql },
      });
    }
    // Reported, never fatal. A deploy that skipped the grants step should be
    // loud here (deploy-console-production.sh phase 4 asserts on it) but must
    // not flip a serving revision to unhealthy: the routes that do not touch
    // these tables keep working, and a bug in the probe itself must not be able
    // to take the console down.
    const missingGrants = await repository.missingConsoleGrants().then(
      (missing) => ({ status: missing.length === 0 ? ("ok" as const) : ("incomplete" as const), missing }),
      () => ({ status: "unknown" as const, missing: [] as string[] }),
    );
    return {
      status: "ready",
      environment: config.environment,
      deployment: { revision: deploymentRevision },
      dependencies: { cloudSql },
      grants: missingGrants,
      features: {
        clipPreview: preview.available,
        annotatorApi: appService !== undefined,
        review: dependencies.review?.available ?? false,
        hfStatus: dependencies.hf?.available ?? false,
        acceptance: dependencies.acceptance?.available ?? false,
      },
    };
  });

  // ---------------------------------------------------------------------------
  // Annotator surface. Public reachability is the whole difference from
  // /console/v1: the desktop client talks to it directly rather than through the
  // Cloudflare Worker, which is why the proxy-token gate cannot be applied
  // service-wide (docs/11 §4.3).
  // ---------------------------------------------------------------------------
  if (appService) {
    const annotator = appService;

    app.post("/app/v1/signup", async (request, reply) => {
      requireJsonBody(request);
      const input = signupSchema.parse(request.body);
      const result = await annotator.signup(input, context(request, config));
      return reply.code(202).send(result);
    });

    app.post("/app/v1/signup/verify", async (request, reply) => {
      requireJsonBody(request);
      const input = signupVerifySchema.parse(request.body);
      await annotator.verifySignup(input, context(request, config));
      return reply.code(204).send();
    });

    // The way out of a verification mail that never arrived. Without it a failed
    // first delivery is terminal: the invite is spent, the account exists, and
    // signing up again just reports the address as taken.
    app.post("/app/v1/signup/resend", async (request, reply) => {
      requireJsonBody(request);
      const input = signupResendSchema.parse(request.body);
      const result = await annotator.resendSignupCode(input, context(request, config));
      return reply.code(202).send(result);
    });

    // The way back in for a forgotten password. Without it a lost annotator
    // password was terminal: there is no operator-side reset for annotator
    // credentials either, so the account — and the devices enrolled under it —
    // were simply gone.
    app.post("/app/v1/password-reset/request", async (request, reply) => {
      requireJsonBody(request);
      const input = appPasswordResetRequestSchema.parse(request.body);
      const result = await annotator.requestPasswordReset(input, context(request, config));
      return reply.code(202).send(result);
    });

    app.post("/app/v1/password-reset/confirm", async (request, reply) => {
      requireJsonBody(request);
      const input = appPasswordResetConfirmSchema.parse(request.body);
      await annotator.confirmPasswordReset(input, context(request, config));
      return reply.code(204).send();
    });

    app.post("/app/v1/login", async (request, reply) => {
      requireJsonBody(request);
      const input = appLoginSchema.parse(request.body);
      const grant = await annotator.login(input, context(request, config));
      return reply.code(200).send(grant);
    });

    app.post("/app/v1/logout", async (request, reply) => {
      const authorization = request.headers.authorization ?? "";
      await annotator.logout(authorization.slice(7));
      return reply.code(204).send();
    });

    // The browser transport (docs/34 §6.1): same verification as /app/v1/login,
    // but the session travels as an HttpOnly cookie and never through the
    // response body — the web pages hold no token, so script cannot leak one.
    app.post("/app/v1/web/login", async (request, reply) => {
      requireJsonBody(request);
      const input = appLoginSchema.parse(request.body);
      const grant = await annotator.login(input, context(request, config));
      setAppSessionCookie(reply, grant.token, grant.expiresInSeconds);
      return reply.code(200).send({ expiresInSeconds: grant.expiresInSeconds });
    });

    app.post("/app/v1/web/logout", async (request, reply) => {
      const token = readCookie(request.headers.cookie, appSessionCookieName) ?? "";
      await annotator.logout(token);
      clearAppSessionCookie(reply);
      return reply.code(204).send();
    });

    // 身份摘要而不再只有裸 id（docs/65 J-45）：个人中心登录后要能确认「我是谁」。
    // `annotatorId` 字段原样保留，桌面客户端的既有读法不受影响。
    app.get("/app/v1/session", async (request) =>
      annotator.sessionSummary(String(request.annotatorId)),
    );

    // Signed-in password change (docs/34 §6.3). 204 like the reset confirm; the
    // caller's own session keeps working, every other one is gone.
    app.post("/app/v1/password", async (request, reply) => {
      requireJsonBody(request);
      const input = changePasswordSchema.parse(request.body);
      await annotator.changePassword(
        String(request.annotatorId),
        String(request.annotatorTokenHash),
        input,
        context(request, config),
      );
      return reply.code(204).send();
    });

    app.get("/app/v1/profile", async (request) => ({
      profile: await annotator.getProfile(String(request.annotatorId)),
    }));

    // 条款状态与补签（docs/34）。此前 consent 只在注册那一刻写入且此后冻结：
    // 换了条款版本、或同一份文书对「是否授权训练用途」的部署解读变了，本人
    // 都没有任何自助路径——只能靠人手改库（那会连带重贴已采集数据的标签）或
    // 弃号重注册。
    app.get("/app/v1/consent", async (request) => annotator.getConsentState(String(request.annotatorId)));

    app.post("/app/v1/consent", async (request, reply) => {
      requireJsonBody(request);
      const input = consentRenewalSchema.parse(request.body);
      const result = await annotator.renewConsent(
        input,
        String(request.annotatorId),
        context(request, config),
      );
      return reply.code(result.renewed ? 201 : 200).send(result);
    });

    app.put("/app/v1/profile", async (request) => {
      requireJsonBody(request);
      const input = profileSchema.parse(request.body);
      return {
        profile: await annotator.updateProfile(
          String(request.annotatorId),
          input,
          context(request, config),
        ),
      };
    });

    // What the claim landing page renders before the user submits anything:
    // activation (terms + password) or reset (password only). Public — the
    // challenge id in the URL is the only thing the holder has yet.
    app.get("/app/v1/claim/:challengeId", async (request) => {
      const params = claimParamsSchema.parse(request.params);
      return annotator.claimPreflight(params.challengeId, context(request, config));
    });

    app.post("/app/v1/claim/confirm", async (request, reply) => {
      requireJsonBody(request);
      const input = claimConfirmSchema.parse(request.body);
      await annotator.confirmClaim(input, context(request, config));
      return reply.code(204).send();
    });

    app.post("/app/v1/devices/enroll", async (request, reply) => {
      requireJsonBody(request);
      const input = enrollDeviceSchema.parse(request.body);
      const result = await annotator.enrollDevice(
        String(request.annotatorId),
        input,
        context(request, config),
      );
      // 201 only when a credential was actually minted; a replay is a 200 with a
      // null token, so the client can tell the two apart without parsing prose.
      return reply.code(result.token ? 201 : 200).send(result);
    });

    // 绑定前的签名 catalog 启动（docs/37 §Bootstrap，issue #323）。
    //
    // 鉴权：**不在** `appPublicPaths` 里，所以上面那个 onRequest 钩子要求一个有效
    // 的标注员会话——这正是"严格限定的会话鉴权模式"。它对设备凭据那条路没有任何
    // 影响：`/v1/catalog` 的设备闸在另一个服务的另一个钩子里，本次改动没碰。
    //
    // 响应形状与 `/v1/catalog` 共用 `resolveCatalogDelivery`，两条路不可能漂移。
    //
    // 一处**有意**的不同：上面的 onSend 会把 `cache-control` 统一改写成 `no-store`，
    // 盖掉这里的 `private, no-cache`。不去豁免它——客户端的 ETag 存在自己的 SQLite
    // 里、`If-None-Match` 也是自己带的，从不依赖 HTTP 缓存，而 `no-store` 只会更严。
    app.get("/app/v1/catalog/bootstrap", async (request, reply) => {
      const channel = config.catalogBootstrapChannel;
      const releases = await annotator.bootstrapCatalog(String(request.annotatorId), channel);
      const delivery = resolveCatalogDelivery(releases, request.headers);
      for (const [name, value] of Object.entries(delivery.headers)) {
        reply.header(name, value);
      }
      switch (delivery.kind) {
        case "not_published":
          return reply.code(503).send({
            code: "catalog_not_published",
            message: `no ${channel} catalog release is currently published`,
            requestId: request.id,
            retryable: true,
          });
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

    // What the client must show and confirm before signup. Public: it is asked
    // for before any account exists, and it is the same document for everyone.
    app.get("/app/v1/terms", async () => annotator.terms());

    app.get("/app/v1/devices", async (request) => ({
      devices: await annotator.listDevices(String(request.annotatorId)),
    }));

    // —— 标注员自视图（docs/66 批次 C；docs/65 J-42/J-44/J-14）—————————————
    //
    // 下面三条的读数走 console 读池（browse），不是 app 池：grant-console-roles.sql
    // 对 dataminer_app 的拒授清单（sessions/clips/capture_grants）是设计本身，这里
    // 不动它——DB 层该角色依旧读不到采集记录，能读到的只有这几条服务端代码路径，
    // 而它们的查询主体永远取自会话本人。借池先例即 annotator-admin（运营侧的凭据
    // 写路径借 app 池，方向相反）。

    app.get("/app/v1/contribution", async (request) => {
      const query = contributionQuerySchema.parse(request.query);
      return browse.contributionForAnnotator(
        String(request.annotatorId),
        query.days,
        query.timeBasis,
        query.tz,
      );
    });

    app.get("/app/v1/consents", async (request) => ({
      consents: await annotator.listConsents(String(request.annotatorId)),
    }));

    // 「我能不能开始采集」的一站式自查（docs/65 J-14）：注册完成页说的前置在这里
    // 逐盏灯可见。聚合只读，不改变任何治理状态。
    //
    // 「运营签发授权」那盏灯随 certified 车道一起摘掉（docs/100）：录制不再需要
    // 逐次授权凭证，留着它只会是一盏**永远红的**灯——它会告诉每一位标注员他们
    // 还不能开始采集，而这是假的。能不能录由「游戏在不在已验签目录里」决定，
    // 那是目录侧的事实，不是标注员自查面能回答的。
    app.get("/app/v1/readiness", async (request) => {
      const annotatorId = String(request.annotatorId);
      const [summary, consents, devices] = await Promise.all([
        annotator.sessionSummary(annotatorId),
        annotator.listConsents(annotatorId),
        annotator.listDevices(annotatorId),
      ]);
      const activeConsent = consents.find(
        (row) => row.status === "active" && row.revokedAt === null,
      );
      const activeDevices = devices.filter((device) => device.status === "active");
      const seenRecently = devices.some(
        (device) =>
          device.lastSeenAt !== null &&
          Date.now() - Date.parse(device.lastSeenAt) < 7 * 24 * 3_600_000,
      );
      return {
        steps: [
          { key: "account", ok: summary.status === "active", detail: summary.status },
          {
            key: "consent",
            ok: activeConsent !== undefined,
            detail: activeConsent?.policyVersion ?? null,
          },
          { key: "device", ok: activeDevices.length > 0, detail: String(activeDevices.length) },
          { key: "client_seen", ok: seenRecently, detail: null },
        ],
      };
    });

    // The way back from a lost credential. Enrollment is idempotent per machine
    // and returns no token on replay, so without this a client whose credential
    // store was wiped could never upload again.
    app.post("/app/v1/devices/:deviceId/credential", async (request, reply) => {
      const params = deviceIdParamsSchema.parse(request.params);
      const rotated = await annotator.rotateDeviceCredential(
        String(request.annotatorId),
        params.deviceId,
        context(request, config),
      );
      return reply.code(201).send(rotated);
    });

    // Telemetry authenticates with the *device* credential, not a user session:
    // it runs unattended beside capture, long after any interactive sign-in has
    // expired. That is why it sits outside the session hook's reach.
    app.post("/app/v1/client-events", async (request, reply) => {
      requireJsonBody(request);
      const authorization = request.headers.authorization;
      const token = authorization?.startsWith("Bearer ") ? authorization.slice(7) : "";
      if (token.length === 0) {
        throw unauthorized("invalid_device_credential", "a device credential is required");
      }
      const body = clientEventBatchSchema.parse(request.body);
      const result = await annotator.recordClientEvents(
        createHash("sha256").update(token, "utf8").digest("hex"),
        body.events,
      );
      return reply.code(202).send(result);
    });
  }

  app.post("/console/v1/auth/login", async (request, reply) => {
    // In cf_access mode the console has no password step of its own: sign-in is
    // /auth/cf-access/login, driven by the Cloudflare Access assertion.
    if (config.loginMode === "cf_access") {
      throw notFound("password_login_disabled", "this console signs in through Cloudflare Access");
    }
    requireJsonBody(request);
    const input = loginSchema.parse(request.body);
    const ctx = context(request, config);
    // password_only issues a session on the spot (200 + cookie, same shape as an
    // OTP verify); password_otp returns a 202 challenge and defers the session to
    // /auth/otp/verify. The web client tells the two apart by the response body.
    if (config.loginMode === "password_only") {
      const grant = await auth.loginPasswordOnly(input.email, input.password, ctx);
      setSessionCookie(reply, grant.token, grant.maxAgeSeconds);
      return reply.code(200).send({ operator: publicOperator(grant.operator) });
    }
    const result = await auth.login(input.email, input.password, ctx);
    return reply.code(202).send(result);
  });

  app.post("/console/v1/auth/cf-access/login", async (request, reply) => {
    // Cloudflare Access authenticated the request at the edge and forwarded the
    // signed assertion in this header; the console verifies it and issues a
    // session. There is no request body — the assertion is the credential.
    if (config.loginMode !== "cf_access") {
      throw notFound("cf_access_login_disabled", "Cloudflare Access sign-in is not enabled");
    }
    const assertion = request.headers["cf-access-jwt-assertion"];
    const grant = await auth.loginWithCfAccess(
      typeof assertion === "string" ? assertion : "",
      context(request, config),
    );
    setSessionCookie(reply, grant.token, grant.maxAgeSeconds);
    return reply.code(200).send({ operator: publicOperator(grant.operator) });
  });

  app.post("/console/v1/auth/otp/verify", async (request, reply) => {
    requireJsonBody(request);
    const input = otpVerifySchema.parse(request.body);
    const grant = await auth.verifyOtp(input.challengeId, input.code, context(request, config));
    setSessionCookie(reply, grant.token, grant.maxAgeSeconds);
    return reply.code(200).send({ operator: publicOperator(grant.operator) });
  });

  app.post("/console/v1/auth/otp/resend", async (request, reply) => {
    requireJsonBody(request);
    const input = otpResendSchema.parse(request.body);
    const result = await auth.resendOtp(input.challengeId, context(request, config));
    return reply.code(202).send(result);
  });

  app.post("/console/v1/auth/password-reset/request", async (request, reply) => {
    requireJsonBody(request);
    const input = passwordResetRequestSchema.parse(request.body);
    await auth.requestPasswordReset(input.email, context(request, config));
    return reply.code(202).send({ status: "accepted" });
  });

  app.post("/console/v1/auth/password-reset/confirm", async (request, reply) => {
    requireJsonBody(request);
    const input = passwordResetConfirmSchema(config.password.minLength).parse(request.body);
    await auth.completeChallengePasswordSet(
      "password_reset",
      input.challengeId,
      input.token,
      input.password,
      context(request, config),
    );
    return reply.code(200).send({ status: "password_updated" });
  });

  app.post("/console/v1/auth/invite/accept", async (request, reply) => {
    requireJsonBody(request);
    const input = acceptInviteSchema(config.password.minLength).parse(request.body);
    await auth.completeChallengePasswordSet(
      "invite",
      input.challengeId,
      input.token,
      input.password,
      context(request, config),
    );
    return reply.code(200).send({ status: "activated" });
  });

  // What the sign-in screen needs before a session exists. Deliberately tiny and
  // free of anything an unauthenticated caller should not see: no operator data,
  // no counts, no configuration beyond which sign-in to render.
  app.get("/console/v1/meta", async () => ({
    environment: config.environment,
    // Without this the SPA always drew the password form, and under cf_access
    // that form is a dead end: /auth/login answers 404 password_login_disabled.
    loginMode: config.loginMode,
  }));

  app.get("/console/v1/auth/session", async (request) => {
    const session = requireSession(request);
    return {
      operator: publicOperator(session.operator),
      capabilities: capabilitiesFor(session.operator.role),
      session: {
        expiresAt: session.session.expiresAt,
        sudoUntil: session.session.sudoUntil,
      },
      // The step-up factor follows the login mode, and the UI has to know which
      // one to ask for: under cf_access there is no console password to prompt
      // for, and prompting anyway would be a dead end.
      stepUp: config.loginMode === "cf_access" ? "cf_access_assertion" : "password",
      // Which deployment the operator is actually looking at. The SPA used to
      // hardcode "staging" in its header, so the production console announced
      // itself as staging — an operator could act on real data believing it was
      // a test environment. `/readyz` already carries this, but the Worker only
      // proxies `/console/`, so the session is where the SPA can reach it.
      environment: config.environment,
    };
  });

  app.post("/console/v1/auth/logout", async (request, reply) => {
    const session = requireSession(request);
    await auth.logout(session, context(request, config));
    clearSessionCookie(reply);
    return reply.code(200).send({ status: "signed_out" });
  });

  app.post("/console/v1/auth/sudo", async (request, reply) => {
    requireJsonBody(request);
    const session = requireSession(request);
    const input = sudoSchema.parse(request.body);
    // Both factors are handed over and the service picks by login mode, so the
    // caller cannot choose the weaker one by sending the other field. Under
    // cf_access the assertion is the edge-injected header, exactly as it is for
    // sign-in — the browser has nothing to attach by hand.
    const assertion = request.headers["cf-access-jwt-assertion"];
    const result = await auth.grantSudo(
      session,
      {
        password: input.password,
        assertion: typeof assertion === "string" ? assertion : undefined,
      },
      context(request, config),
    );
    return reply.code(200).send(result);
  });

  app.get("/console/v1/annotators", async (request) => {
    const query = annotatorListQuerySchema.parse(request.query);
    return browse.listAnnotators(readable(request, query));
  });

  app.get("/console/v1/annotators/:id", async (request) => {
    const params = idParamsSchema.parse(request.params);
    return found(await browse.getAnnotator(readable(request, params).id), "annotator");
  });

  app.get("/console/v1/annotators/:id/contribution", async (request) => {
    const params = idParamsSchema.parse(request.params);
    const query = contributionQuerySchema.parse(request.query);
    return browse.contributionForAnnotator(
      readable(request, params).id,
      query.days,
      query.timeBasis,
      query.tz,
    );
  });

  app.get("/console/v1/devices", async (request) => {
    const query = deviceListQuerySchema.parse(request.query);
    return browse.listDevices(readable(request, query));
  });

  app.get("/console/v1/devices/:id", async (request) => {
    const params = idParamsSchema.parse(request.params);
    return found(await browse.getDevice(readable(request, params).id), "device");
  });

  app.get("/console/v1/devices/:id/contribution", async (request) => {
    const params = idParamsSchema.parse(request.params);
    const query = contributionQuerySchema.parse(request.query);
    return browse.contributionForDevice(
      readable(request, params).id,
      query.days,
      query.timeBasis,
      query.tz,
    );
  });

  app.get("/console/v1/sessions", async (request) => {
    const query = sessionListQuerySchema.parse(request.query);
    return browse.listSessions(readable(request, query));
  });

  app.get("/console/v1/sessions/:id", async (request) => {
    const params = idParamsSchema.parse(request.params);
    return found(await browse.getSession(readable(request, params).id), "session");
  });

  app.get("/console/v1/clips", async (request) => {
    const query = clipListQuerySchema.parse(request.query);
    return browse.listClips(readable(request, query));
  });

  app.get("/console/v1/clips/:id", async (request) => {
    const params = idParamsSchema.parse(request.params);
    return found(await browse.getClip(readable(request, params).id), "clip");
  });

  app.get("/console/v1/games", async (request) => {
    readable(request, null);
    return { games: await browse.listGames() };
  });

  // session 根审计快照（docs/47/67 §3.6）。只读一页目录：kind、对象键、大小、
  // 摘要与两个状态；字节访问仍只走审查面的逐次签发。
  app.get("/console/v1/sessions/:id/artifacts", async (request) => {
    const params = idParamsSchema.parse(request.params);
    return browse.listSessionArtifacts(readable(request, params).id);
  });

  // 会话验收报告（docs/136 §4）：从 R2 session 前缀读 accept.py 落的报告并
  // 原样透传。响应契约与既有错误信封刻意不同——404 `{error:"not_accepted"}`
  // 是业务态「验收还没跑到这个 session」，不是资源寻址失败；503 是「这个
  // 部署没配 ACCEPTANCE_R2_*」，两者绝不混同（伪装 404 会让运营把配置缺失
  // 误读成管线积压）。`code` 字段并排给出，供 SPA 的错误码映射复用。
  if (dependencies.acceptance) {
    const acceptance = dependencies.acceptance;
    app.get("/console/v1/sessions/:id/acceptance", async (request, reply) => {
      const params = idParamsSchema.parse(request.params);
      readable(request, null);
      if (!acceptance.available) {
        return reply.code(503).send({
          error: "acceptance_storage_unconfigured",
          code: "acceptance_storage_unconfigured",
          message:
            "acceptance report storage is not configured (ACCEPTANCE_R2_*), so acceptance state is unknown",
          requestId: request.id,
          retryable: false,
        });
      }
      const report = await acceptance.report(params.id);
      if (report === null) {
        return reply.code(404).send({ error: "not_accepted", code: "not_accepted" });
      }
      return report;
    });
  }

  // 白名单客户端遥测（docs/25）。已上云多时，此前没有任何展示入口。
  app.get("/console/v1/client-events", async (request) => {
    const query = clientEventQuerySchema.parse(request.query);
    return browse.listClientEvents(readable(request, query));
  });

  app.get("/console/v1/audit-events", async (request) => {
    const query = auditQuerySchema.parse(request.query);
    return browse.listAuditEvents(readable(request, query));
  });

  app.get("/console/v1/stats/overview", async (request) => {
    readable(request, null);
    return browse.pipelineOverview();
  });

  app.get("/console/v1/stats/pipeline", async (request) => {
    const query = contributionQuerySchema.parse(request.query);
    readable(request, null);
    return browse.pipelineHealth(query.days, query.tz);
  });

  // 全局每日进线（docs/97）。双时间口径同 docs/67 §3.3；天界时区参数化，
  // 缺省 Asia/Shanghai——「每天」按运营日报的天，不是 UTC 的天。
  app.get("/console/v1/stats/daily", async (request) => {
    const query = statsDailyQuerySchema.parse(request.query);
    readable(request, null);
    return browse.dailyIntake(query.days, query.timeBasis, query.tz, query.groupBy);
  });

  // 产出报表（docs/145）。显式区间、可叠两层分组、可对分组值筛选；口径与天界
  // 与上面那条一致，底部合计逐桶对得上。
  app.get("/console/v1/stats/output", async (request) => {
    const query = statsOutputQuerySchema.parse(request.query);
    readable(request, null);
    return browse.outputReport(query);
  });

  // 游戏分布普查（docs/153）。全量存量的单次快照，无参数——窗口化的按游戏拆分
  // 已有 /stats/daily?groupBy=game 与 /stats/output，这条只回答「语料押在哪几款
  // 游戏上」。已购删的段不计（存量口径，与进线的 flow 口径刻意分叉）。
  app.get("/console/v1/stats/game-distribution", async (request) => {
    readable(request, null);
    return browse.gameDistribution();
  });

  app.post("/console/v1/clips/:id/preview-url", async (request) => {
    const session = requireSession(request);
    requireCapability(session.operator.role, "clip.preview");
    const params = idParamsSchema.parse(request.params);
    const grant = await preview.signClipVideo(params.id);
    // Signing hands out a bearer URL for those bytes, so every issuance is a
    // recorded event with the object and its expiry, never the URL itself.
    await repository.recordAudit({
      actorType: "operator",
      actorId: session.operator.id,
      action: "clip.preview_signed",
      entityType: "clip",
      entityId: params.id,
      requestId: String(request.id),
      details: {
        objectKey: grant.objectKey,
        expiresAt: grant.expiresAt,
        operator: session.operator.email,
        ipHash: hashSubject(config.ipHashSalt, clientAddress(request, config.trustedProxyHops, config.proxyToken)),
      },
    });
    return grant;
  });

  // ---------------------------------------------------------------------------
  // Data-review surface (docs/33). Everything here is gated on review.read —
  // deliberately not console.read, because frames/events are per-event input
  // streams (§7.1). Tree navigation over allowlisted prefixes is key listing
  // only and is not audited (docs/09 §4.3 keys carry no user input); every
  // byte exit — video presign, frames, meta — writes an audit event.
  // ---------------------------------------------------------------------------
  if (dependencies.review) {
    const review = dependencies.review;

    const reviewable = (request: FastifyRequest): AuthenticatedSession => {
      const session = requireSession(request);
      requireCapability(session.operator.role, "review.read");
      return session;
    };

    const auditByteExit = async (
      request: FastifyRequest,
      session: AuthenticatedSession,
      action: string,
      source: string,
      objectKey: string,
      details: Record<string, unknown> = {},
    ) => {
      await repository.recordAudit({
        actorType: "operator",
        actorId: session.operator.id,
        action,
        entityType: "r2_object",
        entityId: objectKey,
        requestId: String(request.id),
        details: {
          ...details,
          source,
          objectKey,
          operator: session.operator.email,
          ipHash: hashSubject(
            config.ipHashSalt,
            clientAddress(request, config.trustedProxyHops, config.proxyToken),
          ),
        },
      });
    };

    app.get("/console/v1/review/sources", async (request) => {
      reviewable(request);
      return { sources: review.sources() };
    });

    app.get("/console/v1/review/tree", async (request) => {
      reviewable(request);
      const query = reviewTreeQuerySchema.parse(request.query);
      return review.tree(query.source, query.prefix, query.cursor);
    });

    app.get("/console/v1/review/object", async (request) => {
      reviewable(request);
      const query = reviewObjectQuerySchema.parse(request.query);
      return review.objectInfo(query.source, query.key);
    });

    app.post("/console/v1/review/video-url", async (request) => {
      requireJsonBody(request);
      const session = reviewable(request);
      const input = reviewSignVideoSchema.parse(request.body);
      const grant = await review.signVideo(input.source, input.key);
      // Same discipline as clip.preview_signed: record the object and expiry,
      // never the URL itself.
      await auditByteExit(request, session, "review.video_signed", grant.source, grant.objectKey, {
        expiresAt: grant.expiresAt,
      });
      return grant;
    });

    app.post("/console/v1/review/viz-url", async (request) => {
      requireJsonBody(request);
      const session = reviewable(request);
      const input = reviewSignVizSchema.parse(request.body);
      const grant = await review.signVizAsset(input.source, input.key);
      // Presigned like review.video_signed; the URL itself never reaches the
      // audit trail (docs/104 §3).
      await auditByteExit(request, session, "review.viz_signed", grant.source, grant.objectKey, {
        expiresAt: grant.expiresAt,
      });
      return grant;
    });

    app.get("/console/v1/review/frames", async (request) => {
      const session = reviewable(request);
      const query = reviewObjectQuerySchema.parse(request.query);
      const result = await review.frames(query.source, query.key);
      await auditByteExit(request, session, "review.frames_read", result.source, query.key, {
        schema: result.schema,
        sha256: result.provenance.sha256,
      });
      return result;
    });

    app.get("/console/v1/review/meta", async (request) => {
      const session = reviewable(request);
      const query = reviewObjectQuerySchema.parse(request.query);
      const result = await review.meta(query.source, query.key);
      await auditByteExit(request, session, "review.meta_read", result.source, query.key);
      return result;
    });

    // The depth surface (docs/77). Unlike every other review route this one
    // answers with bytes rather than JSON: the viewer needs the f32 frame as an
    // ArrayBuffer, and base64 through a JSON envelope would inflate a 3.5 MiB
    // frame by a third for nothing.
    app.get("/console/v1/review/depth-object", async (request, reply) => {
      const session = reviewable(request);
      const query = reviewObjectQuerySchema.parse(request.query);
      const result = await review.depthObject(query.source, query.key);
      await auditByteExit(request, session, "review.depth_read", result.source, query.key, {
        sizeBytes: result.bytes.byteLength,
      });
      return reply
        .code(200)
        .header("content-type", result.contentType)
        .header("cache-control", "private, max-age=300")
        .send(result.bytes);
    });
  }

  // Sub-resources rather than the ingest API's colon verbs: the router cannot
  // tell two custom verbs apart under the same parameter, and /clips/:id:qa
  // would silently collide with /clips/:id:purge.
  app.get("/console/v1/catalog-versions", async (request) => {
    const query = catalogQuerySchema.parse(request.query);
    const session = requireSession(request);
    return { versions: await governance.listCatalogVersions(session, query.kind, query.gameId) };
  });

  app.get("/console/v1/catalog-releases", async (request) => {
    const query = catalogReleaseQuerySchema.parse(request.query);
    const session = requireSession(request);
    return { releases: await governance.listCatalogReleases(session, query.channel) };
  });

  if (dependencies.catalogPublisher) {
    app.post("/console/v1/catalog-releases", async (request, reply) => {
      const { session } = write(request, config);
      requireCapability(session.operator.role, "governance.write");
      if (!session.session.sudoActive) {
        throw forbidden(
          "sudo_required",
          `re-enter your password to confirm this action; it stays confirmed for ${config.session.sudoMinutes} minutes`,
        );
      }
      const input = publishCatalogReleaseSchema.parse(request.body);
      const release = await dependencies.catalogPublisher!.publish({
        ...input,
        actor: session.operator.email,
      });
      return reply.code(201).send({ release });
    });
  }

  app.post("/console/v1/annotators", async (request, reply) => {
    const { session, key, context: ctx } = write(request, config);
    const input = createAnnotatorSchema.parse(request.body);
    const annotator = await governance.createAnnotator(session, key, input, ctx);
    return reply.code(201).send({ annotator });
  });

  // Operator-side registration and credential administration (docs/34 §5.2 /
  // §6.5). Handlers sit here on the governance surface — proxy gate, operator
  // session, RBAC, sudo — while every credential write inside the service runs
  // on the app pool and role (docs/34 §7.8). The claimUrl in each 201 appears
  // exactly once, like a device token.
  if (annotatorAdmin) {
    const admin = annotatorAdmin;

    app.post("/console/v1/annotators/registration", async (request, reply) => {
      requireJsonBody(request);
      const session = requireSession(request);
      const input = annotatorRegistrationSchema.parse(request.body);
      const issued = await admin.register(session, input, context(request, config));
      return reply.code(201).send(issued);
    });

    app.post("/console/v1/annotators/:id/credential-invite", async (request, reply) => {
      requireJsonBody(request);
      const session = requireSession(request);
      const params = idParamsSchema.parse(request.params);
      const input = annotatorCredentialInviteSchema.parse(request.body);
      const issued = await admin.inviteCredential(
        session,
        params.id,
        input.email,
        context(request, config),
      );
      return reply.code(201).send(issued);
    });

    app.post("/console/v1/annotators/:id/password-reset", async (request, reply) => {
      requireJsonBody(request);
      const session = requireSession(request);
      const params = idParamsSchema.parse(request.params);
      const issued = await admin.triggerPasswordReset(session, params.id, context(request, config));
      return reply.code(201).send(issued);
    });

    app.put("/console/v1/annotators/:id/profile", async (request) => {
      requireJsonBody(request);
      const session = requireSession(request);
      const params = idParamsSchema.parse(request.params);
      const input = adminProfileUpdateSchema.parse(request.body);
      return {
        profile: await admin.updateProfile(
          session,
          params.id,
          input.profile,
          context(request, config),
        ),
      };
    });
  }

  // Invite administration. The plaintext codes come back exactly once, in the
  // create response; every later read shows the code (it is not a secret — see
  // 11 §12.2) but the point of issuing is this one response.
  if (invites) {
    const inviteAdmin = invites;

    app.get("/console/v1/invites", async (request) => {
      const session = requireSession(request);
      const query = inviteListQuerySchema.parse(request.query);
      return { invites: await inviteAdmin.list(session, query.status ? { status: query.status } : {}) };
    });

    app.post("/console/v1/invites", async (request, reply) => {
      requireJsonBody(request);
      const session = requireSession(request);
      const input = createInviteSchema.parse(request.body);
      const created = await inviteAdmin.create(
        session,
        { count: input.count, note: input.note ?? null, expiresAt: input.expiresAt ?? null },
        context(request, config),
      );
      return reply.code(201).send(created);
    });

    app.post("/console/v1/invites/:code/revocation", async (request) => {
      const session = requireSession(request);
      const params = inviteCodeParamsSchema.parse(request.params);
      return { invite: await inviteAdmin.revoke(session, params.code, context(request, config)) };
    });

    // "The admin is also an annotator." An operator who wants to contribute
    // capture data registers on the client like anyone else — this mints them
    // the code to do it with, tagged so the console can later show which
    // annotator they are. It authorizes only the caller, at the lowest
    // privilege in the system, so it is not gated like the writes above.
    app.post("/console/v1/operators/me/annotator-invite", async (request, reply) => {
      const session = requireSession(request);
      const issued = await inviteAdmin.issueSelfInvite(session, context(request, config));
      return reply.code(201).send(issued);
    });
  }

  app.patch("/console/v1/annotators/:id", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = updateAnnotatorSchema.parse(request.body);
    return { annotator: await governance.updateAnnotator(session, key, params.id, input, ctx) };
  });

  app.post("/console/v1/consents", async (request, reply) => {
    const { session, key, context: ctx } = write(request, config);
    const input = createConsentSchema.parse(request.body);
    const consent = await governance.createConsent(session, key, input, ctx);
    return reply.code(201).send({ consent });
  });

  app.post("/console/v1/consents/:id/revocation", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = revokeSchema.parse(request.body);
    return { consent: await governance.revokeConsent(session, key, params.id, input.reason, ctx) };
  });

  app.post("/console/v1/devices", async (request, reply) => {
    const { session, key, context: ctx } = write(request, config);
    const input = createDeviceSchema.parse(request.body);
    const issued = await governance.createDevice(session, key, input, ctx);
    return reply.code(201).send(issued);
  });

  app.patch("/console/v1/devices/:id", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = updateDeviceSchema.parse(request.body);
    return { device: await governance.updateDevice(session, key, params.id, input, ctx) };
  });

  app.post("/console/v1/devices/:id/credential", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    return governance.rotateDeviceCredential(session, key, params.id, ctx);
  });


  app.post("/console/v1/clips/:id/quality", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = clipQualitySchema.parse(request.body);
    return governance.recordClipQuality(session, key, params.id, input, ctx);
  });

  // 判定的默认粒度（issue #855 PR 5）。人审的是一场录制，不是一条 10 秒片段；
  // 上面那条逐 clip 入口保留下来做例外修正，不做日常入口。
  app.post("/console/v1/sessions/:id/quality", async (request) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = sessionQualitySchema.parse(request.body);
    return governance.recordSessionQuality(session, key, params.id, input, ctx);
  });

  // ---------------------------------------------------------------------------
  // HF release reconciliation (docs/33 §6). Reads gate on review.read; the
  // reconcile and mark writes gate on governance.write. The reconcile target
  // is allowlist-checked in the service, so the token is never steered at an
  // arbitrary repo.
  // ---------------------------------------------------------------------------
  if (dependencies.hf) {
    const hf = dependencies.hf;

    app.get("/console/v1/hf/repos", async (request) => {
      const session = requireSession(request);
      requireCapability(session.operator.role, "review.read");
      return { repos: hf.repos() };
    });

    app.get("/console/v1/hf/status", async (request) => {
      const session = requireSession(request);
      requireCapability(session.operator.role, "review.read");
      const query = hfStatusQuerySchema.parse(request.query);
      return hf.status(query.source, query.subjectKey, query.videoSizeBytes ?? null);
    });

    app.post("/console/v1/hf/reconcile", async (request) => {
      requireJsonBody(request);
      const session = requireSession(request);
      requireCapability(session.operator.role, "governance.write");
      const input = hfReconcileSchema.parse(request.body);
      const result = await hf.reconcile(input.repoId, session.operator.id);
      await repository.recordAudit({
        actorType: "operator",
        actorId: session.operator.id,
        action: "hf.reconciled",
        entityType: "hf_repo",
        entityId: input.repoId,
        requestId: String(request.id),
        details: {
          revision: result.revision,
          itemCount: result.itemCount,
          matchableCount: result.matchableCount,
          operator: session.operator.email,
        },
      });
      return result;
    });

    app.post("/console/v1/hf/marks", async (request, reply) => {
      requireJsonBody(request);
      const session = requireSession(request);
      requireCapability(session.operator.role, "governance.write");
      const input = hfMarkSchema.parse(request.body);
      const mark = await hf.recordMark(
        input.source,
        input.subjectKey,
        input.mark,
        input.note ?? null,
        session.operator.id,
      );
      await repository.recordAudit({
        actorType: "operator",
        actorId: session.operator.id,
        action: "hf.marked",
        entityType: "r2_object",
        entityId: input.subjectKey,
        requestId: String(request.id),
        details: { source: input.source, mark: input.mark, note: input.note ?? null, operator: session.operator.email },
      });
      return reply.code(201).send({ mark });
    });
  }

  // Review verdicts (docs/33 §5) live on governance, not on the review byte
  // path: they are pure database writes and stay available even when the
  // review credentials are absent (the ledger outlives any one surface).
  app.post("/console/v1/review/verdicts", async (request, reply) => {
    const { session, key, context: ctx } = write(request, config);
    const input = reviewVerdictSchema.parse(request.body);
    const verdict = await governance.recordReviewVerdict(session, key, input, ctx);
    return reply.code(201).send({ verdict });
  });

  app.get("/console/v1/review/verdicts", async (request) => {
    const session = requireSession(request);
    requireCapability(session.operator.role, "review.read");
    const query = reviewVerdictQuerySchema.parse(request.query);
    return { verdicts: await governance.listReviewVerdicts(session, query.source, query.subjectKey) };
  });

  app.post("/console/v1/clips/:id/purge", async (request, reply) => {
    const { session, key, context: ctx } = write(request, config);
    const params = idParamsSchema.parse(request.params);
    const input = clipPurgeSchema.parse(request.body);
    const outcome = await governance.requestClipPurge(session, key, params.id, input.reason, ctx);
    return reply.code(202).send(outcome);
  });

  app.get("/console/v1/operators", async (request) => {
    const session = requireSession(request);
    const operators = await auth.listOperators(session);
    // Which of them also contribute capture data. Derived from the self-issued
    // invites rather than stored on the operator row, so the annotator surface
    // never needs write access to `operators` (11 §4.2).
    const links = invites ? await invites.annotatorLinks(session) : new Map();
    return {
      operators: operators.map((operator) => ({
        ...publicOperator(operator),
        annotator: links.get(operator.id) ?? null,
      })),
    };
  });

  app.post("/console/v1/operators", async (request, reply) => {
    requireJsonBody(request);
    const session = requireSession(request);
    const input = createOperatorSchema.parse(request.body);
    const operator = await auth.inviteOperator(
      session,
      input.email,
      input.role,
      context(request, config),
    );
    return reply.code(201).send({ operator: publicOperator(operator) });
  });

  app.patch("/console/v1/operators/:operatorId", async (request) => {
    requireJsonBody(request);
    const session = requireSession(request);
    const params = operatorIdParamsSchema.parse(request.params);
    const input = updateOperatorSchema.parse(request.body);
    const operator = await auth.updateOperator(
      session,
      params.operatorId,
      input,
      context(request, config),
    );
    return { operator: publicOperator(operator) };
  });

  return app;
}

function requireSession(request: FastifyRequest): AuthenticatedSession {
  const session = request.consoleSession;
  if (!session) throw unauthorized("session_required", "a console session is required");
  return session;
}

// Every browsing route runs the capability check on the server and then hands the
// validated query straight through, so a route can never read without one.
function readable<T>(request: FastifyRequest, query: T): T {
  requireCapability(requireSession(request).operator.role, "console.read");
  return query;
}

// Every governance write needs the same three things before it runs: a session,
// an idempotency key, and the request context that lands in the audit trail.
function write(request: FastifyRequest, config: ConsoleConfig) {
  requireJsonBody(request);
  const session = requireSession(request);
  const header = request.headers["idempotency-key"];
  const key = idempotencyKeySchema.parse(typeof header === "string" ? header : "");
  return { session, key, context: context(request, config) };
}

function found<T>(value: T | null, entity: string): T {
  if (!value) throw notFound(`${entity}_not_found`, `the ${entity} was not found`);
  return value;
}

function context(request: FastifyRequest, config: ConsoleConfig): RequestContext {
  return {
    requestId: String(request.id),
    ipHash: hashSubject(config.ipHashSalt, clientAddress(request, config.trustedProxyHops, config.proxyToken)),
    userAgent: truncatedUserAgent(request),
  };
}

// The wire shape never carries the password hash, the lock state, or anything
// else that only the server needs.
function publicOperator(operator: OperatorRecord) {
  return {
    id: operator.id,
    email: operator.email,
    role: operator.role,
    status: operator.status,
    lastLoginAt: operator.lastLoginAt,
    createdAt: operator.createdAt,
  };
}

async function readiness(probe: () => Promise<void>) {
  try {
    await probe();
    return { status: "ready" as const };
  } catch {
    return { status: "unavailable" as const };
  }
}

/**
 * Diagnostic fields for an otherwise unhandled 500, chosen so the failure can be
 * identified from the log alone without putting row values in it.
 *
 * The previous shape logged only `errorName`, which for any node-postgres
 * failure is the string `"error"`. On 2026-07-28 that made a missing GRANT
 * indistinguishable from any other fault: two console routes 500'd and the only
 * evidence was `{"errorName":"error","msg":"unhandled console request error"}`,
 * so the cause had to be reconstructed by diffing which routes still worked.
 *
 * The split is deliberate. `message`, SQLSTATE and the identifier fields name
 * the *class* of failure ("permission denied for table review_verdicts",
 * `42501`) and carry no user data. The postgres fields that do embed row values
 * — `detail` ("Key (email)=(…) already exists"), `where`, `internalQuery` — are
 * omitted on purpose. The HTTP response is unchanged and still says only
 * "request failed"; this detail exists in the access-controlled log, not on the
 * wire.
 */
export function errorDiagnostics(error: unknown): Record<string, unknown> {
  if (!(error instanceof Error)) return { errorName: "UnknownError" };
  const pg = error as {
    code?: unknown;
    table?: unknown;
    schema?: unknown;
    constraint?: unknown;
    routine?: unknown;
  };
  const stringField = (value: unknown): string | undefined =>
    typeof value === "string" && value.length > 0 ? value : undefined;
  return {
    errorName: error.name,
    errorMessage: error.message,
    pgCode: stringField(pg.code),
    pgTable: stringField(pg.table),
    pgSchema: stringField(pg.schema),
    pgConstraint: stringField(pg.constraint),
    pgRoutine: stringField(pg.routine),
    stack: error.stack,
  };
}

export type ConsoleApp = ReturnType<typeof buildApp>;
export type { FastifyReply };

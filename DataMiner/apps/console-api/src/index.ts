import { AnnotatorAdminService } from "./annotator-admin.js";
import { PostgresAppRepository } from "./app-repository.js";
import { publishCatalogRelease } from "@dataminer/api/catalog-publisher";
import { type CatalogQuery } from "@dataminer/api/signed-catalog";
import { AppService } from "./app-service.js";
import { InviteAdminRepository } from "./invite-admin.js";
import { buildApp } from "./app.js";
import { PostgresBrowseRepository } from "./browse-repository.js";
import { PostgresGovernanceRepository } from "./governance-repository.js";
import { PreviewService, R2PreviewSigner } from "./preview.js";
import { HfService, HttpHfClient } from "./hf.js";
import { AcceptanceService, R2AcceptanceStorage } from "./acceptance.js";
import { R2ReviewStorage, ReviewService } from "./review.js";
import { loadConfig } from "./config.js";
import { buildMailer } from "./mailer.js";
import {
  CloudflareD1CredentialSink,
  drainMirrorCredentialOutbox,
  UnconfiguredCredentialSink,
} from "./mirror-publisher.js";
import { PostgresConsoleRepository } from "./repository.js";
import { createPool } from "./sql.js";

const config = loadConfig();
const pool = createPool(config.databaseUrl);
const repository = new PostgresConsoleRepository(pool, () => pool.end());
const browse = new PostgresBrowseRepository(pool);
const governance = new PostgresGovernanceRepository(pool);
const invites = new InviteAdminRepository(pool);
const previewSigner = config.preview ? new R2PreviewSigner(config.preview) : null;
const preview = new PreviewService(pool, previewSigner, config);
const reviewStorage = config.review ? new R2ReviewStorage(config.review) : null;
const review = new ReviewService(pool, reviewStorage, config);
const hfClient = config.hf ? new HttpHfClient(config.hf.token) : null;
const hf = new HfService(pool, hfClient, config);
// 未配置也构造并注入（storage=null）：验收路由必须存在并答 503「存储未配置」，
// 不注册路由会让它看起来像 404「未验收」（docs/136 §4）。
const acceptance = new AcceptanceService(
  config.acceptance ? new R2AcceptanceStorage(config.acceptance) : null,
  config.acceptance?.bucket ?? "diogenes-model-training",
);
const mailer = buildMailer(config);
const catalogPublishPool = config.catalogPublishDatabaseUrl
  ? createPool(config.catalogPublishDatabaseUrl)
  : null;
// 与 `dist/publish-catalog.js`（那条 Cloud Run job 的 CLI）是同一个发布函数的
// 两个入口。此前两个入口各自抄了一份 certified 的 KMS 签名分支；两份都随
// issue #829 删除（docs/100 §11），签名器只剩编译内嵌的 dev 种子钥匙。
const catalogPublisher = catalogPublishPool ? {
  async publish(input: {
    channel: "dev";
    catalogId: string;
    catalogVersion: string;
    actor: string;
    trustedCatalogVersionIds: string[];
  }) {
    const client = await catalogPublishPool.connect();
    try {
      const query: CatalogQuery = async (text, params) => {
        const result = await client.query(text, params);
        return { rows: result.rows as Array<Record<string, unknown>> };
      };
      return publishCatalogRelease(query, input);
    } finally {
      client.release();
    }
  },
} : undefined;

// The annotator surface, when configured, gets its OWN pool on its OWN
// connection string — that is where the dataminer_app_staging role lives
// (docs/11 §4.2). Reusing `pool` here would keep the code paths apart but leave
// them one database role, which is exactly the isolation this is supposed to
// provide. A pooled client's `query` runs each statement on some connection
// from the pool, so signup's transaction is bracketed with an explicit
// BEGIN/COMMIT on one checked-out client rather than the shared pool.
const appPool = config.app ? createPool(config.app.databaseUrl) : null;
const appRepository =
  config.app && appPool
    ? new PostgresAppRepository(appPool, async (run) => {
        const client = await appPool.connect();
        try {
          await client.query("BEGIN");
          const result = await run(client);
          await client.query("COMMIT");
          return result;
        } catch (error) {
          await client.query("ROLLBACK");
          throw error;
        } finally {
          client.release();
        }
      })
    : null;
const appService =
  config.app && appRepository
    ? new AppService(
        appRepository,
        mailer,
        config,
        config.app.terms,
        // Delivery failures land in the process log as well as the audit trail.
        // A dead mail provider and a struggling database are the same incident
        // often enough that the durable record can be missing exactly when it
        // is needed; this is the signal to alert on.
        (fields, message) => {
          const { severity = "ERROR", ...diagnostics } = fields;
          console.error(JSON.stringify({ severity, message, ...diagnostics }));
        },
      )
    : undefined;
// The operator-side credential administration (docs/34 §5.2). Handlers live on
// the governance surface; every credential write goes through the same app
// repository — and therefore the same dataminer_app role — as self-service.
const annotatorAdmin = appRepository
  ? new AnnotatorAdminService(appRepository, repository, mailer, config)
  : undefined;

const app = buildApp({
  config,
  repository,
  browse,
  governance,
  invites,
  preview,
  review,
  hf,
  acceptance,
  mailer,
  ...(catalogPublisher ? { catalogPublisher } : {}),
  ...(appService ? { appService } : {}),
  ...(annotatorAdmin ? { annotatorAdmin } : {}),
  deploymentRevision: process.env.K_REVISION ?? null,
  logger: true,
});

/*
 * D1 观测镜像的 outbox drain。
 *
 * 跑在 API 进程里而不是单独的 worker：这条队列每天只有个位数到几十行
 * （凭据签发/轮换/停用），为它拉一个进程不划算。它不在任何请求路径上，
 * 失败只体现为积压——积压有指标可看，因此宁可让它长出来，也不静默丢弃。
 */
const mirrorSink = config.mirror
  ? new CloudflareD1CredentialSink(config.mirror)
  : new UnconfiguredCredentialSink();
const drainIntervalMs = (config.mirror?.drainSeconds ?? 30) * 1_000;
const drainBatch = config.mirror?.drainBatch ?? 100;

let draining = false;
const drainTimer = setInterval(() => {
  // 上一轮没跑完就跳过：慢 D1 不该让 drain 叠罗汉。
  if (draining) return;
  draining = true;
  void drainMirrorCredentialOutbox(pool, mirrorSink, drainBatch)
    .then((result) => {
      if (result.failed > 0) {
        app.log.warn({ failed: result.failed }, "mirror credential publish deferred");
      }
    })
    .catch((error: unknown) => {
      app.log.error({ err: error }, "mirror credential drain failed");
    })
    .finally(() => {
      draining = false;
    });
}, drainIntervalMs);
// 不阻止进程退出：这是后台维护，不是必须完成的工作。
drainTimer.unref();

if (!config.mirror) {
  app.log.warn(
    "CONSOLE_MIRROR_D1_* is unset: device credentials will queue in mirror_credential_outbox and the D1 mirror will reject every device",
  );
}

async function shutdown(signal: string) {
  app.log.info({ signal }, "shutting down");
  clearInterval(drainTimer);
  await app.close();
  await repository.close();
  await appPool?.end();
  await catalogPublishPool?.end();
  process.exit(0);
}

process.once("SIGINT", () => void shutdown("SIGINT"));
process.once("SIGTERM", () => void shutdown("SIGTERM"));

await app.listen({ host: config.host, port: config.port });

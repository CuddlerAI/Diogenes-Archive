/**
 * 手动演练：把 `client_event.recorded` outbox 排空到 GCS（docs/23 §6）。
 *
 *   DATABASE_URL=postgres://... LOGS_GCS_BUCKET=dataminer-logs DATAMINER_ENV=staging \
 *     pnpm --filter @dataminer/api exec tsx scripts/drill-client-events-export.ts [--dry-run]
 *
 * 与 worker 跑的是同一套模块函数；--dry-run 只认领后放回并报积压深度，
 * 是验收「导出器 dry-run 可跑并暴露积压」的入口。GCS 认证走 ADC。
 */
import pg from "pg";

import {
  clientEventExportBacklog,
  drainClientEventExport,
  GcsClientEventSink,
} from "../src/client-events-export.js";
import { environments, type Environment } from "../src/environment.js";

const databaseUrl = process.env.DATABASE_URL;
const bucket = process.env.LOGS_GCS_BUCKET;
const environment = process.env.DATAMINER_ENV as Environment;
if (!databaseUrl || !bucket || !environments.includes(environment)) {
  console.error("required: DATABASE_URL, LOGS_GCS_BUCKET, DATAMINER_ENV=staging|production");
  process.exit(2);
}

const dryRun = process.argv.includes("--dry-run");
const pool = new pg.Pool({ connectionString: databaseUrl, max: 2 });

console.log("backlog before:", await clientEventExportBacklog(pool));
const result = await drainClientEventExport(pool, new GcsClientEventSink({ bucket }), {
  environment,
  batchSize: 200,
  dryRun,
});
console.log(dryRun ? "dry-run result:" : "drain result:", result);
console.log("backlog after:", await clientEventExportBacklog(pool));
await pool.end();

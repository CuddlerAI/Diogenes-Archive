/**
 * Publish one signed catalog release (docs/36).
 *
 * Lives in `src/` rather than `scripts/` so the build compiles it to
 * `dist/publish-catalog.js`, the same way `migrate.ts` becomes
 * `dist/migrate.js`. That is what lets a Cloud Run job run it with plain
 * `node`.
 *
 * 这里曾经有一条 certified 分支：它按 `request.channel` 去 Cloud KMS 取那把
 * 非导出私钥来签。certified 发布侧随 issue #829 收口（docs/100 §11）——那条
 * 车道发出来的目录没有任何客户端装得上（客户端只钉 `DEV_CATALOG_ANCHOR`
 * 一把公钥），签名器与 `DATAMINER_CERTIFIED_KMS_KEY` 因此一并删除。roster 里
 * 显式写 certified 会在 `publishCatalogRelease` 的入口守卫上被点名拒绝。
 */
import { readFile } from "node:fs/promises";
import path from "node:path";

import { Client } from "pg";

import {
  publishCatalogRelease,
  rosterPathFromArgv,
  type CatalogPublishRequest,
} from "./catalog-publisher.js";
import { type CatalogQuery } from "./signed-catalog.js";

const databaseUrl = process.env.DATABASE_URL;
if (!databaseUrl) throw new Error("DATABASE_URL is required");

// A Cloud Run job has no filesystem to stage a roster onto, so the roster may
// arrive as an environment variable instead of a path. The file form stays the
// default for local use; neither is a secret (the roster is a list of catalog
// version uuids).
const inlineRoster = process.env.CATALOG_ROSTER_JSON;
const request = JSON.parse(
  inlineRoster ?? (await readFile(path.resolve(rosterPathFromArgv(process.argv)), "utf8")),
) as CatalogPublishRequest;

const client = new Client({ connectionString: databaseUrl });
await client.connect();
try {
  const query: CatalogQuery = async (text, params) => {
    const result = await client.query(text, params);
    return { rows: result.rows as Array<Record<string, unknown>> };
  };
  const result = await publishCatalogRelease(query, request);
  process.stdout.write(`${JSON.stringify(result)}\n`);
} finally {
  await client.end();
}

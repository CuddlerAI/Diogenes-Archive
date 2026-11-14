import path from "node:path";

import { Client } from "pg";

import { readCatalogBundle, registerCatalogBundle } from "./register-catalog.js";

if (process.env.DATAMINER_ENV !== "production") {
  throw new Error("DATAMINER_ENV must be exactly production");
}
const databaseUrl = process.env.DATABASE_URL;
if (!databaseUrl) throw new Error("DATABASE_URL is required");

const bundleName = process.env.CATALOG_BUNDLE_NAME;
if (!bundleName || !/^[a-z0-9][a-z0-9-]{0,119}$/.test(bundleName)) {
  throw new Error("CATALOG_BUNDLE_NAME must be a simple lowercase bundle directory name");
}
const approver = process.env.CATALOG_APPROVER?.trim();
if (!approver || approver.length > 320) throw new Error("CATALOG_APPROVER is required");

const dryRun = process.env.CATALOG_DRY_RUN === "true";
if (!dryRun && process.env.CATALOG_DRY_RUN !== "false") {
  throw new Error("CATALOG_DRY_RUN must be exactly true or false");
}
if (!dryRun && process.env.CONFIRM_CATALOG_REGISTRATION !== "register-production-dev-catalog") {
  throw new Error(
    "CONFIRM_CATALOG_REGISTRATION must be exactly register-production-dev-catalog",
  );
}

const bundle = await readCatalogBundle(path.resolve("/app/catalogs/dev", bundleName));
// 双读（issue #855 PR 1）：legacy 一代必须自称 `non_trainable: true`；中性一代
// （`declaredNonTrainable === null`）不再声明训练分类，本注册入口照收。
//
// 放行中性一代**不放宽任何审批**：下面的 options 仍然是 approveTrainable=false /
// gameTrainable=false，两个训练审批位继续 fail-closed，可训练性仍只由
// `clip_trainability` 派生（issue #855 §C）。被明确拒绝的只剩一种：一份 legacy
// bundle 自称 `non_trainable: false`——它没有任何下发渠道收得下（docs/100）。
if (bundle.declaredNonTrainable === false) {
  throw new Error(
    "this job refuses a legacy catalog bundle that declares non_trainable: false; no delivery channel accepts it (docs/100)",
  );
}

const client = new Client({ connectionString: databaseUrl });
await client.connect();
try {
  await client.query("BEGIN");
  const report = await registerCatalogBundle(
    (text, params) => client.query(text, params as unknown[]),
    bundle,
    {
      approve: true,
      approver,
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: false,
      dryRun,
    },
  );
  if (dryRun) {
    await client.query("ROLLBACK");
  } else {
    await client.query("COMMIT");
  }
  process.stdout.write(
    `${JSON.stringify({
      dryRun,
      catalogId: bundle.catalogId,
      catalogVersion: bundle.catalogVersion,
      ...report,
    })}\n`,
  );
} catch (error) {
  await client.query("ROLLBACK").catch(() => undefined);
  throw error;
} finally {
  await client.end();
}

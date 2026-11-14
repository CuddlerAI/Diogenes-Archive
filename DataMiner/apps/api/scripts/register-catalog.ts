/**
 * CLI for docs/49 E3': register a catalog bundle into `games` +
 * `trusted_catalog_versions` so a real client's `POST /v1/sessions` stops
 * answering catalog_version_not_found.
 *
 * Usage (through the Cloud SQL proxy for production):
 *
 *   DATABASE_URL=postgres://dataminer_api:<pw>@127.0.0.1:5432/dataminer_production \
 *     pnpm --filter @dataminer/api register:catalog -- \
 *       --bundle ../../config/catalogs/release/<name> \
 *       --approve --approver "<operator email>" \
 *       --approve-trainable --allow-capture --game-trainable
 *
 * Defaults are fail-closed: without flags the rows land as draft /
 * approved_for_trainable=false and the game as not-capturable. Re-running with
 * the same bytes is a no-op; changed bytes under an unchanged version are
 * refused. `--dry-run` prints the plan without writing.
 */
import path from "node:path";

import pg from "pg";

import { readCatalogBundle, registerCatalogBundle } from "../src/register-catalog.js";

const { Client } = pg;

function flag(name: string): boolean {
  return process.argv.includes(name);
}

function option(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  if (index === -1) return undefined;
  const value = process.argv[index + 1];
  if (!value || value.startsWith("--")) {
    console.error(`${name} requires a value`);
    process.exit(2);
  }
  return value;
}

if (!process.env.DATABASE_URL) {
  console.error("DATABASE_URL is required");
  process.exit(2);
}
const bundleDir = option("--bundle");
if (!bundleDir) {
  console.error("--bundle <catalog bundle directory> is required");
  process.exit(2);
}

const approver = option("--approver");
const contentKind = option("--content-kind");
const viewKind = option("--view-kind");
const materialKind = option("--material-kind");
const options = {
  approve: flag("--approve"),
  ...(approver !== undefined ? { approver } : {}),
  approveTrainable: flag("--approve-trainable"),
  allowCapture: flag("--allow-capture"),
  gameTrainable: flag("--game-trainable"),
  ...(contentKind !== undefined ? { contentKind } : {}),
  ...(viewKind !== undefined ? { viewKind } : {}),
  ...(materialKind !== undefined ? { materialKind } : {}),
  dryRun: flag("--dry-run"),
};

const bundle = await readCatalogBundle(path.resolve(bundleDir));
console.log(
  `bundle ${bundle.catalogId}@${bundle.catalogVersion} (${bundle.schemaGeneration} schema, ${bundle.declaredNonTrainable === null ? "no authored training classification" : bundle.declaredNonTrainable ? "non-trainable" : "trainable"}) — game ${bundle.game.id}`,
);

const client = new Client({ connectionString: process.env.DATABASE_URL });
await client.connect();
try {
  await client.query("BEGIN");
  const report = await registerCatalogBundle(
    (text, params) => client.query(text, params as unknown[]),
    bundle,
    options,
  );
  if (options.dryRun) {
    await client.query("ROLLBACK");
  } else {
    await client.query("COMMIT");
  }
  console.log(JSON.stringify(report, null, 2));
  if (options.dryRun) console.log("dry run: nothing was written");
} catch (error) {
  await client.query("ROLLBACK").catch(() => undefined);
  console.error("✘ registration failed:", error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
} finally {
  await client.end();
}

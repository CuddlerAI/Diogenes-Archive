import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Client } from "pg";

import { environments } from "./environment.js";

// Named explicitly so a migration cannot be applied by an ambient or misspelled
// environment. Production is allowed; "prod", "PRODUCTION", and unset are not.
const target = process.env.DATAMINER_ENV;
if (!target || !(environments as readonly string[]).includes(target)) {
  throw new Error(`DATAMINER_ENV must be one of: ${environments.join(", ")}`);
}
if (!process.env.DATABASE_URL) {
  throw new Error("DATABASE_URL is required");
}
const client = new Client({ connectionString: process.env.DATABASE_URL });

// 迁移用 RAISE NOTICE 做存量点名：0022 / 0024 / 0029 / 0035 都在动数据之前先把
// 「有多少行、都是什么」打出来。理由写在 0035 的 ① 段——「不许只信一次人工确认，
// 那是数据事实，要落在迁移的输出里而不是落在一次对话里」。
//
// 但 node-postgres 把 NOTICE 走事件派发（`pg/lib/client.js` 的 `emit('notice', msg)`），
// **没有监听者就静默丢弃**。在此之前全仓库零监听，所以那四次点名一条都没人看见过：
// 迁移照常成功，点名照常蒸发，docs/100 §11.7 这类「回填生产实况」的坑位始终填不上。
//
// 点名是一次性的——墓碑立完之后再查，查到的是处理后的状态，不是处理前的现场。
// 挂在 connect() 之前：连上之后的第一条语句就可能发 NOTICE。
client.on("notice", (notice) => {
  process.stdout.write(`${notice.severity ?? "NOTICE"}: ${notice.message ?? ""}\n`);
});

const migrationsDirectory = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../migrations");

await client.connect();
try {
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    const version = Number(file.slice(0, file.indexOf("_")));
    const ledgerExists = await client.query<{ exists: boolean }>(
      `SELECT to_regclass('public.schema_migrations') IS NOT NULL AS exists`,
    );
    if (ledgerExists.rows[0]?.exists) {
      const applied = await client.query(`SELECT 1 FROM schema_migrations WHERE version = $1`, [version]);
      if (applied.rowCount === 1) continue;
    }
    process.stdout.write(`applying ${file}\n`);
    await client.query(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
} finally {
  await client.end();
}

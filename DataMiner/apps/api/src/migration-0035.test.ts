import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, describe, expect, it } from "vitest";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

const RETIREMENT = "0035_retire_certified_catalog_channel.sql";

async function migratedThrough34(): Promise<PGlite> {
  const db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files.filter((file) => Number(file.slice(0, 4)) <= 34)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  return db;
}

function retirementSql(): Promise<string> {
  return readFile(path.join(migrationsDirectory, RETIREMENT), "utf8");
}

const envelope = Buffer.from(JSON.stringify({ schemaVersion: "dm-signed-catalog-bundle-1" }));

/**
 * 一条 0022 之后形状的 certified 发布行，字段配齐到 catalog_releases_check2
 * （status ↔ 归属字段的那张矩阵）能过为止。
 *
 * envelope_sha256 必须与 envelope_bytes 真实对上——0015 有一条 CHECK 逐字节校验它。
 */
async function forgeCertifiedRelease(
  db: PGlite,
  status: "draft" | "published" | "superseded",
  version: string,
): Promise<string> {
  const id = randomUUID();
  const attribution =
    status === "draft"
      ? "NULL, NULL, NULL"
      : status === "published"
        ? "'ops@example.com', now(), NULL"
        : "'ops@example.com', now(), now()";
  await db.query(
    `INSERT INTO catalog_releases(
       id, channel, catalog_id, catalog_version, manifest_bytes,
       envelope_bytes, envelope_sha256, signing_key_id, status, created_by,
       published_by, published_at, superseded_at
     ) VALUES ($1, 'certified', 'dataminer-certified-cod-steam', $2,
               $3, $3, 'sha256:' || encode(sha256($3), 'hex'),
               'dm-certified-kms-v1', $4, 'test', ${attribution})`,
    [id, version, envelope, status],
  );
  return id;
}

let open: PGlite[] = [];

afterAll(async () => {
  await Promise.all(open.map((db) => db.close()));
  open = [];
});

// 重放迁移的测试自带一个 vitest 默认 5s 关不住的成本：`migratedThrough()` 每次
// 起一个全新的 PGlite 实例、逐个重放三十多份迁移。本机实测冷跑那一条 3.9–4.0 s，
// 离 5 s 只剩一秒出头的余量；`pnpm test:workspace` 让 mirror / api / console-api
// 三个包同时跑时余量被吃光，实测约三次里红一次，且失败形态是超时——症状看起来像
// 「迁移逻辑错了」，指不到真正的原因。
//
// 这里按**文件**放宽而不是全包放宽：慢的是「重放迁移」这一类测试，其余三十来个
// 测试文件仍该守着 5 s，否则真卡住的测试就没人拦了。30 s 是实测冷跑的约 7 倍、
// 并行下最坏观测值（14 s）的两倍，够宽而仍能拦住真的挂起。
describe("certified catalog channel retirement migration 0035", { timeout: 30_000 }, () => {
  it("records the migration and retires the vocabulary on a clean database", async () => {
    const db = await migratedThrough34();
    open.push(db);
    await db.exec(await retirementSql());

    const ledger = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 35`,
    );
    expect(ledger.rows).toEqual([{ name: "retire_certified_catalog_channel" }]);

    // 读 CHECK 的定义而不是靠一次插入试出来——插入会被别的约束挡下，
    // 读出来的却只有一句 "constraint failed"（0029 的同款纪律）。
    const check = await db.query<{ definition: string }>(
      `SELECT pg_get_constraintdef(oid) AS definition
         FROM pg_constraint WHERE conname = 'catalog_releases_channel_retired'`,
    );
    expect(check.rows[0]!.definition).toContain("'dev'");
    expect(check.rows[0]!.definition).toContain("revoked");
    // 旧词表没了：全表再没有任何 CHECK 提到 certified。
    const stale = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM pg_constraint
        WHERE conrelid = 'catalog_releases'::regclass AND contype = 'c'
          AND pg_get_constraintdef(oid) LIKE '%certified%'`,
    );
    expect(stale.rows[0]!.count).toBe("0");
  }, 120_000);

  // §3 的边界：索引、唯一约束、release 指针机制一律不动。0022 已经把粒度下沉到
  // (channel, catalog_id)，channel 收成单值后它们语义仍然正确。这条用例是防止
  // 后来人"顺手重建一下"——那正是 0029 当初推迟这一步时担心的成本，而它不存在。
  it("leaves every index, unique constraint and the release pointer untouched", async () => {
    const db = await migratedThrough34();
    open.push(db);
    const before = await db.query<{ indexdef: string }>(
      `SELECT indexdef FROM pg_indexes WHERE tablename = 'catalog_releases' ORDER BY indexname`,
    );
    const publishBefore = await db.query<{ definition: string }>(
      `SELECT prosrc AS definition FROM pg_proc WHERE proname = 'publish_catalog_release'`,
    );

    await db.exec(await retirementSql());

    const after = await db.query<{ indexdef: string }>(
      `SELECT indexdef FROM pg_indexes WHERE tablename = 'catalog_releases' ORDER BY indexname`,
    );
    const publishAfter = await db.query<{ definition: string }>(
      `SELECT prosrc AS definition FROM pg_proc WHERE proname = 'publish_catalog_release'`,
    );
    expect(after.rows).toEqual(before.rows);
    expect(publishAfter.rows).toEqual(publishBefore.rows);
    // 0022 下沉过的那条部分唯一索引仍在，且仍是 (channel, catalog_id) 粒度。
    expect(after.rows.map((row) => row.indexdef).join("\n")).toContain(
      "catalog_releases_one_published_per_catalog",
    );
  }, 120_000);

  /**
   * 与 0029 的关键不同：那一条遇到存量就停住（没人裁决过怎么处置），这一条
   * **处置**存量——issue #829 §5 选项 A，owner 2026-08-20 裁决。
   *
   * 处置方式是 revoke 成墓碑，而墓碑的价值全在"留下来的东西"：签名字节、
   * 发布归属、审计链。所以这里逐项断言留存，而不只断言 status 变了。
   */
  it("entombs a published certified release, keeping its bytes and publication attribution", async () => {
    const db = await migratedThrough34();
    open.push(db);
    const id = await forgeCertifiedRelease(db, "published", "2026-07-31.2");

    await db.exec(await retirementSql());

    const row = await db.query<{
      status: string;
      channel: string;
      envelope_bytes: Uint8Array;
      published_by: string | null;
      published_at: Date | null;
      revoked_by: string | null;
      revoked_at: Date | null;
    }>(
      `SELECT status, channel, envelope_bytes, published_by, published_at,
              revoked_by, revoked_at
         FROM catalog_releases WHERE id = $1`,
      [id],
    );
    const stored = row.rows[0]!;
    expect(stored.status).toBe("revoked");
    // 渠道原样留着——它是审计事实，不可改（protect_catalog_release）。
    expect(stored.channel).toBe("certified");
    // "要留的是字节"：envelope 逐字节不动。
    expect(Buffer.from(stored.envelope_bytes).equals(envelope)).toBe(true);
    // 发布归属完整留存，正是 0026 注释说的效果。
    expect(stored.published_by).toBe("ops@example.com");
    expect(stored.published_at).not.toBeNull();
    expect(stored.revoked_by).toBe("migration-0035");
    expect(stored.revoked_at).not.toBeNull();

    // 审计链补上了一条，照 0026 的形状。
    const audit = await db.query<{ actor_id: string; details: Record<string, unknown> }>(
      `SELECT actor_id, details FROM audit_events
        WHERE action = 'catalog.release_revoked' AND entity_id = $1`,
      [id],
    );
    expect(audit.rows).toHaveLength(1);
    expect(audit.rows[0]!.actor_id).toBe("migration-0035");
    expect(audit.rows[0]!.details).toMatchObject({
      channel: "certified",
      catalogId: "dataminer-certified-cod-steam",
      catalogVersion: "2026-07-31.2",
      reason: "certified_channel_retired",
    });
  }, 120_000);

  // draft 与 superseded 也要被扫到。只处置 published 会把一条 draft 留在库里，
  // 而下面那条 CHECK 会当场在它上面失败——迁移在生产上原地炸掉。
  it("entombs draft and superseded certified rows too, not just the published one", async () => {
    const db = await migratedThrough34();
    open.push(db);
    const draft = await forgeCertifiedRelease(db, "draft", "2026-07-31.3");
    const superseded = await forgeCertifiedRelease(db, "superseded", "2026-07-31.1");

    await db.exec(await retirementSql());

    const rows = await db.query<{ id: string; status: string }>(
      `SELECT id, status FROM catalog_releases WHERE id = ANY($1::uuid[]) ORDER BY id`,
      [[draft, superseded].sort()],
    );
    expect(rows.rows.map((row) => row.status)).toEqual(["revoked", "revoked"]);
  }, 120_000);

  // 收词表的**目的**：新的 certified 发布再也进不来。墓碑活着不等于渠道活着。
  it("refuses every new certified release while the tombstone survives", async () => {
    const db = await migratedThrough34();
    open.push(db);
    const entombed = await forgeCertifiedRelease(db, "published", "2026-07-31.2");
    await db.exec(await retirementSql());

    // draft（INSERT 的默认状态）与 published 都进不来。
    await expect(forgeCertifiedRelease(db, "draft", "2026-08-20.1")).rejects.toThrow();
    await expect(forgeCertifiedRelease(db, "published", "2026-08-20.2")).rejects.toThrow();
    // dev 仍然收得进——收的是 certified，不是整张表。
    await expect(
      db.query(
        `INSERT INTO catalog_releases(
           id, channel, catalog_id, catalog_version, manifest_bytes,
           envelope_bytes, envelope_sha256, signing_key_id, created_by
         ) VALUES ($1, 'dev', 'dataminer-call-of-duty-steam-release-test', '2026-08-20.0',
                   $2, $2, 'sha256:' || encode(sha256($2), 'hex'),
                   'dm-dev-catalog-delivery-v1', 'test')`,
        [randomUUID(), envelope],
      ),
    ).resolves.toBeDefined();

    // 墓碑本身分毫未动，而且**删不掉**（reject_catalog_release_delete，0015:200）。
    const survivor = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM catalog_releases WHERE id = $1`,
      [entombed],
    );
    expect(survivor.rows[0]!.count).toBe("1");
    await expect(
      db.query(`DELETE FROM catalog_releases WHERE id = $1`, [entombed]),
    ).rejects.toThrow(/immutable and may only transition to revoked/);
  }, 120_000);
});

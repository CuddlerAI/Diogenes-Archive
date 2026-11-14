/**
 * 迁移 0038 与 0028 标题闸的合围（issue #855 PR 2）。
 *
 * 这条测试存在的理由是一个具体的失败模式：改完 32 份 bundle 的 `display_name`、
 * 重新注册一遍，人会以为标题已经跟着变了。**不会。** `registerCatalogBundle()`
 * 对已存在的 `games` 行只升级 capture/trainable 两个 flag，从不写 title；两个 flag
 * 都无需升级时连 UPDATE 都不发。于是库里那行仍写着「禁止训练」，而 0028 的
 * `games_non_trainable_title_guard` 会在 PR 5 的后批准时把它挡下来——在离改名三个
 * 月之后的一个完全不相干的地方。
 *
 * 所以这里用真数据库（PGlite = 进程内真 Postgres，CHECK 与触发器都真的在跑）钉四件事：
 *
 *   ① 所有活跃 catalog 的标题都不命中 `禁止训练|non[-_ ]?trainable`；
 *   ② 0038 把已登记的旧标题改成中性标题，带审计（含旧值），且不碰任何审批位；
 *   ③ 代表游戏（COD）在 0038 之后**能**被批准为可训练——0028 之前正是这一步被拒；
 *   ④ 0028 的 CHECK 仍然生效：构造一个自相矛盾的标题照旧被拒。
 */
import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { beforeAll, describe, expect, it } from "vitest";

import {
  NON_TRAINABLE_TITLE_PATTERN,
  readCatalogBundle,
  registerCatalogBundle,
  titleDeclaresNonTrainable,
  type QueryFn,
} from "./register-catalog.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const migrationsDirectory = path.resolve(here, "../migrations");
const catalogsDirectory = path.resolve(here, "../../../config/catalogs");
const devDirectory = path.join(catalogsDirectory, "dev");

/** 活跃目录 = 32 shipped + synthetic。三份带 SUPERSEDED.md 的冻结快照不在其中。 */
async function activeCatalogTitles(): Promise<Array<{ gameId: string; title: string }>> {
  const shipped = JSON.parse(
    await readFile(path.join(catalogsDirectory, "shipped-games.json"), "utf8"),
  ) as { games: Array<{ catalogDir: string }> };
  const directories = [...shipped.games.map((game) => game.catalogDir), "synthetic"];
  return Promise.all(
    directories.map(async (directory) => {
      const profile = JSON.parse(
        await readFile(path.join(devDirectory, directory, "game-profile.json"), "utf8"),
      ) as { game_id: string; display_name: string };
      return { gameId: profile.game_id, title: profile.display_name };
    }),
  );
}

async function migrationFiles(): Promise<string[]> {
  return (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
}

async function migratedThrough(highest: number): Promise<PGlite> {
  const db = new PGlite();
  for (const file of (await migrationFiles()).filter((f) => Number(f.slice(0, 4)) <= highest)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  return db;
}

async function applyMigration(db: PGlite, version: number): Promise<void> {
  const file = (await migrationFiles()).find((f) => Number(f.slice(0, 4)) === version);
  if (!file) throw new Error(`migration ${version} not found`);
  await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
}

const COD_DIRECTORY = path.join(devDirectory, "cod-steam-release-test");
const COD_GAME_ID = "call-of-duty-steam-release-test";
/** 0028 的迁移注释逐字记下的那个旧标题；它是这条闸最初挡住的那一行。 */
const COD_LEGACY_TITLE = "Call of Duty® Steam（release 测试，禁止训练）";

describe("① 33 份活跃 catalog 的标题都不再自称禁止训练", () => {
  it("no active display_name matches the 0028 guard pattern", async () => {
    const titles = await activeCatalogTitles();
    expect(titles).toHaveLength(33);
    const offenders = titles.filter((entry) => titleDeclaresNonTrainable(entry.title));
    expect(offenders).toEqual([]);
  });

  it("keeps the pattern itself byte-identical to migration 0028", async () => {
    // 两层同口径：工具这层挡不住的写法，数据库那层也不该放行，反之亦然。
    const sql = await readFile(
      path.join(migrationsDirectory, "0028_non_trainable_title_guard.sql"),
      "utf8",
    );
    expect(sql).toContain("禁止训练|non[-_ ]?trainable");
    expect(NON_TRAINABLE_TITLE_PATTERN.source).toBe("禁止训练|non[-_ ]?trainable");
  });
});

describe("② 0038 迁移已登记的旧标题", () => {
  let db: PGlite;
  let titles: Array<{ gameId: string; title: string }>;

  beforeAll(async () => {
    titles = await activeCatalogTitles();
    db = await migratedThrough(37);
    // 只种"生产库真的有的那些行"里的一个代表 + 一个已经中性的行 + 一个不相干的行。
    // 三行各证一件事：会改、不会重复改、不会误伤。
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ($1, $2, 'game', 'first_person', 'rendered', true, false)`,
      [COD_GAME_ID, COD_LEGACY_TITLE],
    );
    const synthetic = titles.find((entry) => entry.gameId === "synthetic-win32-dev")!;
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ($1, $2, 'game', 'first_person', 'rendered', true, false)`,
      [synthetic.gameId, synthetic.title],
    );
    // 不在活跃 catalog 里，但标题命中同一条正则：本迁移是逐 id 点名的，
    // 不得顺手把它一起改掉——一次改名是一次身份变更，得有出处。
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ('an-unrelated-legacy-game', '某个历史条目（禁止训练）',
               'game', 'first_person', 'rendered', false, false)`,
    );
    await applyMigration(db, 38);
  }, 120_000);

  it("rewrites the registered legacy title to its neutral form", async () => {
    const row = await db.query<{ title: string }>(`SELECT title FROM games WHERE id = $1`, [
      COD_GAME_ID,
    ]);
    expect(row.rows[0]!.title).toBe(titles.find((e) => e.gameId === COD_GAME_ID)!.title);
    expect(row.rows[0]!.title).toBe("Call of Duty® Steam");
  });

  it("records an audit event carrying the old title, which nothing else remembers", async () => {
    const audit = await db.query<{ entity_id: string; details: Record<string, unknown> }>(
      `SELECT entity_id, details FROM audit_events
        WHERE actor_id = 'migration-0038' AND action = 'game.title_neutralized'`,
    );
    expect(audit.rows).toHaveLength(1);
    expect(audit.rows[0]!.entity_id).toBe(COD_GAME_ID);
    expect(audit.rows[0]!.details).toMatchObject({
      oldTitle: COD_LEGACY_TITLE,
      newTitle: "Call of Duty® Steam",
      approvedForTrainable: false,
    });
  });

  it("touches neither approval bit", async () => {
    const row = await db.query<{ allowed_for_capture: boolean; approved_for_trainable: boolean }>(
      `SELECT allowed_for_capture, approved_for_trainable FROM games WHERE id = $1`,
      [COD_GAME_ID],
    );
    expect(row.rows[0]).toEqual({ allowed_for_capture: true, approved_for_trainable: false });
  });

  it("leaves an already-neutral row and an unrelated game alone", async () => {
    const untouched = await db.query<{ title: string }>(
      `SELECT title FROM games WHERE id = 'an-unrelated-legacy-game'`,
    );
    expect(untouched.rows[0]!.title).toBe("某个历史条目（禁止训练）");
    // synthetic 的标题本来就是中性的，所以它不该产生审计行——上面已断言只有 1 行。
    const synthetic = await db.query<{ title: string }>(
      `SELECT title FROM games WHERE id = 'synthetic-win32-dev'`,
    );
    expect(synthetic.rows[0]!.title).toBe("DataMiner Synthetic Game");
  });

  it("renames nothing and audits nothing when the titles are already neutral", async () => {
    // 迁移只更新 title 确实不同的行。这条测的是"已经是目标状态"那个分支：一个已经
    // 跑过 0038 的库（或一个新库先用中性标题登记过）再跑一次，应当 0 行、0 条审计。
    // 不能靠"对同一个库重跑迁移文件"来测——schema_migrations 的唯一键会先炸，
    // 那验的是迁移运行器的守卫，不是本迁移的幂等性。
    const fresh = await migratedThrough(37);
    try {
      await fresh.query(
        `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                           allowed_for_capture, approved_for_trainable)
         VALUES ($1, $2, 'game', 'first_person', 'rendered', true, false)`,
        [COD_GAME_ID, "Call of Duty® Steam"],
      );
      await applyMigration(fresh, 38);
      const audit = await fresh.query<{ count: string }>(
        `SELECT count(*)::text AS count FROM audit_events WHERE actor_id = 'migration-0038'`,
      );
      expect(audit.rows[0]!.count).toBe("0");
      const row = await fresh.query<{ title: string }>(
        `SELECT title FROM games WHERE id = $1`,
        [COD_GAME_ID],
      );
      expect(row.rows[0]!.title).toBe("Call of Duty® Steam");
    } finally {
      await fresh.close();
    }
  }, 120_000);
});

describe("③/④ 0028 的 CHECK 保留：中性标题可后批准，矛盾标题仍被拒", () => {
  let db: PGlite;
  const query: QueryFn = async (text, params) => {
    const result = await db.query(text, params as unknown[]);
    return { rows: result.rows as Array<Record<string, unknown>> };
  };

  beforeAll(async () => {
    db = await migratedThrough(38);
  }, 120_000);

  it("refuses the legacy title outright — this is the state 0038 had to move us out of", async () => {
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ('guard-legacy-title', $1, 'game', 'first_person', 'rendered', true, false)`,
      [COD_LEGACY_TITLE],
    );
    await expect(
      db.query(`UPDATE games SET approved_for_trainable = true WHERE id = 'guard-legacy-title'`),
    ).rejects.toThrow(/games_non_trainable_title_guard/);
  });

  it("lets the representative game be approved for trainable once its title is neutral", async () => {
    const bundle = await readCatalogBundle(COD_DIRECTORY);
    expect(bundle.schemaGeneration).toBe("neutral");
    // 中性一代不声明训练分类，所以第一道守卫（bundle 自称 non_trainable）不再适用；
    // 第二道（标题）才是那次事故真正的挡板，它现在放行是因为标题真的中性了。
    expect(bundle.declaredNonTrainable).toBeNull();
    expect(titleDeclaresNonTrainable(bundle.game.title)).toBe(false);

    const report = await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "migration-0038-test@example.com",
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: true,
    });
    expect(report.game.action).toBe("inserted");

    const row = await db.query<{ title: string; approved_for_trainable: boolean }>(
      `SELECT title, approved_for_trainable FROM games WHERE id = $1`,
      [COD_GAME_ID],
    );
    expect(row.rows[0]).toEqual({
      title: "Call of Duty® Steam",
      approved_for_trainable: true,
    });
  });

  it("still refuses --game-trainable for a title that declares itself non-trainable", async () => {
    const bundle = await readCatalogBundle(COD_DIRECTORY);
    const contradictory = {
      ...bundle,
      game: { ...bundle.game, id: `guard-${randomUUID().slice(0, 8)}`, title: COD_LEGACY_TITLE },
    };
    await expect(
      registerCatalogBundle(query, contradictory, {
        approve: true,
        approver: "migration-0038-test@example.com",
        approveTrainable: false,
        allowCapture: true,
        gameTrainable: true,
      }),
    ).rejects.toThrow(/declares it non-trainable/);
  });
});

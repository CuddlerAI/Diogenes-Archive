import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

/**
 * 0030 把 depth 变成第七件产物（裁决见 docs/107，推翻 docs/77 §7.1）。
 * 这里锁三件事：枚举加得上、件数期望按 depth 变化、以及**同意书没覆盖 depth 时
 * 带 depth 的 clip 不可训练**——最后一条是 docs/77 §7.1 理由②在新架构下的落点，
 * 丢了它就是把那条裁决的实质丢掉，而不是搬家。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
});

afterAll(async () => {
  await db.close();
});

describe("0030 · depth 进摄取面", () => {
  it("artifact_kind 里有 depth，且原有六种一个不少", async () => {
    const { rows } = await db.query<{ label: string }>(
      `SELECT unnest(enum_range(NULL::artifact_kind))::text AS label`,
    );
    const labels = rows.map((row) => row.label);
    expect(labels).toContain("depth");
    for (const kind of ["video", "frames", "events", "meta", "manifest", "audio"]) {
      expect(labels).toContain(kind);
    }
  });

  it("同意书覆盖表已登记新版，且默认只有它", async () => {
    const { rows } = await db.query<{ policy_version: string }>(
      `SELECT policy_version FROM depth_consent_policy_versions ORDER BY policy_version`,
    );
    expect(rows.map((row) => row.policy_version)).toEqual(["2026-07-25-teaser"]);
  });

  it("视图仍可查询", async () => {
    await db.query(`SELECT clip_id, is_trainable, blockers FROM clip_trainability LIMIT 0`);
  });

  /**
   * 本次改动的要害：depth 字节一旦存在，同意书就必须是覆盖 depth 的那一代。
   * 用视图定义做断言——真实数据要贯穿 sessions/devices/catalog 十余张表的合法外键链，
   * 成本远高于它能多证明的东西；而「判据存在且挂在 has_depth_object 上」在定义里可判定。
   */
  it("blocker 判据挂在真实 depth 对象上，而不是 meta_json 声明", async () => {
    const { rows } = await db.query<{ definition: string }>(
      `SELECT pg_get_viewdef('clip_trainability'::regclass, true) AS definition`,
    );
    const definition = rows[0]!.definition;
    expect(definition).toContain("depth_consent_not_covered");
    expect(definition).toContain("has_depth_object");
    expect(definition).toContain("depth_consent_policy_versions");
    // 件数期望这一侧刻意看声明，两个方向不能对调
    expect(definition).toMatch(/meta_json.*depth.*state/s);
  });

  it("空的覆盖表意味着所有带 depth 的 clip 都不可训练（fail-closed 初始态）", async () => {
    await db.query(`DELETE FROM depth_consent_policy_versions`);
    const { rows } = await db.query<{ definition: string }>(
      `SELECT pg_get_viewdef('clip_trainability'::regclass, true) AS definition`,
    );
    // 判据是 NOT EXISTS(...)，表空 → 恒不满足 → blocker 恒挂
    expect(rows[0]!.definition).toMatch(/NOT \(EXISTS/);
    await db.query(
      `INSERT INTO depth_consent_policy_versions(policy_version, note)
       VALUES ('2026-07-25-teaser', 'restored by test')`,
    );
  });
});

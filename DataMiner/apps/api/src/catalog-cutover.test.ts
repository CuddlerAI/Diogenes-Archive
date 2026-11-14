/**
 * issue #855 PR 4 断代编排的真库端到端。
 *
 * 这里跑的是真东西：整条迁移账本进 PGlite，仓库里真实的 32 款计划文件与真实 bundle
 * 字节，注册 + 签名 + 发布 + 切 current 全走 `runCutover`。三个陷阱各有一条断言，
 * 且断的是**后果**而不是「参数传了没有」：
 *
 * ① 发出去的 manifest 字节确实是中性一代且不含 `non_trainable`；
 * ② 跑完之后 `games` 的两个治理位一位都没动，即使计划里那款标着 admitted；
 * ③ 有在飞会话时 `preflight` 拒绝执行，给了 override 才放行。
 */
import { mkdtemp, readdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  CutoverError,
  preflight,
  prepareCutover,
  readCutoverFacts,
  runCutover,
  statusParity,
  trustedKey,
  type CutoverFacts,
  type PreparedRelease,
} from "./catalog-cutover.js";
import { CATALOG_SCHEMA_VERSIONS } from "./catalog-schema.js";
import type { CatalogQuery, CatalogValidator } from "./signed-catalog.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const migrationsDirectory = path.resolve(here, "../migrations");
const repoRoot = path.resolve(here, "../../..");
const planPath = path.join(repoRoot, "config", "catalogs", "release-plan.json");
const catalogRoot = path.join(repoRoot, "config", "catalogs", "dev");

/** 一款 4 份 payload + 一款 6 份 payload（带 depth/camera 可选册子）。 */
const FOUR_PAYLOAD_GAME = "atomic-heart-pc";
const DEPTH_GAME = "fallout4-steam-release-test";
const ACTOR = "cutover-test@example.com";

let db: PGlite;
let query: CatalogQuery;

/**
 * 本文件**不跑**真的 `ClientSourceCatalogValidator`，理由有两条：
 *
 * 1. 它不加任何覆盖。「真 Rust 客户端解析器收得下中性 schema」已经由
 *    `signed-catalog.test.ts` 那条 "validates and signs the authored Fallout 4 depth
 *    pair through the client loader" 证明了——它用的 `FALLOUT4_DIR` 就是
 *    `fallout4-steam-release-test`，一份 shipped 的中性 bundle。这里再跑五遍只是重复。
 * 2. 它会**弄红别人的测试**。`frontend` job 没有任何 Rust 步骤，所以那个 validator 落到
 *    `cargo run`，由 rustup 按 `rust-toolchain.toml` 现装 1.97.1。vitest 是 4 worker
 *    并行的（`vitest.config.ts`，#864），多个 worker 同时首次触发 rustup 安装会互相踩：
 *    实测报 `recovering from a partially installed toolchain` 之后整条工具链损坏，此后
 *    每次 cargo 调用都失败——红的是 `signed-catalog.test.ts` 与 `catalog-publisher.test.ts`，
 *    而它们和这次改动毫无关系。
 *
 * 桩仍然守着这个接缝：它断言交到 validator 手上的确实是中性 manifest。
 */
const neutralManifestValidator: CatalogValidator = {
  validate(manifest) {
    const parsed = JSON.parse(manifest.toString("utf8")) as Record<string, unknown>;
    expect(parsed.schema_version).toBe(CATALOG_SCHEMA_VERSIONS.manifest.neutral);
    expect("non_trainable" in parsed).toBe(false);
    return Promise.resolve();
  },
};

beforeAll(async () => {
  db = new PGlite();
  for (const file of (await readdir(migrationsDirectory))
    .filter((name) => /^\d+_[a-z0-9_]+\.sql$/.test(name))
    .sort()) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  query = async (text, params) => {
    const result = await db.query(text, params);
    return { rows: result.rows as Array<Record<string, unknown>> };
  };
}, 180_000);

afterAll(async () => {
  await db?.close();
});

async function preparePair(): Promise<PreparedRelease[]> {
  const cutover = await prepareCutover({
    planPath,
    catalogRoot,
    only: [FOUR_PAYLOAD_GAME, DEPTH_GAME],
  });
  return cutover.releases;
}

describe("release plan preparation (offline)", () => {
  it("reads the manifest schema from the plan file instead of hardcoding it", async () => {
    const cutover = await prepareCutover({ planPath, catalogRoot });
    const onDisk = JSON.parse(await readFile(planPath, "utf8")) as { manifestSchemaVersion: string };
    // 陷阱 ①：脚本发的就是计划文件说的那一代。硬编码会和计划文件漂移。
    expect(cutover.manifestSchemaVersion).toBe(onDisk.manifestSchemaVersion);
    expect(cutover.manifestSchemaVersion).toBe(CATALOG_SCHEMA_VERSIONS.manifest.neutral);
    expect(cutover.generation).toBe("neutral");
    expect(cutover.releases).toHaveLength(32);
    expect(cutover.releases.every(({ bundle }) => bundle.schemaGeneration === "neutral")).toBe(true);
    // 中性一代不再声明训练分类（issue #855 §A.4）。
    expect(cutover.releases.every(({ bundle }) => bundle.declaredNonTrainable === null)).toBe(true);
  });

  it("refuses a plan whose manifest generation disagrees with the bundles on disk", async () => {
    // 陷阱 ① 的另一半：计划说 legacy、bundle 已经是中性，必须在连库之前判死，
    // 而不是发出去 32 份旧格式 manifest。
    const staging = await mkdtemp(path.join(tmpdir(), "dataminer-cutover-plan-"));
    const plan = JSON.parse(await readFile(planPath, "utf8")) as Record<string, unknown>;
    plan.manifestSchemaVersion = CATALOG_SCHEMA_VERSIONS.manifest.legacy;
    const legacyPlan = path.join(staging, "release-plan.json");
    await writeFile(legacyPlan, JSON.stringify(plan));
    await expect(prepareCutover({ planPath: legacyPlan, catalogRoot })).rejects.toThrow(
      /bundle is neutral schema but the plan publishes dm-catalog-bundle-2/,
    );
  });

  it("refuses an unknown manifest schema version", async () => {
    const staging = await mkdtemp(path.join(tmpdir(), "dataminer-cutover-plan-"));
    const plan = JSON.parse(await readFile(planPath, "utf8")) as Record<string, unknown>;
    plan.manifestSchemaVersion = "dm-catalog-bundle-99";
    const bogus = path.join(staging, "release-plan.json");
    await writeFile(bogus, JSON.stringify(plan));
    await expect(prepareCutover({ planPath: bogus, catalogRoot })).rejects.toThrow(CutoverError);
  });

  it("refuses a plan whose pinned content hash drifted from the bundle bytes", async () => {
    const staging = await mkdtemp(path.join(tmpdir(), "dataminer-cutover-plan-"));
    const plan = JSON.parse(await readFile(planPath, "utf8")) as {
      releases: Array<{ gameId: string; payloads: Array<{ contentHash: string }> }>;
    };
    const target = plan.releases.find((release) => release.gameId === FOUR_PAYLOAD_GAME)!;
    target.payloads[0]!.contentHash = `sha256:${"0".repeat(64)}`;
    const drifted = path.join(staging, "release-plan.json");
    await writeFile(drifted, JSON.stringify(plan));
    await expect(
      prepareCutover({ planPath: drifted, catalogRoot, only: [FOUR_PAYLOAD_GAME] }),
    ).rejects.toThrow(/which the bundle on disk does not carry/);
  });

  it("honours --only and --limit", async () => {
    const one = await prepareCutover({ planPath, catalogRoot, only: [FOUR_PAYLOAD_GAME] });
    expect(one.releases.map(({ planned }) => planned.gameId)).toEqual([FOUR_PAYLOAD_GAME]);
    const three = await prepareCutover({ planPath, catalogRoot, limit: 3 });
    expect(three.releases).toHaveLength(3);
    await expect(prepareCutover({ planPath, catalogRoot, only: ["no-such-game"] })).rejects.toThrow(
      /not in the release plan/,
    );
  });
});

describe("cutover against a real migration ledger", () => {
  it("registers, signs, publishes and switches current for each game", async () => {
    const releases = await preparePair();
    const outcomes = await runCutover(releases, {
      manifestSchemaVersion: CATALOG_SCHEMA_VERSIONS.manifest.neutral,
      actor: ACTOR,
      approve: false,
      registerQuery: query,
      publishQuery: query,
      validator: neutralManifestValidator,
    });
    expect(outcomes.map((outcome) => outcome.result)).toEqual(["published", "published"]);

    for (const { planned } of releases) {
      const release = await db.query<{
        status: string;
        catalog_version: string;
        manifest_bytes: Uint8Array;
        published_by: string;
      }>(
        `SELECT status::text AS status, catalog_version, manifest_bytes, published_by
           FROM catalog_releases WHERE channel = 'dev' AND catalog_id = $1`,
        [planned.catalogId],
      );
      expect(release.rows).toHaveLength(1);
      const row = release.rows[0]!;
      // current 切换：每 (channel, catalog_id) 只有一份 published（迁移 0022）。
      expect(row.status).toBe("published");
      expect(row.catalog_version).toBe(planned.catalogVersion);
      expect(row.published_by).toBe(ACTOR);

      // 陷阱 ①：发出去的字节确实是中性一代。漏传 manifestSchemaVersion 时这里会是
      // dm-catalog-bundle-2 且带 non_trainable，而发布依然"成功"——所以断的是字节。
      const manifest = JSON.parse(Buffer.from(row.manifest_bytes).toString("utf8")) as Record<
        string,
        unknown
      >;
      expect(manifest.schema_version).toBe(CATALOG_SCHEMA_VERSIONS.manifest.neutral);
      expect("non_trainable" in manifest).toBe(false);
      expect(manifest.catalog_id).toBe(planned.catalogId);
    }

    // 6 份 payload 的那款把可选的 depth/camera 册子也带进了 roster（迁移 0033 之后
    // 才发得出去；0022 的 `kind_count <> 4` 会拒绝它）。
    const admitted = releases.find(({ planned }) => planned.gameId === DEPTH_GAME)!;
    expect(admitted.planned.payloads).toHaveLength(6);
    const entries = await db.query<{ kind: string }>(
      `SELECT DISTINCT entry.kind::text AS kind
         FROM catalog_release_entries entry
         JOIN catalog_releases release ON release.id = entry.release_id
        WHERE release.catalog_id = $1`,
      [admitted.planned.catalogId],
    );
    expect(entries.rows.map((entry) => entry.kind).sort()).toEqual([
      "camera_binding",
      "depth_profile",
      "device_profile",
      "game_mapping",
      "game_profile",
      "recording_profile",
    ]);
  }, 300_000);

  it("leaves every governance bit untouched", async () => {
    // 陷阱 ②：断代只换格式。计划里的 admission=admitted 是采集准入的独立决定，
    // 不该被一次格式断代顺手翻开。
    const games = await db.query<{
      id: string;
      allowed_for_capture: boolean;
      approved_for_trainable: boolean;
    }>(
      `SELECT id, allowed_for_capture, approved_for_trainable FROM games WHERE id = ANY($1::text[])`,
      [[FOUR_PAYLOAD_GAME, DEPTH_GAME]],
    );
    expect(games.rows).toHaveLength(2);
    for (const row of games.rows) {
      expect(row.allowed_for_capture).toBe(false);
      expect(row.approved_for_trainable).toBe(false);
    }
    const trusted = await db.query<{ status: string; approved_for_trainable: boolean }>(
      `SELECT t.status::text AS status, t.approved_for_trainable
         FROM trusted_catalog_versions t WHERE t.game_id = ANY($1::text[])`,
      [[FOUR_PAYLOAD_GAME, DEPTH_GAME]],
    );
    expect(trusted.rows.length).toBeGreaterThan(0);
    // 未传 --approve：行落 draft，可训练审批位保持 false。
    expect(trusted.rows.every((row) => row.status === "draft")).toBe(true);
    expect(trusted.rows.every((row) => row.approved_for_trainable === false)).toBe(true);
  });

  it("is resumable: a re-run skips what is already published and writes nothing new", async () => {
    const before = await db.query<{ count: string }>(`SELECT count(*)::text AS count FROM catalog_releases`);
    const releases = await preparePair();
    const outcomes = await runCutover(releases, {
      manifestSchemaVersion: CATALOG_SCHEMA_VERSIONS.manifest.neutral,
      actor: ACTOR,
      approve: false,
      registerQuery: query,
      publishQuery: query,
      validator: neutralManifestValidator,
    });
    expect(outcomes.map((outcome) => outcome.result)).toEqual(["skipped", "skipped"]);
    const after = await db.query<{ count: string }>(`SELECT count(*)::text AS count FROM catalog_releases`);
    expect(after.rows[0]!.count).toBe(before.rows[0]!.count);
  }, 120_000);

  it("stops with a pointed message when a release exists at this version but is not published", async () => {
    // 同一 (channel, catalog_id, catalog_version) 发不了第二次（唯一约束），也不能改写
    // 既有行。恢复逻辑必须认出这一款需要人裁决，而不是撞进 publisher 事务中段。
    const releases = await preparePair();
    const target = releases.find(({ planned }) => planned.gameId === FOUR_PAYLOAD_GAME)!;
    await db.query(
      `UPDATE catalog_releases
          SET status = 'revoked', revoked_by = 'test', revoked_at = transaction_timestamp()
        WHERE channel = 'dev' AND catalog_id = $1`,
      [target.planned.catalogId],
    );
    const outcomes = await runCutover([target], {
      manifestSchemaVersion: CATALOG_SCHEMA_VERSIONS.manifest.neutral,
      actor: ACTOR,
      approve: false,
      registerQuery: query,
      publishQuery: query,
      validator: neutralManifestValidator,
    });
    expect(outcomes).toHaveLength(1);
    expect(outcomes[0]!.result).toBe("failed");
    expect(outcomes[0]!.detail).toMatch(/status=revoked; it cannot be re-published/);
  }, 120_000);

  it("reads the database facts the preflight needs against the real schema", async () => {
    const releases = await preparePair();
    const facts = await readCutoverFacts(query, releases, 6);
    // 这条断言真正证明的是那几段 SQL 对得上真账本（`sessions` / `games` /
    // `trusted_catalog_versions` / `catalog_releases` 的列名与类型）。
    expect(facts.inFlightTotal).toBe(0);
    expect(facts.inFlight).toEqual([]);
    expect(facts.games.size).toBe(2);
    expect(facts.releases.length).toBeGreaterThan(0);
    expect(facts.trusted.size).toBeGreaterThan(0);
  }, 120_000);
});

describe("preflight gates", () => {
  function factsWith(overrides: Partial<CutoverFacts>): CutoverFacts {
    return {
      trusted: new Map(),
      games: new Map(),
      releases: [],
      inFlight: [],
      inFlightTotal: 0,
      sessionWindowHours: 6,
      ...overrides,
    };
  }

  it("refuses to run while a session is in flight, and yields to the explicit override", async () => {
    // 陷阱 ③：切 current 会掐断正在录制的那一场（orchestrator.rs#try_activate_catalog）。
    const releases = await preparePair();
    const facts = factsWith({
      inFlight: [{ game_id: FOUR_PAYLOAD_GAME, sessions: 2, latest: "2026-08-26 10:00:00+00" }],
      inFlightTotal: 2,
    });
    const blocked = preflight(releases, facts, {
      approve: false,
      allowActiveSessions: false,
      acceptStatusDrift: false,
    });
    expect(blocked.inFlightSelected).toBe(2);
    expect(blocked.blockers).toHaveLength(1);
    expect(blocked.blockers[0]).toMatch(/in-flight session\(s\).*cut by the current switch/);

    const overridden = preflight(releases, facts, {
      approve: false,
      allowActiveSessions: true,
      acceptStatusDrift: false,
    });
    expect(overridden.blockers).toEqual([]);
  });

  it("refuses when the incoming row status would not match its predecessor", async () => {
    // 陷阱 ②：格式断代不该顺带改治理姿态，两个方向都算改。
    const releases = await preparePair();
    const trusted = new Map(
      releases.flatMap(({ planned }) =>
        planned.payloads.map((payload) => [
          trustedKey(payload.kind, payload.catalogId),
          [
            {
              kind: payload.kind,
              catalog_id: payload.catalogId,
              version: "predecessor",
              status: "approved",
              approved_for_trainable: false,
              content_hash: `sha256:${"1".repeat(64)}`,
            },
          ],
        ]),
      ),
    );
    const facts = factsWith({ trusted });

    const withoutApprove = preflight(releases, facts, {
      approve: false,
      allowActiveSessions: false,
      acceptStatusDrift: false,
    });
    expect(withoutApprove.parity.drift.length).toBe(10);
    expect(withoutApprove.blockers[0]).toMatch(/status their predecessor row does not have/);

    // 与前代看齐就不再是漂移。
    const withApprove = preflight(releases, facts, {
      approve: true,
      allowActiveSessions: false,
      acceptStatusDrift: false,
    });
    expect(withApprove.parity.drift).toEqual([]);
    expect(withApprove.blockers).toEqual([]);

    // 显式接受也放行。
    const accepted = preflight(releases, facts, {
      approve: false,
      allowActiveSessions: false,
      acceptStatusDrift: true,
    });
    expect(accepted.blockers).toEqual([]);
  });

  it("does not infer a posture from a revoked or absent predecessor", async () => {
    const releases = await preparePair();
    const payload = releases[0]!.planned.payloads[0]!;
    const revoked = statusParity(
      releases,
      factsWith({
        trusted: new Map([
          [
            trustedKey(payload.kind, payload.catalogId),
            [
              {
                kind: payload.kind,
                catalog_id: payload.catalogId,
                version: "predecessor",
                status: "revoked",
                approved_for_trainable: false,
                content_hash: `sha256:${"2".repeat(64)}`,
              },
            ],
          ],
        ]),
      }),
      false,
    );
    // 撤销是治理行为，不该被一次格式断代继承；没有前代的也不产生约束。
    expect(revoked.drift).toEqual([]);
    expect(revoked.summary.get("predecessor revoked")).toBe(1);
    expect(revoked.summary.get("no predecessor")).toBe(9);
  });

  it("reports admitted games that the database does not actually allow capture for", async () => {
    const releases = await preparePair();
    const verdict = preflight(releases, factsWith({}), {
      approve: false,
      allowActiveSessions: false,
      acceptStatusDrift: false,
    });
    // 只报告，不修：翻开采集准入是独立的治理决定（issue #855 §A）。
    expect(verdict.admittedButNotCapturable).toEqual([FOUR_PAYLOAD_GAME, DEPTH_GAME]);
    expect(verdict.blockers).toEqual([]);
  });
});

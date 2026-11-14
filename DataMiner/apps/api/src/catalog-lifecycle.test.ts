/**
 * 目录生命周期的四类分层，以及发布侧那条闸（issue #855 §1）。
 *
 * 分类本身由 `tools/check-catalog-lifecycle.mjs` 从仓库真实目录**派生**（shipped 由
 * shipped-games.json 回答，冻结由 `SUPERSEDED.md` sidecar 回答）。这里直接 import
 * 那份实现，而不是再抄一遍判据——抄一遍就有两份可以互相矛盾的真相，而本 issue 要
 * 消灭的正是这种。
 *
 * 服务端的 `catalog-lifecycle.ts` 只能手写常量：它跑在 Cloud Run 的摄取 API 里，那里
 * 没有 `config/catalogs/dev/`。手写的代价是漂移，这条测试就是用来消掉那个代价的。
 */
import path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  FROZEN_LEGACY_SNAPSHOT_CATALOG_IDS,
  NON_PUBLISHABLE_CATALOG_IDS,
  NON_RELEASE_CATALOG_IDS,
  assertPublishableCatalogId,
} from "./catalog-lifecycle.js";
import { publishCatalogRelease } from "./catalog-publisher.js";

/**
 * 工具是 `.mjs`（CI 里直接 `node` 跑，没有构建步骤），所以这里显式声明它的形状。
 * 声明漂了不会静默通过：下面每条断言都真的调用它，形状不对当场炸。
 */
type CatalogEntry = {
  directory: string;
  path: string;
  catalogId: string;
  catalogVersion: string;
  gameId: string;
  displayName: string;
  manifest: Record<string, unknown>;
  shipped: { displayName: string; admission: string } | null;
};
type LifecycleBuckets = {
  shipped: CatalogEntry[];
  synthetic: CatalogEntry[];
  frozen: CatalogEntry[];
  unclassified: CatalogEntry[];
};
type ReleasePlan = {
  schemaVersion: string;
  manifestSchemaVersion: string;
  signed: boolean;
  published: boolean;
  releases: Array<{
    gameId: string;
    catalogId: string;
    payloads: Array<{ kind: string; catalogId: string; version: string; contentHash: string }>;
  }>;
};
type LifecycleTool = {
  readCatalogLifecycleInventory: () => LifecycleBuckets;
  nonPublishableCatalogIds: (buckets: LifecycleBuckets) => string[];
  buildReleasePlan: (buckets: LifecycleBuckets) => ReleasePlan;
  NON_TRAINABLE_TITLE_PATTERN: RegExp;
};

const lifecycleTool = (await import(
  path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    "../../../tools/check-catalog-lifecycle.mjs",
  )
)) as LifecycleTool;
const {
  readCatalogLifecycleInventory,
  nonPublishableCatalogIds,
  buildReleasePlan,
  NON_TRAINABLE_TITLE_PATTERN,
} = lifecycleTool;

const buckets = readCatalogLifecycleInventory();

describe("catalog directory lifecycle", () => {
  it("classifies every directory as 32 shipped + 1 synthetic + 3 superseded = 36", () => {
    expect(buckets.unclassified.map((entry) => entry.directory)).toEqual([]);
    expect(buckets.shipped).toHaveLength(32);
    expect(buckets.synthetic).toHaveLength(1);
    expect(buckets.frozen).toHaveLength(3);
    expect(
      buckets.shipped.length + buckets.synthetic.length + buckets.frozen.length,
    ).toBe(36);
  });

  it("keeps the deleted local-test bundle deleted", () => {
    const directories = [...buckets.shipped, ...buckets.synthetic, ...buckets.frozen].map(
      (entry) => entry.directory,
    );
    expect(directories).not.toContain("yuanshen-ungranted");
  });

  it("puts every active bundle on the neutral schema and every frozen one on legacy", () => {
    for (const entry of [...buckets.shipped, ...buckets.synthetic]) {
      expect(entry.manifest.schema_version).toBe("dm-catalog-bundle-3");
      expect(entry.manifest).not.toHaveProperty("non_trainable");
    }
    // 冻结快照**必须**还带旧字段：它们存在的全部意义就是那批原字节。把它们"顺手
    // 迁移"和把活跃 bundle 停在旧 schema 一样是缺陷，只是方向相反。
    for (const entry of buckets.frozen) {
      expect(entry.manifest.schema_version).toBe("dm-catalog-bundle-2");
      expect(entry.manifest.non_trainable).toBe(true);
    }
  });

  it("gives all 33 active bundles a title that clears the 0028 guard", () => {
    const active = [...buckets.shipped, ...buckets.synthetic];
    expect(active).toHaveLength(33);
    for (const entry of active) {
      expect(NON_TRAINABLE_TITLE_PATTERN.test(entry.displayName)).toBe(false);
    }
  });
});

describe("the publisher refuses what must never be released", () => {
  it("matches the runtime deny-list to the directories on disk", () => {
    expect([...NON_PUBLISHABLE_CATALOG_IDS].sort()).toEqual(nonPublishableCatalogIds(buckets));
    expect([...FROZEN_LEGACY_SNAPSHOT_CATALOG_IDS].sort()).toEqual(
      buckets.frozen.map((entry) => entry.catalogId).sort(),
    );
    expect([...NON_RELEASE_CATALOG_IDS]).toEqual(
      buckets.synthetic.map((entry) => entry.catalogId),
    );
  });

  it("names the reason, because the fix differs per class", () => {
    expect(() =>
      assertPublishableCatalogId("dataminer-bioshock-infinite-steam-release-test"),
    ).toThrow(/superseded research snapshot/);
    expect(() => assertPublishableCatalogId("dataminer-synthetic-dev")).toThrow(
      /E2E\/regression baseline/,
    );
    for (const entry of buckets.shipped) {
      expect(() => assertPublishableCatalogId(entry.catalogId)).not.toThrow();
    }
  });

  it("refuses before it ever reads the roster", async () => {
    // 守卫必须在读库之前判死：一次"发布 synthetic"的请求不该先查一轮 roster 才被
    // 发现，否则错误信息会先指向 roster 的形状问题。
    let queried = false;
    await expect(
      publishCatalogRelease(
        async () => {
          queried = true;
          return { rows: [] };
        },
        {
          channel: "dev",
          catalogId: "dataminer-synthetic-dev",
          catalogVersion: "2026-08-25.0",
          actor: "lifecycle-test@example.com",
          trustedCatalogVersionIds: ["a", "b", "c", "d"],
        },
        { validate: async () => undefined },
      ),
    ).rejects.toThrow(/E2E\/regression baseline/);
    expect(queried).toBe(false);
  });
});

describe("the pending-signature release plan", () => {
  it("covers exactly the 32 shipped games and nothing else", () => {
    const plan = buildReleasePlan(buckets);
    expect(plan.releases).toHaveLength(32);
    // 本 PR 只生成产物，不签不发（issue #855 PR 2 最后一条 checklist）。
    expect(plan.signed).toBe(false);
    expect(plan.published).toBe(false);
    expect(plan.manifestSchemaVersion).toBe("dm-catalog-bundle-3");
    const ids = plan.releases.map((release) => release.catalogId);
    expect([...ids].sort()).toEqual(buckets.shipped.map((entry) => entry.catalogId).sort());
    for (const refused of NON_PUBLISHABLE_CATALOG_IDS) {
      expect(ids).not.toContain(refused);
    }
    for (const release of plan.releases) {
      // 每份至少四本册子：device / recording / game / mapping（docs/57 §3.1）。
      const kinds = new Set(release.payloads.map((payload) => payload.kind));
      expect([...kinds]).toEqual(
        expect.arrayContaining([
          "device_profile",
          "recording_profile",
          "game_profile",
          "game_mapping",
        ]),
      );
    }
  });
});

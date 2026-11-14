/**
 * 目录生命周期里那条**发布侧**的闸（issue #855 §1、§A.9）。
 *
 * `config/catalogs/dev/` 下的 34 份目录分四类，其中两类永远不进正式 release：
 *
 * - 三份带 `SUPERSEDED.md` 的研究快照 —— 冻结在 legacy schema、保持原字节。它们
 *   存在的全部意义就是那批原字节；发布它们等于把一份已被取代的配置重新变成活的。
 * - `synthetic` —— E2E / 回归基线，不是游戏。它可以注册、可以内嵌、可以被测试消费，
 *   就是不该出现在下发给采集机的正式目录里。
 *
 * 两类都**能**被注册进 `trusted_catalog_versions`（登记不等于发布，也不该等于），
 * 所以只靠"名单里有行 id"挡不住它们——发布这一步必须自己认得它们。
 *
 * 为什么是手写常量而不是扫目录：这段代码跑在 Cloud Run 的摄取 API 里，那里没有
 * `config/catalogs/dev/`（只有 job 挂载的 `/app/catalogs/dev`），扫盘会让守卫的
 * 有无取决于部署形态。手写的代价是漂移，而漂移由 `catalog-lifecycle.test.ts`
 * 按仓库真实目录逐条对账消掉：多一份冻结快照而没加进来，测试当场红。
 */

/** 三份 `SUPERSEDED.md` 研究快照的 catalog id。 */
export const FROZEN_LEGACY_SNAPSHOT_CATALOG_IDS = [
  "dataminer-bioshock-infinite-steam-release-test",
  "dataminer-halo-mcc-steam-release-test",
  "dataminer-wolfenstein2-steam-release-test",
] as const;

/** 活跃但不进正式 release 的目录：今天只有 synthetic 基线。 */
export const NON_RELEASE_CATALOG_IDS = ["dataminer-synthetic-dev"] as const;

export const NON_PUBLISHABLE_CATALOG_IDS: readonly string[] = [
  ...FROZEN_LEGACY_SNAPSHOT_CATALOG_IDS,
  ...NON_RELEASE_CATALOG_IDS,
];

const FROZEN = new Set<string>(FROZEN_LEGACY_SNAPSHOT_CATALOG_IDS);
const NON_RELEASE = new Set<string>(NON_RELEASE_CATALOG_IDS);

/**
 * 发布前点名。理由写进错误里而不是笼统说"不允许"：两类的补救动作完全不同——
 * 冻结快照该换成它的取代者，synthetic 该改用真实游戏目录。
 */
export function assertPublishableCatalogId(catalogId: string): void {
  if (FROZEN.has(catalogId)) {
    throw new Error(
      `catalog ${catalogId} is a superseded research snapshot frozen on the legacy schema ` +
        "and must never be published (issue #855 §A.9); publish its replacement instead",
    );
  }
  if (NON_RELEASE.has(catalogId)) {
    throw new Error(
      `catalog ${catalogId} is an E2E/regression baseline, not a shipped game, and does not ` +
        "belong in a catalog release (issue #855 §1)",
    );
  }
}

/**
 * 目录 schema 的两代，以及新旧并存期的兼容桥（issue #855 PR 1）。
 *
 * ## 为什么需要这一层
 *
 * 断代前，`non_trainable` 不只是"数据文件里有这个字段"：manifest、game/device/
 * recording profile 四种载荷都把它定义成**必填不变量**，服务端注册器、签名发布器、
 * Rust 客户端解析器和 `catalog-validator` 全都据此 fail-closed。所以不能先改纸面
 * schema、再改 31 份 JSON、最后修客户端——中间任何一步都会当场炸掉注册、签名、
 * 解析和 `dev-catalog` CI（issue #855 §5）。
 *
 * 本模块是那座桥：把"这份载荷属于哪一代"从各个读者里抽出来，变成一张表加两个断言。
 * **它只回答代际，不回答信任**——信任仍然由签名、content hash 与交叉引用回答。
 *
 * ## 两代的差别只有一件事
 *
 * | | legacy | neutral |
 * |---|---|---|
 * | manifest | `dm-catalog-bundle-2` | `dm-catalog-bundle-3` |
 * | device profile | `dm-device-profile-2` | `dm-device-profile-3` |
 * | recording profile | `dm-recording-profile-1` | `dm-recording-profile-2` |
 * | game profile | `dm-game-profile-2` | `dm-game-profile-3` |
 * | `non_trainable` | 必填 | **必须缺席** |
 *
 * `mapping.json` 从来没有这个字段，因此没有代际，`dm-mapping-2` 两边通用。
 *
 * 中性一代**不是**"该字段改成可选"。可选意味着一份新 schema 的 bundle 可以再次
 * 携带它，于是旧语义借尸还魂：下游读者读到一个它本不该消费的布尔，而 authoring
 * 侧会以为自己仍然在用它表达训练资格。所以规则是**带了就直接拒绝**
 * （issue #855 PR 1 第 1 条）。
 *
 * ## 代际必须整份一致
 *
 * 一份 bundle 的 manifest 与四本 profile 必须同代。半迁移的 bundle（manifest 已升
 * 中性、game profile 还带旧字段）是最坏的一种状态：它在某些读者眼里合法、在另一些
 * 眼里非法，而错误信息不会指向真正的原因。`assertConsistentGeneration` 在
 * authoring 期就把它判死。
 */

export type CatalogSchemaGeneration = "legacy" | "neutral";

/** 携带过 `non_trainable` 的四种载荷。`game_mapping` 从未携带，故不在此表。 */
export type VersionedCatalogPayloadKind =
  | "manifest"
  | "device_profile"
  | "recording_profile"
  | "game_profile";

/** 中性一代必须缺席的 authored 字段。 */
export const RETIRED_TRAINING_FIELD = "non_trainable";

export const CATALOG_SCHEMA_VERSIONS: Record<
  VersionedCatalogPayloadKind,
  Record<CatalogSchemaGeneration, string>
> = {
  manifest: { legacy: "dm-catalog-bundle-2", neutral: "dm-catalog-bundle-3" },
  device_profile: { legacy: "dm-device-profile-2", neutral: "dm-device-profile-3" },
  recording_profile: { legacy: "dm-recording-profile-1", neutral: "dm-recording-profile-2" },
  game_profile: { legacy: "dm-game-profile-2", neutral: "dm-game-profile-3" },
};

/** 代际无关的载荷 schema：从来不带训练/环境词，两代共用。 */
export const GENERATION_NEUTRAL_SCHEMA_VERSIONS = {
  game_mapping: "dm-mapping-2",
} as const;

export type ManifestSchemaVersion =
  (typeof CATALOG_SCHEMA_VERSIONS)["manifest"][CatalogSchemaGeneration];

const PAYLOAD_LABEL: Record<VersionedCatalogPayloadKind, string> = {
  manifest: "catalog manifest",
  device_profile: "device profile",
  recording_profile: "recording profile",
  game_profile: "game profile",
};

/**
 * 这份载荷属于哪一代。未知 schema 直接抛——沉默地当成某一代，正是让半迁移
 * bundle 混进来的那条路。
 */
export function catalogSchemaGeneration(
  kind: VersionedCatalogPayloadKind,
  schemaVersion: unknown,
): CatalogSchemaGeneration {
  const table = CATALOG_SCHEMA_VERSIONS[kind];
  if (schemaVersion === table.legacy) return "legacy";
  if (schemaVersion === table.neutral) return "neutral";
  throw new Error(
    `unsupported ${PAYLOAD_LABEL[kind]} schema ${JSON.stringify(schemaVersion)}; ` +
      `expected ${table.legacy} (legacy) or ${table.neutral} (neutral)`,
  );
}

export function isNeutralManifestSchema(schemaVersion: unknown): boolean {
  return schemaVersion === CATALOG_SCHEMA_VERSIONS.manifest.neutral;
}

/**
 * 训练词表的代际闸。两个方向各挡一种错：
 *
 * - legacy 一代**必须**显式带布尔 `non_trainable`。缺失不是"中性"——一份自称
 *   `dm-catalog-bundle-2` 却不带该字段的载荷是损坏，不是断代。
 * - 中性一代**必须**不带。带了就是旧语义借尸还魂，直接拒绝。
 *
 * 返回 legacy 一代 authored 的那个布尔；中性一代返回 `null`，表示**这份 bundle
 * 不再声明训练分类**——不是"声明为可训练"。
 */
export function readDeclaredNonTrainable(
  kind: VersionedCatalogPayloadKind,
  generation: CatalogSchemaGeneration,
  payload: Record<string, unknown>,
  origin: string,
): boolean | null {
  const declared = payload[RETIRED_TRAINING_FIELD];
  if (generation === "legacy") {
    if (typeof declared !== "boolean") {
      throw new Error(
        `${origin} (${PAYLOAD_LABEL[kind]} ${CATALOG_SCHEMA_VERSIONS[kind].legacy}) must declare an explicit boolean ${RETIRED_TRAINING_FIELD}`,
      );
    }
    return declared;
  }
  if (RETIRED_TRAINING_FIELD in payload) {
    throw new Error(
      `${origin} (${PAYLOAD_LABEL[kind]} ${CATALOG_SCHEMA_VERSIONS[kind].neutral}) must not carry ${RETIRED_TRAINING_FIELD}; ` +
        "the neutral catalog schema does not express training classification (issue #855 §A)",
    );
  }
  return null;
}

/**
 * 整份 bundle 同代。半迁移是最坏状态：部分读者放行、部分读者拒绝，而错误信息
 * 指不到真正的原因。
 */
export function assertConsistentGeneration(
  manifestGeneration: CatalogSchemaGeneration,
  payloads: ReadonlyArray<{ kind: VersionedCatalogPayloadKind; generation: CatalogSchemaGeneration }>,
  origin: string,
): void {
  for (const payload of payloads) {
    if (payload.generation !== manifestGeneration) {
      throw new Error(
        `${origin} mixes catalog schema generations: manifest is ${manifestGeneration} ` +
          `but ${PAYLOAD_LABEL[payload.kind]} is ${payload.generation}; ` +
          "a bundle migrates as one unit (issue #855 §5)",
      );
    }
  }
}

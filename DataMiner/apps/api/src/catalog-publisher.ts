import { createHash, randomUUID } from "node:crypto";

import { assertPublishableCatalogId } from "./catalog-lifecycle.js";
import {
  CATALOG_SCHEMA_VERSIONS,
  type ManifestSchemaVersion,
} from "./catalog-schema.js";
import { ClientSourceCatalogValidator } from "./catalog-validator.js";
import {
  PostgresCatalogReleaseSource,
  assembleSignedBundle,
  serializeSignedBundle,
  signedBundleEtag,
  type CatalogManifest,
  type CatalogManifestCategory,
  type CatalogQuery,
  type CatalogValidator,
} from "./signed-catalog.js";

/**
 * 发布渠道。
 *
 * 单值。certified 采集车道随 docs/100 从录制侧删除（PR #578），发布侧随
 * issue #829 收口（docs/100 §11）：它曾经是一条服务端仍然发得出、却没有任何
 * 客户端装得上的幽灵车道——客户端只钉了 `DEV_CATALOG_ANCHOR` 一把公钥
 * （`catalog.rs`），certified 信封的验签必然失败。
 *
 * 留成类型而不是删掉整个字段：`catalog_releases.channel` 那一列还在（词表收敛
 * 是独立的一步，issue #829 §5），而**显式**发 certified 的调用方必须拿到一条
 * 指名道姓的拒绝，见下面 `publishCatalogRelease` 的入口守卫。
 */
export type CatalogChannel = "dev";

/**
 * 发布出来的 manifest 用哪一代 schema（issue #855 PR 1 第 3 条）。
 *
 * **默认必须留在 legacy。** 客户端是先发布、后升级的：PR 1 只让服务端**会写**新
 * 格式，切换 current 是 PR 4 在确认兼容客户端已经铺开之后的事。部署 PR 1 就静默改变
 * 旧客户端拉到的字节，等于在没有回滚客户端的情况下换掉下发格式——那正是 issue 明令
 * 禁止的一步。
 */
export const DEFAULT_PUBLISHED_MANIFEST_SCHEMA: ManifestSchemaVersion =
  CATALOG_SCHEMA_VERSIONS.manifest.legacy;

const PUBLISHABLE_MANIFEST_SCHEMAS: readonly ManifestSchemaVersion[] = [
  CATALOG_SCHEMA_VERSIONS.manifest.legacy,
  CATALOG_SCHEMA_VERSIONS.manifest.neutral,
];

export type CatalogPublishRequest = {
  channel: CatalogChannel;
  /**
   * 省略即 `DEFAULT_PUBLISHED_MANIFEST_SCHEMA`（legacy）。显式传中性版本才产出
   * 中性 manifest——发布新格式是一次有意的动作，不是升级依赖的副作用。
   */
  manifestSchemaVersion?: ManifestSchemaVersion;
  /**
   * 这一份 bundle 的目录 id。断代前是「每渠道一份大目录」，id 由渠道派生出
   * 两个舰队常量；docs/57 §3.3 之后是**每游戏一份 bundle**，id 因此必须由
   * 调用方给出（它就是 authored bundle 目录里 catalog.json 的 catalog_id）。
   */
  catalogId: string;
  catalogVersion: string;
  actor: string;
  trustedCatalogVersionIds: string[];
};

export type CatalogPublishResult = {
  releaseId: string;
  catalogId: string;
  catalogVersion: string;
  signingKeyId: string;
  etag: string;
};

type TrustedRow = {
  id: string;
  kind: string;
  catalog_id: string;
  version: string;
  content_hash: string;
  status: string;
  approved_for_trainable: boolean;
};

const CATEGORY_FOR_KIND: Record<string, CatalogManifestCategory> = {
  device_profile: "device_profiles",
  recording_profile: "recording_profiles",
  game_profile: "game_profiles",
  game_mapping: "mappings",
  depth_profile: "depth_profiles",
  camera_binding: "camera_bindings",
};

const OPTIONAL_KINDS = new Set(["depth_profile", "camera_binding"]);

function requiredText(value: string, name: string): string {
  const trimmed = value.trim();
  if (!trimmed) throw new Error(`${name} is required`);
  return trimmed;
}

/**
 * 这次发布产出哪一代 manifest。省略即 legacy。
 *
 * 运行时守卫，不是类型的重复：roster 有一条 `JSON.parse(...) as CatalogPublishRequest`
 * 的路（publish-catalog.ts），类型在那里挡不住任何东西。
 */
export function publishedManifestSchema(
  request: Pick<CatalogPublishRequest, "manifestSchemaVersion">,
): ManifestSchemaVersion {
  const schemaVersion = request.manifestSchemaVersion ?? DEFAULT_PUBLISHED_MANIFEST_SCHEMA;
  if (!PUBLISHABLE_MANIFEST_SCHEMAS.includes(schemaVersion)) {
    throw new Error(
      `unsupported manifest schema version: ${schemaVersion}; expected one of ${PUBLISHABLE_MANIFEST_SCHEMAS.join(", ")}`,
    );
  }
  return schemaVersion;
}

function placeholders(count: number): string {
  return Array.from({ length: count }, (_, index) => `$${index + 1}`).join(", ");
}

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function manifestFor(
  request: CatalogPublishRequest,
  rows: TrustedRow[],
): { bytes: Buffer; entries: Array<TrustedRow & { ordinal: number }> } {
  const schemaVersion = publishedManifestSchema(request);
  const manifest: CatalogManifest = {
    schema_version: schemaVersion,
    catalog_id: requiredText(request.catalogId, "catalogId"),
    catalog_version: requiredText(request.catalogVersion, "catalogVersion"),
    // manifest 不再自称 mode（docs/57 §3.3）——客户端的信任锚由验签的那把公钥
    // 决定。legacy 一代仍写 `non_trainable: true`：dev 成为唯一渠道后它是恒真的
    // 不变量，assembleSignedBundle 仍会拿它对账。中性一代**整个字段不写**
    // （issue #855 §A.4）——写成 `undefined` 也不行，JSON.stringify 会把
    // undefined 的键整个丢掉，但显式删除让意图留在代码里而不是留在序列化行为里。
    ...(schemaVersion === CATALOG_SCHEMA_VERSIONS.manifest.legacy
      ? { non_trainable: true as const }
      : {}),
    device_profiles: [],
    recording_profiles: [],
    game_profiles: [],
    mappings: [],
    depth_profiles: [],
    camera_bindings: [],
  };
  const entries: Array<TrustedRow & { ordinal: number }> = [];

  for (const [kind, category] of Object.entries(CATEGORY_FOR_KIND)) {
    const selected = rows
      .filter((row) => row.kind === kind)
      .sort(
        (left, right) =>
          compareText(left.catalog_id, right.catalog_id) ||
          compareText(left.version, right.version),
      );
    if (selected.length === 0 && !OPTIONAL_KINDS.has(kind)) {
      throw new Error(`release roster is missing required kind ${kind}`);
    }
    for (const [ordinal, row] of selected.entries()) {
      manifest[category].push({
        id: row.catalog_id,
        version: row.version,
        content_hash: row.content_hash,
      });
      entries.push({ ...row, ordinal });
    }
  }
  return { bytes: Buffer.from(JSON.stringify(manifest), "utf8"), entries };
}

/**
 * Publish an explicit trusted-row roster. Selection never uses MAX(version).
 * Validation and signing finish before the immutable draft and atomic current
 * pointer switch are committed in one database transaction.
 */
export async function publishCatalogRelease(
  query: CatalogQuery,
  request: CatalogPublishRequest,
  validator: CatalogValidator = new ClientSourceCatalogValidator(),
): Promise<CatalogPublishResult> {
  // 运行时守卫，不是类型的重复：这个 request 有一条路是 `JSON.parse(roster) as
  // CatalogPublishRequest`（publish-catalog.ts），类型在那里挡不住任何东西。
  //
  // 退役的渠道**点名拒绝**，不静默当 dev——照 docs/100 §10.1 第 2 条的先例。
  // 一个仍然发 certified 的调用方期待的是 KMS 那把已退役的锚签出来的字节；
  // 悄悄发它一份 dev 信封，它只会死在一句 signature verification failed 上，
  // 排障顺着签名去查，而真相是那条渠道整条没有了。
  const channel: string = request.channel;
  if (channel !== "dev") {
    throw new Error(
      channel === "certified"
        ? "the certified catalog channel was retired (docs/100); dev is the only channel"
        : `unsupported catalog channel: ${channel}`,
    );
  }
  // 在读库之前判死：一次发布请求的形状错误不该先花一轮 roster 查询才被发现。
  publishedManifestSchema(request);
  // 生命周期闸（issue #855 §1）：三份 SUPERSEDED 研究快照与 synthetic 基线可以登记、
  // 可以内嵌、可以被测试消费，就是不得发布。登记与发布是两件事，只靠"名单里有行 id"
  // 挡不住它们——发布这一步必须自己认得。
  assertPublishableCatalogId(requiredText(request.catalogId, "catalogId"));
  const actor = requiredText(request.actor, "actor");
  if (!Array.isArray(request.trustedCatalogVersionIds)) {
    throw new Error("trustedCatalogVersionIds must be an explicit array");
  }
  const ids = [...new Set(request.trustedCatalogVersionIds)];
  // 四本册子（docs/57 §3.1）：device / recording / game / mapping 各至少一行。
  // 下面 manifestFor 还会逐类检查缺失，这里挡住的是「连数量都不够」的名单。
  if (ids.length !== request.trustedCatalogVersionIds.length || ids.length < 4) {
    throw new Error("release roster must contain at least four unique trusted row ids");
  }
  const selected = await query(
    `SELECT id, kind::text, catalog_id, version, content_hash, status::text,
            approved_for_trainable
       FROM trusted_catalog_versions
      WHERE id IN (${placeholders(ids.length)})`,
    ids,
  );
  if (selected.rows.length !== ids.length) {
    throw new Error("release roster contains an unknown trusted catalog version id");
  }
  const rows = selected.rows as TrustedRow[];
  if (rows.some((row) => row.status === "revoked")) {
    throw new Error("release roster contains a revoked trusted catalog version");
  }
  const builtManifest = manifestFor(request, rows);
  const source = new PostgresCatalogReleaseSource(query, builtManifest.bytes);
  const bundle = await assembleSignedBundle(source, validator);
  const envelope = serializeSignedBundle(bundle);
  const envelopeHash = `sha256:${createHash("sha256").update(envelope).digest("hex")}`;
  const parsedManifest = JSON.parse(builtManifest.bytes.toString("utf8")) as CatalogManifest;
  const releaseId = randomUUID();

  await query("BEGIN");
  try {
    // 经 SECURITY DEFINER 函数加锁（迁移 0020），不要在这里直接写 `FOR SHARE`：
    // 行锁子句要求 UPDATE 级权限，而 publisher 角色对这张表只该有 SELECT——它
    // 可以钉住这些行，绝不该能改写它们。锁仍然记在本事务上，语义不变。
    // 第二个参数是「要求 approved」：只有 certified 曾经要求它，dev 从不。
    // 渠道退成单值后它恒 false。函数签名属发布面 SQL，随词表收敛那一步再动
    // （issue #829 §4 第二步）。
    const locked = await query(
      `SELECT lock_publishable_catalog_versions($1::uuid[], $2::boolean) AS id`,
      [ids, false],
    );
    if (locked.rows.length !== ids.length) {
      throw new Error("release roster changed eligibility while it was being built");
    }
    await query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version,
         manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id, created_by
       ) VALUES ($1, $2, $3, $4, $5::bytea, $6::bytea, $7, $8, $9)`,
      [
        releaseId,
        request.channel,
        parsedManifest.catalog_id,
        parsedManifest.catalog_version,
        builtManifest.bytes,
        envelope,
        envelopeHash,
        bundle.signingKeyId,
        actor,
      ],
    );
    for (const entry of builtManifest.entries) {
      await query(
        `INSERT INTO catalog_release_entries(
           release_id, trusted_catalog_version_id, kind, ordinal
         ) VALUES ($1, $2, $3::catalog_kind, $4)`,
        [releaseId, entry.id, entry.kind, entry.ordinal],
      );
    }
    await query(`SELECT publish_catalog_release($1, $2)`, [releaseId, actor]);
    await query("COMMIT");
  } catch (error) {
    await query("ROLLBACK");
    throw error;
  }

  return {
    releaseId,
    catalogId: parsedManifest.catalog_id,
    catalogVersion: parsedManifest.catalog_version,
    signingKeyId: bundle.signingKeyId,
    etag: signedBundleEtag(envelope),
  };
}

/**
 * 解析 `publish:catalog` 的命令行，取出 roster 文件路径。
 *
 * 放在这里而不是脚本里，只为一件事：脚本是顶层 await + 连库的，测试无法导入它。
 * 而这段解析**恰恰需要测试**——它曾经让 docs/36 记的那条命令 100% 失败：
 *
 *   pnpm --filter @dataminer/api publish:catalog -- ./roster.json
 *
 * pnpm 把 `--` 分隔符原样透传，脚本收到的是 `[node, script, '--', roster]`：
 * argv[2] 是 `--`、长度为 4，而旧代码取 argv[2] 并要求长度恰好为 3，于是必然
 * 抛 usage 错误。register-catalog.ts 用按名取值的解析（argv.includes），多一个
 * `--` 无所谓，所以同样带 `--` 的 register 命令一直是好的——只有这个严格按长度
 * 校验的炸了，且因为没有任何测试执行过这层 CLI、下发链路也从未在生产上跑过，
 * 一直没人发现。
 *
 * 仍然坚持"恰好一个参数"：发布是不可逆动作，多出来的杂参应当拒绝而不是猜。
 */
export function rosterPathFromArgv(argv: readonly string[]): string {
  const args = argv.slice(2).filter((arg) => arg !== "--");
  if (args.length !== 1 || !args[0]) {
    throw new Error("usage: pnpm publish:catalog -- <release-roster.json>");
  }
  return args[0];
}

import { createHash } from "node:crypto";

/**
 * 签名 catalog 下发的响应契约（docs/37）。
 *
 * 这份逻辑有**两个**调用点：摄取面的 `GET /v1/catalog`（设备凭据鉴权）与 console
 * 面的 `GET /app/v1/catalog/bootstrap`（标注员会话鉴权，绑定前用）。两条路只在
 * **谁可以调**上不同；ETag、schema/客户端版本兼容判定、`If-None-Match` 语义、
 * 返回哪些响应头、返回的是不是原样字节，必须逐字一致——否则「绑定前能拿到、
 * 绑定后拿不到」这类漂移会以最难查的方式出现在真机上。
 *
 * 所以判定放在这里、与 HTTP 框架无关：调用方只把 release 与请求头交进来，拿到
 * 一个已经决定好的结果，再翻译成自己的 reply。两边不可能各写一份。
 *
 * 这个模块**不做鉴权**。谁有资格拿到 release 由各自的调用点决定，这是两条路唯一
 * 允许不同的地方。
 */

/**
 * 2 = 集合下发（docs/57 §3.3 批次二）。断代前每渠道只有一份大目录，这条路返回
 * 那一份 envelope 的原样字节；每游戏一份 bundle 之后，返回的是该渠道已发布且
 * 当前客户端可以消费的 bundle 集合。
 *
 * 升这个数字是本次断代的关键一步，不是记账：一个只声明 `Schema-Max: 1` 的旧
 * 客户端会在下面的兼容门上拿到 409，而不是收到一份它解析不了的集合体。这道门
 * 本来就是为这种形状变更准备的。
 */
export const CATALOG_SCHEMA_VERSION = 2;
export const MIN_CATALOG_CLIENT_VERSION = [0, 5, 0] as const;
// 0.7.20 rejects the entire set if it encounters KCD2's profile-v3/combined
// binding. This is a per-catalog consumer requirement, not a global upgrade gate.
const CATALOG_MIN_CLIENT_VERSIONS: ReadonlyMap<string, readonly [number, number, number]> = new Map([
  ["dataminer-kingdom-come-deliverance-2-steam", [0, 7, 21]],
]);
const KCD2_COMBINED_V2_MIN_CLIENT_VERSION = [0, 7, 24] as const;
// The first CP2077 release carrying its depth pair (2026-09-18.0) uses the holstered-v3 camera
// binding that 0.7.32 and older do not know; like 0.7.20 with KCD2 they would reject the whole
// set. RGB-only revisions before it stay consumable by every client.
const CP2077_DEPTH_PAIR_MIN_CLIENT_VERSION = [0, 7, 33] as const;

/** The date of a canonical `YYYY-MM-DD.N` catalog version, or undefined for anything else. */
function canonicalCatalogDate(catalogVersion: string): string | undefined {
  const date = /^(\d{4}-\d{2}-\d{2})\.(?:0|[1-9]\d*)$/.exec(catalogVersion)?.[1];
  const timestamp = date === undefined ? Number.NaN : Date.parse(`${date}T00:00:00Z`);
  const canonical = Number.isFinite(timestamp) && new Date(timestamp).toISOString().slice(0, 10) === date;
  return canonical ? date : undefined;
}

function minimumClientVersion(release: CatalogReleaseForDelivery): readonly [number, number, number] | undefined {
  // Deploy compatibility handling before replacing the current catalog: a prior release remains
  // consumable by old clients until the new one actually becomes current. Unknown revisions
  // cannot opt old clients into a source schema they may not understand.
  const date = canonicalCatalogDate(release.catalogVersion);
  if (release.catalogId === "dataminer-cyberpunk-2077-steam-dev") {
    return date !== undefined && date < "2026-09-18" ? undefined : CP2077_DEPTH_PAIR_MIN_CLIENT_VERSION;
  }
  const minimum = CATALOG_MIN_CLIENT_VERSIONS.get(release.catalogId);
  if (release.catalogId !== "dataminer-kingdom-come-deliverance-2-steam") return minimum;
  if (date !== undefined && date < "2026-09-11") return minimum;
  return KCD2_COMBINED_V2_MIN_CLIENT_VERSION;
}
export const CATALOG_VARY =
  "Authorization, X-DataMiner-Catalog-Schema-Max, X-DataMiner-Client-Version";

/**
 * 下发渠道。
 *
 * certified 车道废除后（docs/100 §4.3）只剩一条渠道：客户端不再发
 * `X-DataMiner-Catalog-Channel`，服务端也不再按它**路由**。渠道仍是类型层面的
 * 概念（`catalog_releases.channel` 那一列没动），只是每次下发都取部署渠道。
 *
 * 头的名字仍然导出：`app.ts` 用它挡住一个仍然声明 `certified` 的旧客户端
 * （docs/100 §7）——那种客户端 pin 的是已退役的 KMS 锚，静默发它 dev 字节只会
 * 让它以一句 "signature verification failed" 倒下，排障方向完全错了。
 */
export const CATALOG_CHANNEL_HEADER = "x-dataminer-catalog-channel";

export type CatalogChannel = "dev" | "certified";

/** 一条已发布的 release，只含下发需要的字段。 */
export type CatalogReleaseForDelivery = {
  catalogId: string;
  catalogVersion: string;
  envelopeBytes: Buffer;
  envelopeSha256: string;
};

/**
 * 取渠道当前 release 的查询。两个服务共用同一份常量：摄取面以 `dataminer_console`
 * 派生的摄取角色跑它，console 面以 `dataminer_app` 跑它。`status = 'published'`
 * 这个谓词是两条路都不能少的那一条——少了它，草稿 release 会被当成当前的发出去。
 */
export const currentCatalogReleaseSql = `SELECT catalog_id, catalog_version, envelope_bytes, envelope_sha256
         FROM catalog_releases
        WHERE channel = $1
          AND status = 'published'`;

type DeliveryHeaders = Record<string, string | string[] | undefined>;

export type CatalogDeliveryResult =
  | { kind: "not_published"; headers: Record<string, string> }
  | { kind: "incompatible"; reason: string; headers: Record<string, string> }
  | { kind: "not_modified"; headers: Record<string, string> }
  | { kind: "ok"; headers: Record<string, string>; body: Buffer };

/**
 * 判定一次下发请求的结果。顺序与 docs/37 一致：先看有没有发布，再看客户端能不能
 * 消费，最后才是条件请求。
 *
 * 兼容性检查刻意排在 `If-None-Match` **之前**：一个版本太老的客户端即使 ETag 命中
 * 也必须收到 409 而不是 304，否则它会以为自己手上那份缓存仍然当前。
 */
export function resolveCatalogDelivery(
  releases: readonly CatalogReleaseForDelivery[],
  headers: DeliveryHeaders,
): CatalogDeliveryResult {
  if (releases.length === 0) {
    return {
      kind: "not_published",
      headers: { "cache-control": "private, no-cache", vary: CATALOG_VARY },
    };
  }

  const incompatibility = catalogIncompatibility(headers);
  const clientVersion = incompatibility === null
    ? parseClientVersion(headers["x-dataminer-client-version"] as string)
    : null;
  const consumable = clientVersion === null ? releases : releases.filter((release) => {
    const minimum = minimumClientVersion(release);
    return minimum === undefined || compareVersion(clientVersion, minimum) >= 0;
  });
  if (consumable.length === 0) {
    return {
      kind: "not_published",
      headers: { "cache-control": "private, no-cache", vary: CATALOG_VARY },
    };
  }

  // 先过滤可消费目录，再按 catalog_id 定序。ETag 与响应体都必须与库里的行序无关——否则同一份集合
  // 会因为一次无关的 VACUUM 换个 ETag，把所有客户端的缓存一起打穿。
  const ordered = [...consumable].sort((left, right) =>
    left.catalogId < right.catalogId ? -1 : left.catalogId > right.catalogId ? 1 : 0,
  );
  const body = serializeCatalogSet(ordered);
  const etag = catalogSetEtag(body);
  const responseHeaders: Record<string, string> = {
    etag,
    "cache-control": "private, no-cache",
    "x-dataminer-catalog-count": String(ordered.length),
    vary: CATALOG_VARY,
  };

  if (incompatibility) {
    return { kind: "incompatible", reason: incompatibility, headers: responseHeaders };
  }

  if (ifNoneMatchIncludes(headers["if-none-match"], etag)) {
    return { kind: "not_modified", headers: responseHeaders };
  }

  return { kind: "ok", headers: responseHeaders, body };
}

/**
 * 集合体（`dm-signed-catalog-set-1`）。
 *
 * 每份 bundle 以**原样字节的 base64** 嵌入，不做重新序列化：签名覆盖的是
 * manifest 字节，而 envelope 的 sha256 是 registry 承诺过的精确字节，两者都不能
 * 因为一次 JSON 往返而漂移。
 *
 * 集合**整体没有签名**——每份 bundle 各自带签名，客户端逐份验。这意味着中间人
 * 无法伪造或篡改任何一份，但可以**少发**几份。少发的后果是客户端看不到某款游戏，
 * 不是拿到假目录；防回滚的高水位仍然逐 catalog_id 记在客户端侧。把整个集合再签
 * 一次需要一把「集合钥匙」和一套集合级的版本号，那是另一件事，不在批次二里。
 */
export function serializeCatalogSet(releases: readonly CatalogReleaseForDelivery[]): Buffer {
  return Buffer.from(
    JSON.stringify({
      schemaVersion: "dm-signed-catalog-set-1",
      bundles: releases.map((release) => ({
        catalogId: release.catalogId,
        catalogVersion: release.catalogVersion,
        envelopeSha256: release.envelopeSha256,
        envelope: release.envelopeBytes.toString("base64"),
      })),
    }),
    "utf8",
  );
}

export function catalogSetEtag(body: Buffer): string {
  return `"sha256-${createHash("sha256").update(body).digest("hex")}"`;
}

export function catalogReleaseEtag(envelopeSha256: string): string {
  const match = /^sha256:([0-9a-f]{64})$/.exec(envelopeSha256);
  if (!match?.[1]) {
    throw new Error("published catalog release has a non-canonical envelope sha256");
  }
  return `"sha256-${match[1]}"`;
}

/** 返回不兼容的具体原因；`null` 表示这个客户端可以消费当前 release。 */
export function catalogIncompatibility(headers: DeliveryHeaders): string | null {
  const schemaMax = headers["x-dataminer-catalog-schema-max"];
  const clientVersion = headers["x-dataminer-client-version"];
  if (typeof schemaMax !== "string") {
    return "X-DataMiner-Catalog-Schema-Max is required";
  }
  const parsedSchemaMax = Number(schemaMax);
  if (!/^[0-9]+$/.test(schemaMax) || !Number.isSafeInteger(parsedSchemaMax)) {
    return "X-DataMiner-Catalog-Schema-Max must be a non-negative integer";
  }
  if (parsedSchemaMax < CATALOG_SCHEMA_VERSION) {
    return `this client supports catalog envelope schema ${parsedSchemaMax}, but the published release is schema ${CATALOG_SCHEMA_VERSION}`;
  }
  if (typeof clientVersion !== "string") {
    return "X-DataMiner-Client-Version is required";
  }
  const version = parseClientVersion(clientVersion);
  if (version === null) {
    return "X-DataMiner-Client-Version must be MAJOR.MINOR.PATCH";
  }
  if (compareVersion(version, MIN_CATALOG_CLIENT_VERSION) < 0) {
    return `client ${clientVersion} is older than the minimum ${MIN_CATALOG_CLIENT_VERSION.join(".")} required to consume catalog releases`;
  }
  return null;
}

function parseClientVersion(value: string): readonly [number, number, number] | null {
  const match = /^([0-9]+)\.([0-9]+)\.([0-9]+)(?:[-+][0-9A-Za-z.-]+)?$/.exec(value);
  if (!match) return null;
  const parts = match.slice(1, 4).map(Number);
  if (parts.some((part) => !Number.isSafeInteger(part))) return null;
  return [parts[0]!, parts[1]!, parts[2]!];
}

function compareVersion(
  left: readonly [number, number, number],
  right: readonly [number, number, number],
): number {
  for (let index = 0; index < 3; index += 1) {
    if (left[index]! !== right[index]!) return left[index]! - right[index]!;
  }
  return 0;
}

export function ifNoneMatchIncludes(
  header: string | string[] | undefined,
  currentEtag: string,
): boolean {
  const value = Array.isArray(header) ? header.join(",") : header;
  if (!value) return false;
  return value.split(",").some((candidate) => {
    const trimmed = candidate.trim();
    return trimmed === "*" || trimmed.replace(/^W\//, "") === currentEtag;
  });
}

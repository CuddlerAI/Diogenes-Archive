/**
 * issue #855 PR 4 的目录断代编排：按 `config/catalogs/release-plan.json` 逐款注册并
 * 发布中性 schema 的 catalog release，完成 current 切换。
 *
 * 手工做是 30 次 `register:catalog` + 30 次 `publish:catalog` = 60 条生产命令，其中
 * 一条漏参数会**静默**把整次断代发成旧格式（见下面陷阱 ①）。所以有这一层。
 *
 * 它不是第三条实现：注册走 `registerCatalogBundle`、发布走 `publishCatalogRelease`，
 * 与那两条 CLI 是同一段代码。本模块只负责编排、闸门与结论。
 *
 * ## 为什么逻辑在 `src/` 而 CLI 在 `scripts/`
 *
 * 照 `catalog-publisher.ts#rosterPathFromArgv` 的先例：脚本是顶层 await + 连库的，
 * 测试导不进来。那次的代价是 docs/36 记的发布命令 100% 失败却没人发现——因为没有
 * 任何测试执行得到那层 CLI。断代编排比一行 argv 解析危险得多，它必须可被测试直接
 * 驱动。`scripts/cutover-catalog.ts` 因此只剩参数解析、打印与退出码。
 *
 * ## 三个必须处理的陷阱
 *
 * ① **`manifestSchemaVersion` 漏传 = 静默发成旧格式。**
 *    `catalog-publisher.ts` 省略即 `DEFAULT_PUBLISHED_MANIFEST_SCHEMA`（legacy）。
 *    漏传时发布会「成功」，但发出去的是旧格式——客户端契约不翻代，整个断代白做，
 *    且无任何报错。本模块从**计划文件顶层的 `manifestSchemaVersion`** 读它，不硬编码
 *    （硬编码会和计划文件漂移），并且再要求每份 bundle 自身的代际与它一致：计划说
 *    中性、bundle 还停在 legacy，或反过来，都在连库之前判死（`prepareCutover`）。
 *
 * ② **不顺手改治理状态。**
 *    `registerCatalogBundle` 是只升不降的（SQL 里是 `allowed_for_capture OR $3`）。
 *    本模块**从不传** `allowCapture` / `approveTrainable` / `gameTrainable`，而且它们
 *    不做成参数——采集准入与可训练审批都是独立的治理决定，不属于一次格式断代。
 *    `approve`（把 `trusted_catalog_versions.status` 置 approved）是参数、默认关，
 *    并由 `statusParity` 逐份 payload 与前代行对账，不一致默认拒绝执行。
 *
 * ③ **切 current 会掐断正在录制的会话。**
 *    `orchestrator.rs#try_activate_catalog` 上方的注释写着：目录 payload hash 一变，
 *    `preserves_selection` 必然 false，于是 `invalidate_catalog_session(CatalogRetired)`
 *    ——该场 `Faulted`、`retryable: false`、旧 session 禁止恢复。数据不丢（已 ready 的
 *    段保留），但采集员现场看不出原因在服务端。`readCutoverFacts` 因此查在飞会话，
 *    `preflight` 非零默认拒绝。
 */
import { readFile } from "node:fs/promises";
import path from "node:path";

import { assertPublishableCatalogId } from "./catalog-lifecycle.js";
import { publishCatalogRelease } from "./catalog-publisher.js";
import {
  CATALOG_SCHEMA_VERSIONS,
  catalogSchemaGeneration,
  type CatalogSchemaGeneration,
  type ManifestSchemaVersion,
} from "./catalog-schema.js";
import { readCatalogBundle, registerCatalogBundle, type CatalogBundle } from "./register-catalog.js";
import type { CatalogQuery, CatalogValidator } from "./signed-catalog.js";

/** 发布渠道单值（certified 已退役，docs/100 §11）。 */
export const CUTOVER_CHANNEL = "dev" as const;
export const RELEASE_PLAN_SCHEMA_VERSION = "dm-catalog-release-plan-1";

/** 计划与磁盘/库不符时抛这个，调用方按它决定退出码而不是打印一段栈。 */
export class CutoverError extends Error {}

// -- 计划文件 ---------------------------------------------------------------

export type PlannedPayload = {
  kind: string;
  catalogId: string;
  version: string;
  contentHash: string;
  file: string;
};

export type PlannedRelease = {
  gameId: string;
  catalogDir: string;
  catalogId: string;
  catalogVersion: string;
  displayName: string;
  admission: string;
  payloads: PlannedPayload[];
};

export type ReleasePlan = {
  schemaVersion: string;
  manifestSchemaVersion: string;
  releases: PlannedRelease[];
};

export type PreparedRelease = {
  planned: PlannedRelease;
  bundle: CatalogBundle;
  bundleDir: string;
};

export type PreparedCutover = {
  plan: ReleasePlan;
  planPath: string;
  manifestSchemaVersion: ManifestSchemaVersion;
  generation: CatalogSchemaGeneration;
  releases: PreparedRelease[];
};

export type PrepareOptions = {
  planPath: string;
  catalogRoot: string;
  /** 只跑这些 gameId；空数组表示全跑。 */
  only?: readonly string[];
  /** 在 `only` 过滤之后再截前 n 款。 */
  limit?: number | null;
};

/**
 * 计划与磁盘字节的逐条对账。
 *
 * 计划文件由 `tools/check-catalog-lifecycle.mjs` 生成，CI 已经在守它；但这里仍然重做
 * 一遍，因为**发布用的是磁盘字节**：只要有一条路让计划与 bundle 分叉（本地改了 bundle
 * 没跑 `--write`、或 checkout 了半份），照计划报的 hash 组 roster 就会发出一份与计划
 * 不符的目录。register 那层会因为 content_hash 对不上而拒绝，但那时已经在生产库里跑到
 * 一半了。判死要发生在连库之前。
 */
function assertPlanMatchesBundle(planned: PlannedRelease, bundle: CatalogBundle): void {
  const where = `${planned.gameId} (${planned.catalogDir})`;
  if (bundle.catalogId !== planned.catalogId) {
    throw new CutoverError(`${where}: bundle catalog_id ${bundle.catalogId} != plan ${planned.catalogId}`);
  }
  if (bundle.catalogVersion !== planned.catalogVersion) {
    throw new CutoverError(
      `${where}: bundle catalog_version ${bundle.catalogVersion} != plan ${planned.catalogVersion}`,
    );
  }
  if (bundle.game.id !== planned.gameId) {
    throw new CutoverError(`${where}: bundle game_id ${bundle.game.id} != plan ${planned.gameId}`);
  }
  const key = (kind: string, id: string, version: string, hash: string) => `${kind} ${id}@${version} ${hash}`;
  const onDisk = new Set(
    bundle.entries.map((entry) => key(entry.kind, entry.catalogId, entry.version, entry.contentHash)),
  );
  const inPlan = new Set(
    planned.payloads.map((payload) => key(payload.kind, payload.catalogId, payload.version, payload.contentHash)),
  );
  for (const missing of [...inPlan].filter((entry) => !onDisk.has(entry))) {
    throw new CutoverError(`${where}: plan pins ${missing}, which the bundle on disk does not carry`);
  }
  for (const extra of [...onDisk].filter((entry) => !inPlan.has(entry))) {
    throw new CutoverError(`${where}: bundle carries ${extra}, which the plan does not pin`);
  }
}

/** 纯离线：读计划、读 bundle、逐条对账、判代际。不连库，不写任何东西。 */
export async function prepareCutover(options: PrepareOptions): Promise<PreparedCutover> {
  const planPath = path.resolve(options.planPath);
  const plan = JSON.parse(await readFile(planPath, "utf8")) as ReleasePlan;
  if (plan.schemaVersion !== RELEASE_PLAN_SCHEMA_VERSION) {
    throw new CutoverError(
      `release plan schemaVersion is ${plan.schemaVersion}, expected ${RELEASE_PLAN_SCHEMA_VERSION}`,
    );
  }
  // 陷阱 ①：从计划文件读，不硬编码。未知取值在这里就判死——`catalogSchemaGeneration`
  // 认不出的 manifest schema 传到 publisher 那层也会被拒，但那时第一款已经注册完了。
  const manifestSchemaVersion = plan.manifestSchemaVersion as ManifestSchemaVersion;
  let generation: CatalogSchemaGeneration;
  try {
    generation = catalogSchemaGeneration("manifest", manifestSchemaVersion);
  } catch (error) {
    throw new CutoverError(
      `release plan manifestSchemaVersion ${JSON.stringify(plan.manifestSchemaVersion)} is not a known ` +
        `catalog manifest schema (${Object.values(CATALOG_SCHEMA_VERSIONS.manifest).join(", ")}): ` +
        (error instanceof Error ? error.message : String(error)),
    );
  }
  if (!Array.isArray(plan.releases) || plan.releases.length === 0) {
    throw new CutoverError("release plan carries no releases");
  }

  const only = options.only ?? [];
  for (const wanted of only) {
    if (!plan.releases.some((release) => release.gameId === wanted)) {
      throw new CutoverError(`--only ${wanted} is not in the release plan`);
    }
  }
  const selected = plan.releases
    .filter((release) => only.length === 0 || only.includes(release.gameId))
    .slice(0, options.limit ?? undefined);
  if (selected.length === 0) throw new CutoverError("selection is empty");

  const releases: PreparedRelease[] = [];
  for (const planned of selected) {
    // 生命周期闸（issue #855 §1）：synthetic 与三份 SUPERSEDED 快照不得发布。计划文件
    // 本来就不含它们，但 `--plan` 可以指向别的文件，所以这一步不省。
    assertPublishableCatalogId(planned.catalogId);
    const bundleDir = path.join(path.resolve(options.catalogRoot), planned.catalogDir);
    const bundle = await readCatalogBundle(bundleDir);
    assertPlanMatchesBundle(planned, bundle);
    // 陷阱 ① 的第二半：计划说中性、bundle 还停在 legacy（或反过来）必须当场判死。
    if (bundle.schemaGeneration !== generation) {
      throw new CutoverError(
        `${planned.gameId}: bundle is ${bundle.schemaGeneration} schema but the plan publishes ` +
          `${manifestSchemaVersion} (${generation}); a half-migrated cutover is refused`,
      );
    }
    releases.push({ planned, bundle, bundleDir });
  }
  return { plan, planPath, manifestSchemaVersion, generation, releases };
}

// -- 库侧事实 ---------------------------------------------------------------

export type TrustedRow = {
  kind: string;
  catalog_id: string;
  version: string;
  status: string;
  approved_for_trainable: boolean;
  content_hash: string;
};

export type GameRow = {
  id: string;
  title: string;
  allowed_for_capture: boolean;
  approved_for_trainable: boolean;
};

export type ReleaseRow = {
  id: string;
  catalog_id: string;
  catalog_version: string;
  status: string;
};

export type InFlightRow = { game_id: string; sessions: number; latest: string };

export type CutoverFacts = {
  /** key 为 `${kind} ${catalogId}`，值按 created_at 倒序。 */
  trusted: Map<string, TrustedRow[]>;
  games: Map<string, GameRow>;
  releases: ReleaseRow[];
  inFlight: InFlightRow[];
  inFlightTotal: number;
  sessionWindowHours: number;
};

export const trustedKey = (kind: string, catalogId: string) => `${kind} ${catalogId}`;

export async function readCutoverFacts(
  query: CatalogQuery,
  releases: readonly PreparedRelease[],
  sessionWindowHours: number,
): Promise<CutoverFacts> {
  const kinds: string[] = [];
  const catalogIds: string[] = [];
  for (const { planned } of releases) {
    for (const payload of planned.payloads) {
      kinds.push(payload.kind);
      catalogIds.push(payload.catalogId);
    }
  }
  const trustedRows = await query(
    `SELECT t.kind::text AS kind, t.catalog_id, t.version, t.status::text AS status,
            t.approved_for_trainable, t.content_hash
       FROM trusted_catalog_versions t
       JOIN unnest($1::text[], $2::text[]) AS want(kind, catalog_id)
         ON want.kind = t.kind::text AND want.catalog_id = t.catalog_id
      ORDER BY t.created_at DESC`,
    [kinds, catalogIds],
  );
  const trusted = new Map<string, TrustedRow[]>();
  for (const row of trustedRows.rows as TrustedRow[]) {
    const bucket = trusted.get(trustedKey(row.kind, row.catalog_id));
    if (bucket) bucket.push(row);
    else trusted.set(trustedKey(row.kind, row.catalog_id), [row]);
  }

  const gameIds = releases.map(({ planned }) => planned.gameId);
  const gameRows = await query(
    `SELECT id, title, allowed_for_capture, approved_for_trainable
       FROM games WHERE id = ANY($1::text[])`,
    [gameIds],
  );
  const games = new Map<string, GameRow>();
  for (const row of gameRows.rows as GameRow[]) games.set(row.id, row);

  const releaseRows = await query(
    `SELECT id::text AS id, catalog_id, catalog_version, status::text AS status
       FROM catalog_releases
      WHERE channel = $1 AND catalog_id = ANY($2::text[])
      ORDER BY created_at DESC`,
    [CUTOVER_CHANNEL, releases.map(({ planned }) => planned.catalogId)],
  );

  // 陷阱 ③：在飞会话。按选中的游戏分组——`--only` 先跑一款时，只有那一款的会话会被
  // 掐断，全库计数只作参考。
  const inFlightRows = await query(
    `SELECT game_id, count(*)::int AS sessions, max(started_at)::text AS latest
       FROM sessions
      WHERE ended_at IS NULL
        AND started_at > now() - make_interval(hours => $1::int)
        AND game_id = ANY($2::text[])
      GROUP BY game_id
      ORDER BY sessions DESC, game_id`,
    [sessionWindowHours, gameIds],
  );
  const totalRows = await query(
    `SELECT count(*)::int AS sessions
       FROM sessions
      WHERE ended_at IS NULL AND started_at > now() - make_interval(hours => $1::int)`,
    [sessionWindowHours],
  );

  return {
    trusted,
    games,
    releases: releaseRows.rows as ReleaseRow[],
    inFlight: inFlightRows.rows as InFlightRow[],
    inFlightTotal: Number(totalRows.rows[0]?.sessions ?? 0),
    sessionWindowHours,
  };
}

// -- 陷阱 ② 的对账 ----------------------------------------------------------

export type ParityVerdict = {
  gameId: string;
  kind: string;
  catalogId: string;
  predecessor: string;
  incoming: string;
};

/**
 * 新行的 status 会不会和它的前代不一致。
 *
 * `register-dev-catalog.ts`（生产注册作业）用的是 `approve: true`，所以生产库现存的
 * 那几代行大概率是 approved。但「台账写着」不等于「库里就是」，所以这里不猜——逐条读
 * 出来比。判据：
 *
 * - 前代 `approved` / `retired`  ->  新行应当 approved（`retired` 是被取代的已批准行）；
 * - 前代 `draft`                 ->  新行应当 draft；
 * - 前代 `revoked`               ->  不作推断（撤销是治理行为，不该被一次格式断代继承）；
 * - 没有前代（全新 catalog id）  ->  无约束。
 *
 * 不一致时默认拒绝执行：格式断代不该顺带改治理姿态，两个方向都算改。
 */
export function statusParity(
  releases: readonly PreparedRelease[],
  facts: CutoverFacts,
  approve: boolean,
): { drift: ParityVerdict[]; summary: Map<string, number> } {
  const incoming = approve ? "approved" : "draft";
  const drift: ParityVerdict[] = [];
  const summary = new Map<string, number>();
  const bump = (label: string) => summary.set(label, (summary.get(label) ?? 0) + 1);
  for (const { planned } of releases) {
    for (const payload of planned.payloads) {
      const rows = facts.trusted.get(trustedKey(payload.kind, payload.catalogId)) ?? [];
      const exact = rows.find((row) => row.version === payload.version);
      if (exact) {
        // 已经注册过这一版：status 由那一行说了算，本次不会降级它（`--approve` 把
        // draft 升上去是 `--approve` 自己的语义）。
        bump(`already ${exact.status}`);
        continue;
      }
      const predecessor = rows[0];
      if (!predecessor) {
        bump("no predecessor");
        continue;
      }
      bump(`predecessor ${predecessor.status}`);
      if (predecessor.status === "revoked") continue;
      const expected = predecessor.status === "draft" ? "draft" : "approved";
      if (expected !== incoming) {
        drift.push({
          gameId: planned.gameId,
          kind: payload.kind,
          catalogId: payload.catalogId,
          predecessor: predecessor.status,
          incoming,
        });
      }
    }
  }
  return { drift, summary };
}

// -- 预检结论 ---------------------------------------------------------------

export type PreflightOptions = {
  approve: boolean;
  allowActiveSessions: boolean;
  acceptStatusDrift: boolean;
};

export type Preflight = {
  /** 选中游戏的在飞会话总数（陷阱 ③）。 */
  inFlightSelected: number;
  parity: ReturnType<typeof statusParity>;
  /** 本版 release 已经存在的那些（可恢复性：会被跳过）。 */
  alreadyReleased: ReleaseRow[];
  /** 已存在但不是 published 的 release：同版发不了、也改不了，必须人来裁决。 */
  releaseAnomalies: ReleaseRow[];
  /** 计划标 admitted 但库里 `allowed_for_capture` 不为 true 的游戏（只报告，不改）。 */
  admittedButNotCapturable: string[];
  /** 非空即拒绝执行。 */
  blockers: string[];
};

export function preflight(
  releases: readonly PreparedRelease[],
  facts: CutoverFacts,
  options: PreflightOptions,
): Preflight {
  const blockers: string[] = [];

  const inFlightSelected = facts.inFlight.reduce((sum, row) => sum + Number(row.sessions), 0);
  if (inFlightSelected > 0 && !options.allowActiveSessions) {
    blockers.push(
      `${inFlightSelected} in-flight session(s) on the selected games would be cut by the current switch; ` +
        "re-run when nobody is capturing, or pass --allow-active-sessions",
    );
  }

  const parity = statusParity(releases, facts, options.approve);
  if (parity.drift.length > 0 && !options.acceptStatusDrift) {
    blockers.push(
      `${parity.drift.length} payload(s) would land with a status their predecessor row does not have; ` +
        "match the predecessor with --approve --approver <email>, or pass --accept-status-drift",
    );
  }

  const alreadyReleased = facts.releases.filter((release) =>
    releases.some(
      ({ planned }) =>
        planned.catalogId === release.catalog_id && planned.catalogVersion === release.catalog_version,
    ),
  );
  const releaseAnomalies = alreadyReleased.filter((release) => release.status !== "published");
  if (releaseAnomalies.length > 0) {
    blockers.push(
      `${releaseAnomalies.length} release(s) already exist at this catalog version but are not published ` +
        `(${releaseAnomalies.map((row) => `${row.catalog_id}=${row.status}`).join(", ")}); ` +
        "the same (channel, catalog_id, catalog_version) cannot be published twice",
    );
  }

  const admittedButNotCapturable = releases
    .filter(
      ({ planned }) =>
        planned.admission === "admitted" && facts.games.get(planned.gameId)?.allowed_for_capture !== true,
    )
    .map(({ planned }) => planned.gameId);

  return {
    inFlightSelected,
    parity,
    alreadyReleased,
    releaseAnomalies,
    admittedButNotCapturable,
    blockers,
  };
}

// -- 执行 -------------------------------------------------------------------

export type Outcome = {
  gameId: string;
  catalogId: string;
  result: "published" | "skipped" | "failed";
  detail: string;
};

export type RunOptions = {
  manifestSchemaVersion: ManifestSchemaVersion;
  actor: string;
  approve: boolean;
  approver?: string;
  /** register 用；要能写 games / trusted_catalog_versions / catalog_payloads。 */
  registerQuery: CatalogQuery;
  /** publish 用；`dataminer_catalog_publisher` 那份最小权限就够（docs/36）。 */
  publishQuery: CatalogQuery;
  /** 逐款进度。缺省不输出。 */
  log?: (message: string) => void;
  /** 测试用；缺省是 `ClientSourceCatalogValidator`（跑 tools/catalog-validator）。 */
  validator?: CatalogValidator;
};

/**
 * 逐款推进：register 与 publish 各自成一笔事务。跑到第 17 款失败就停在那里并如实
 * 报告，修完重跑能接着走——register 同字节幂等并回同一批行 id，publish 侧读
 * `catalog_releases` 判「这一款已经发过了」。
 */
export async function runCutover(
  releases: readonly PreparedRelease[],
  options: RunOptions,
): Promise<Outcome[]> {
  const outcomes: Outcome[] = [];
  const log = options.log ?? (() => undefined);
  for (const { planned, bundle } of releases) {
    try {
      // 可恢复性：读 `catalog_releases` 判定，而不是等
      // `UNIQUE (channel, catalog_id, catalog_version)` 抛——那条唯一约束会在
      // `publishCatalogRelease` 的事务中段炸，错误信息不指向「已经发过了」。
      const existing = await options.publishQuery(
        `SELECT id::text AS id, status::text AS status
           FROM catalog_releases
          WHERE channel = $1 AND catalog_id = $2 AND catalog_version = $3`,
        [CUTOVER_CHANNEL, planned.catalogId, planned.catalogVersion],
      );
      const existingRow = existing.rows[0];
      if (existingRow) {
        if (existingRow.status !== "published") {
          throw new CutoverError(
            `${planned.catalogId}@${planned.catalogVersion} already has a release with ` +
              `status=${String(existingRow.status)}; it cannot be re-published under the same version`,
          );
        }
        log(`${planned.gameId}  已发布，跳过（release ${String(existingRow.id)}）`);
        outcomes.push({
          gameId: planned.gameId,
          catalogId: planned.catalogId,
          result: "skipped",
          detail: String(existingRow.id),
        });
        continue;
      }

      // 1) 注册。同字节重跑是 no-op 并回同一批行 id，所以这一步天然可恢复。
      await options.registerQuery("BEGIN", []);
      let roster: string[];
      try {
        const report = await registerCatalogBundle(options.registerQuery, bundle, {
          approve: options.approve,
          ...(options.approver !== undefined ? { approver: options.approver } : {}),
          // 陷阱 ②：这三位一律 false，且**不做成参数**——采集准入与可训练审批是独立
          // 的治理决定，不该由一次格式断代顺手改。
          approveTrainable: false,
          allowCapture: false,
          gameTrainable: false,
        });
        roster = report.entries.map((entry) => {
          if (!entry.id) {
            throw new CutoverError(
              `registration returned no row id for ${entry.kind} ${entry.catalogId}`,
            );
          }
          return entry.id;
        });
        await options.registerQuery("COMMIT", []);
      } catch (error) {
        await options.registerQuery("ROLLBACK", []).catch(() => undefined);
        throw error;
      }

      // 2) 发布 + 切 current。`publishCatalogRelease` 自带事务：验签内容、写不可变
      //    draft，再由 `publish_catalog_release` 原子地把上一份 published 改成
      //    superseded 并把这份置 published——作用域是 (channel, catalog_id)。
      const request = {
        channel: CUTOVER_CHANNEL,
        // 陷阱 ①：显式传，取自计划文件顶层。
        manifestSchemaVersion: options.manifestSchemaVersion,
        catalogId: planned.catalogId,
        catalogVersion: planned.catalogVersion,
        actor: options.actor,
        trustedCatalogVersionIds: roster,
      };
      const result = options.validator
        ? await publishCatalogRelease(options.publishQuery, request, options.validator)
        : await publishCatalogRelease(options.publishQuery, request);
      log(
        `${planned.gameId}  OK 已发布 release ${result.releaseId} ` +
          `signingKey=${result.signingKeyId} etag=${result.etag}`,
      );
      outcomes.push({
        gameId: planned.gameId,
        catalogId: planned.catalogId,
        result: "published",
        detail: result.releaseId,
      });
    } catch (error) {
      outcomes.push({
        gameId: planned.gameId,
        catalogId: planned.catalogId,
        result: "failed",
        detail: error instanceof Error ? error.message : String(error),
      });
      // 停在这里：断代是逐款推进的，后面的款在同一个失败原因下多半也过不去，而继续
      // 跑只会让「停在哪一款」变模糊。
      break;
    }
  }
  return outcomes;
}

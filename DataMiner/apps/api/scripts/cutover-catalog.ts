/**
 * issue #855 PR 4 的目录断代驱动 CLI：按 `config/catalogs/release-plan.json` 逐款注册
 * 并发布中性 schema 的 catalog release，完成 current 切换。
 *
 * 编排、闸门与三个陷阱的处理都在 `src/catalog-cutover.ts`（那里能被测试直接驱动，
 * 见该文件顶部「为什么逻辑在 src/ 而 CLI 在 scripts/」）。本文件只有参数解析、打印
 * 与退出码。
 *
 * 用法：
 *
 *   # 离线预演：不连库，打印要跑的 60 条命令与每条的参数
 *   pnpm --filter @dataminer/api cutover:catalog
 *
 *   # 连库预演：加上在飞会话检查、前代 status 对账、已发布跳过判定
 *   DATABASE_URL=postgres://...@127.0.0.1:55432/dataminer_production \
 *     pnpm --filter @dataminer/api cutover:catalog
 *
 *   # 真跑（先跑一款验证；断代窗口期混代是被支持的，见 issue #855 PR 4）
 *   DATABASE_URL=... CATALOG_PUBLISH_DATABASE_URL=... \
 *     pnpm --filter @dataminer/api cutover:catalog -- \
 *       --apply --actor "operator@example.com" --only cyberpunk-2077-steam
 *
 * 两个数据库角色：register 要写 `games` / `trusted_catalog_versions` /
 * `catalog_payloads`；publish 只该有 `dataminer_catalog_publisher` 那份最小权限
 * （docs/36）。所以连接是两条：`DATABASE_URL` 用于注册，`CATALOG_PUBLISH_DATABASE_URL`
 * 用于发布，后者缺省回落到前者（本地/单角色环境）。
 *
 * 退出码：0 成功；1 预检未通过或执行中途失败；2 参数/计划本身用不了。
 */
import path from "node:path";
import { fileURLToPath } from "node:url";

import pg from "pg";

import {
  CUTOVER_CHANNEL,
  CutoverError,
  preflight,
  prepareCutover,
  readCutoverFacts,
  runCutover,
  type CutoverFacts,
  type PreparedCutover,
} from "../src/catalog-cutover.js";
import type { CatalogQuery } from "../src/signed-catalog.js";

const { Client } = pg;
const here = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(here, "../../..");
const DEFAULT_PLAN = path.join(REPO_ROOT, "config", "catalogs", "release-plan.json");
const DEFAULT_CATALOG_ROOT = path.join(REPO_ROOT, "config", "catalogs", "dev");

// -- 命令行 -----------------------------------------------------------------

const argv = process.argv.slice(2).filter((arg) => arg !== "--");

function usageError(message: string): never {
  console.error(`FAIL ${message}`);
  process.exit(2);
}

const VALUE_FLAGS = new Set([
  "--approver",
  "--actor",
  "--plan",
  "--catalog-root",
  "--only",
  "--limit",
  "--session-window-hours",
]);
const BARE_FLAGS = new Set([
  "--apply",
  "--approve",
  "--allow-active-sessions",
  "--accept-status-drift",
  "--help",
]);

for (const [index, arg] of argv.entries()) {
  if (!arg.startsWith("--")) {
    // 上一个 flag 的值，跳过。
    if (index > 0 && VALUE_FLAGS.has(argv[index - 1] ?? "")) continue;
    usageError(`unexpected argument ${JSON.stringify(arg)}`);
  }
  if (!VALUE_FLAGS.has(arg) && !BARE_FLAGS.has(arg)) usageError(`unknown flag ${arg}`);
}

const flag = (name: string) => argv.includes(name);

function option(name: string): string | undefined {
  const index = argv.indexOf(name);
  if (index === -1) return undefined;
  const value = argv[index + 1];
  if (value === undefined || value.startsWith("--")) usageError(`${name} requires a value`);
  return value;
}

/** `--only` 可重复：`--only a --only b`。 */
function repeated(name: string): string[] {
  const values: string[] = [];
  for (const [index, arg] of argv.entries()) {
    if (arg !== name) continue;
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) usageError(`${name} requires a value`);
    values.push(value);
  }
  return values;
}

if (flag("--help")) {
  console.log(
    [
      "usage: pnpm --filter @dataminer/api cutover:catalog -- [flags]",
      "",
      "  --apply                     真跑。缺省是 dry-run，什么都不写。",
      "  --actor <email>             发布归属（--apply 必填）。",
      "  --approve --approver <e>    同时把 trusted_catalog_versions.status 置 approved。",
      "  --only <gameId>             只跑这一款，可重复。",
      "  --limit <n>                 只跑前 n 款（在 --only 过滤之后）。",
      "  --plan <path>               计划文件，缺省 config/catalogs/release-plan.json。",
      "  --catalog-root <path>       bundle 根目录，缺省 config/catalogs/dev。",
      "  --session-window-hours <n>  在飞会话回看窗口，缺省 6。",
      "  --allow-active-sessions     明知有在飞会话仍然执行（会掐断那几场）。",
      "  --accept-status-drift       接受新行 status 与前代不一致。",
      "",
      "环境：DATABASE_URL（注册，需写 games/trusted_catalog_versions/catalog_payloads）",
      "      CATALOG_PUBLISH_DATABASE_URL（发布，缺省回落到 DATABASE_URL）",
    ].join("\n"),
  );
  process.exit(0);
}

const apply = flag("--apply");
const approve = flag("--approve");
const approver = option("--approver");
const actor = option("--actor");
const planPath = path.resolve(option("--plan") ?? DEFAULT_PLAN);
const catalogRoot = path.resolve(option("--catalog-root") ?? DEFAULT_CATALOG_ROOT);
const only = repeated("--only");
const limitRaw = option("--limit");
const limit = limitRaw === undefined ? null : Number.parseInt(limitRaw, 10);
if (limit !== null && (!Number.isInteger(limit) || limit <= 0)) {
  usageError("--limit must be a positive integer");
}
const sessionWindowHours = Number.parseInt(option("--session-window-hours") ?? "6", 10);
if (!Number.isInteger(sessionWindowHours) || sessionWindowHours <= 0) {
  usageError("--session-window-hours must be a positive integer");
}
const allowActiveSessions = flag("--allow-active-sessions");
const acceptStatusDrift = flag("--accept-status-drift");

if (apply && (actor === undefined || actor.trim() === "")) {
  usageError("--apply requires --actor <email> so the publication is attributable");
}
if (approve && (approver === undefined || approver.trim() === "")) {
  usageError("--approve requires --approver <email> (registerCatalogBundle enforces the same)");
}
if (!approve && approver !== undefined) {
  usageError("--approver without --approve does nothing; drop one of the two");
}

const registerUrl = process.env.DATABASE_URL;
const publishUrl = process.env.CATALOG_PUBLISH_DATABASE_URL ?? registerUrl;
if (apply && !registerUrl) usageError("--apply requires DATABASE_URL");

// -- 打印 -------------------------------------------------------------------

function bundleArgument(bundleDir: string): string {
  // `pnpm --filter` 从 apps/api/ 里跑脚本，所以 CLI 上的相对路径也以那里为基准。
  return path.relative(path.join(REPO_ROOT, "apps", "api"), bundleDir);
}

function printPlan(cutover: PreparedCutover, facts: CutoverFacts | null): void {
  const total = cutover.releases.length;
  console.log(
    [
      `计划 ${cutover.planPath}`,
      `  schemaVersion          ${cutover.plan.schemaVersion}`,
      `  manifestSchemaVersion  ${cutover.manifestSchemaVersion}  (${cutover.generation}) <- 逐款显式传给 publisher`,
      `  releases               ${cutover.plan.releases.length} 款，本次选中 ${total} 款`,
      `  channel                ${CUTOVER_CHANNEL}`,
      `  trusted 行 status      ${approve ? `approved（--approver ${approver}）` : "draft（未传 --approve）"}`,
      "  allowed_for_capture    不动（本脚本从不传 --allow-capture）",
      "  approved_for_trainable 不动（本脚本从不传 --approve-trainable / --game-trainable）",
      "",
    ].join("\n"),
  );
  for (const [index, { planned, bundleDir }] of cutover.releases.entries()) {
    const ordinal = `[${String(index + 1).padStart(2, "0")}/${String(total).padStart(2, "0")}]`;
    const released = facts?.releases.find(
      (release) =>
        release.catalog_id === planned.catalogId && release.catalog_version === planned.catalogVersion,
    );
    const game = facts?.games.get(planned.gameId);
    const state = released
      ? `已有 release（status=${released.status}）-> 跳过`
      : facts
        ? game
          ? `games 行在（allowed_for_capture=${game.allowed_for_capture}）`
          : "games 行不存在 -> 本次新建（allowed_for_capture=false）"
        : "";
    console.log(`${ordinal} ${planned.gameId}${state ? `  ${state}` : ""}`);
    console.log(
      `        register  pnpm --filter @dataminer/api register:catalog -- --bundle ${bundleArgument(bundleDir)}` +
        (approve ? ` --approve --approver ${JSON.stringify(approver)}` : ""),
    );
    console.log(
      `        publish   catalogId=${planned.catalogId} catalogVersion=${planned.catalogVersion} ` +
        `manifestSchemaVersion=${cutover.manifestSchemaVersion} actor=${actor ?? "<--actor>"} ` +
        `roster=${planned.payloads.length} 行 id（由 register 返回）`,
    );
  }
}

function printDatabasePreflight(cutover: PreparedCutover, facts: CutoverFacts): boolean {
  const verdict = preflight(cutover.releases, facts, {
    approve,
    allowActiveSessions,
    acceptStatusDrift,
  });
  console.log("\n-- 库侧预检 --------------------------------------------------");

  // 陷阱 ③
  console.log(
    `在飞会话（ended_at IS NULL，最近 ${facts.sessionWindowHours}h）：` +
      `选中游戏 ${verdict.inFlightSelected} 场，全库 ${facts.inFlightTotal} 场`,
  );
  for (const row of facts.inFlight) {
    console.log(`  ! ${row.game_id}  ${row.sessions} 场，最近一场 ${row.latest}`);
  }
  if (verdict.inFlightSelected > 0) {
    console.log(
      "  ! 切 current 会掐断这些会话：目录 payload hash 一变，客户端 preserves_selection\n" +
        "    必然 false，于是 invalidate_catalog_session(CatalogRetired)，该场 Faulted、retryable:false。\n" +
        "    已 ready 的段保留（不丢数据），但采集员现场看不出原因在服务端。",
    );
    console.log(
      allowActiveSessions
        ? "    -> --allow-active-sessions 已给出，继续。"
        : "    -> 默认拒绝执行。确认无人采集后重跑，或显式加 --allow-active-sessions。",
    );
  }

  // 陷阱 ②：games 治理位现状
  const capturable = [...facts.games.values()].filter((game) => game.allowed_for_capture);
  const trainable = [...facts.games.values()].filter((game) => game.approved_for_trainable);
  const admitted = cutover.releases.filter(({ planned }) => planned.admission === "admitted");
  console.log(
    `\ngames 治理位（本脚本一位都不改）：库里 ${facts.games.size}/${cutover.releases.length} 款已登记，` +
      `其中 allowed_for_capture=true ${capturable.length} 款、approved_for_trainable=true ${trainable.length} 款；` +
      `计划标 admission=admitted 的有 ${admitted.length} 款`,
  );
  if (verdict.admittedButNotCapturable.length > 0) {
    console.log(
      `  i 计划标 admitted 但库里 allowed_for_capture 不为 true：${verdict.admittedButNotCapturable.join(", ")}\n` +
        "    本脚本不会替它们翻开——采集准入是独立的治理决定（issue #855 §A）。",
    );
  }

  // 陷阱 ②：trusted 行 status 对账
  const payloadTotal = cutover.releases.reduce((sum, { planned }) => sum + planned.payloads.length, 0);
  console.log(`\ntrusted_catalog_versions 对账（逐份 payload，共 ${payloadTotal} 份）：`);
  for (const [label, count] of [...verdict.parity.summary].sort()) {
    console.log(`  ${label.padEnd(24)} ${count}`);
  }
  if (verdict.parity.drift.length > 0) {
    const byGame = new Map<string, typeof verdict.parity.drift>();
    for (const item of verdict.parity.drift) {
      const bucket = byGame.get(item.gameId);
      if (bucket) bucket.push(item);
      else byGame.set(item.gameId, [item]);
    }
    console.log(
      `  ! ${verdict.parity.drift.length} 份 payload 的新行 status 会与前代不一致（前代 -> 本次将写入）：`,
    );
    for (const [gameId, items] of byGame) {
      console.log(`    ${gameId}  ${items.length} 份：${items[0]!.predecessor} -> ${items[0]!.incoming}`);
    }
    console.log(
      "    格式断代不该顺带改治理姿态。要么补 --approve --approver <email> 与前代看齐，\n" +
        "    要么确认这就是想要的并加 --accept-status-drift。",
    );
    if (acceptStatusDrift) console.log("    -> --accept-status-drift 已给出，继续。");
  }

  // 已发布的（可恢复性）
  console.log(
    `\ncatalog_releases：选中的 ${cutover.releases.length} 款里，已存在本版 release 的 ` +
      `${verdict.alreadyReleased.length} 款${verdict.alreadyReleased.length > 0 ? "（会跳过）" : ""}`,
  );
  for (const release of verdict.releaseAnomalies) {
    console.log(
      `  ! ${release.catalog_id}@${release.catalog_version} 的 release status=${release.status}，不是 published\n` +
        "    同一 (channel, catalog_id, catalog_version) 无法重发（唯一约束），也不能改写既有行。\n" +
        "    这一款需要人来裁决：要么它本就该是这个状态，要么计划该出一个新的 catalogVersion。",
    );
  }

  for (const blocker of verdict.blockers) console.log(`\nFAIL ${blocker}`);
  return verdict.blockers.length > 0;
}

// -- 主流程 -----------------------------------------------------------------

async function connect(url: string): Promise<{ client: pg.Client; query: CatalogQuery }> {
  const client = new Client({ connectionString: url });
  await client.connect();
  const query: CatalogQuery = async (text, params) => {
    const result = await client.query(text, params as unknown[]);
    return { rows: result.rows as Array<Record<string, unknown>> };
  };
  return { client, query };
}

let cutover: PreparedCutover;
try {
  cutover = await prepareCutover({ planPath, catalogRoot, only, limit });
} catch (error) {
  if (error instanceof CutoverError) usageError(error.message);
  throw error;
}

let facts: CutoverFacts | null = null;
if (registerUrl) {
  const { client, query } = await connect(registerUrl);
  try {
    facts = await readCutoverFacts(query, cutover.releases, sessionWindowHours);
  } finally {
    await client.end().catch(() => undefined);
  }
}

printPlan(cutover, facts);
let blocked = false;
if (facts) {
  blocked = printDatabasePreflight(cutover, facts);
} else {
  console.log(
    "\n-- 库侧预检 --------------------------------------------------\n" +
      "DATABASE_URL 未设置：跳过在飞会话检查、status 对账与已发布判定。\n" +
      "上面的计划是纯离线核对（计划 <-> 磁盘字节 <-> schema 代际）的结果。",
  );
}

if (!apply) {
  console.log(
    `\ndry run：什么都没写。加 --apply --actor <email> 真跑${registerUrl ? "" : "（并设置 DATABASE_URL）"}。`,
  );
  process.exit(blocked ? 1 : 0);
}
if (blocked) {
  console.error("\nFAIL 预检未通过，未执行任何写入。");
  process.exit(1);
}

console.log("\n-- 执行 ------------------------------------------------------");
const registerConnection = await connect(registerUrl!);
const publishConnection = publishUrl === registerUrl ? registerConnection : await connect(publishUrl!);
let outcomes;
try {
  outcomes = await runCutover(cutover.releases, {
    manifestSchemaVersion: cutover.manifestSchemaVersion,
    actor: actor!,
    approve,
    ...(approver !== undefined ? { approver } : {}),
    registerQuery: registerConnection.query,
    publishQuery: publishConnection.query,
    log: (message) => console.log(`  ${message}`),
  });
} finally {
  await registerConnection.client.end().catch(() => undefined);
  if (publishConnection !== registerConnection) {
    await publishConnection.client.end().catch(() => undefined);
  }
}

const tally = {
  published: outcomes.filter((outcome) => outcome.result === "published").length,
  skipped: outcomes.filter((outcome) => outcome.result === "skipped").length,
  failed: outcomes.filter((outcome) => outcome.result === "failed").length,
};
const failure = outcomes.find((outcome) => outcome.result === "failed");
if (failure) {
  console.error(`\nFAIL 停在第 ${outcomes.length} 款：${failure.gameId}\n  ${failure.detail}`);
}
console.log(
  "\n-- 汇总 ------------------------------------------------------\n" +
    `选中 ${cutover.releases.length} 款：发布 ${tally.published}、跳过 ${tally.skipped}、` +
    `失败 ${tally.failed}、未开始 ${cutover.releases.length - outcomes.length}`,
);
if (tally.failed > 0) {
  console.log("修完重跑同一条命令即可接着走：已发布的会跳过，已注册的同字节是 no-op。");
  process.exit(1);
}

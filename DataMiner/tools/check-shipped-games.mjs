#!/usr/bin/env node
/**
 * 核对「安装包内置了哪些游戏」与「服务端认哪些游戏」这两份清单。
 *
 * 背景（docs/101 §4）：内置目录由 cargo feature 决定（build-windows-installer.ps1
 * 的 $allGamesGameIds），服务端准入由生产库 `games` 表决定。二者互不知情，漂移时
 * 采集员会在 UI 里看到一款游戏、选中、录完，然后每次会话注册都被 404
 * `game_not_registered` 拒绝——而那类拒绝在数据库里不留任何行，所以没有任何人
 * 会发现。2026-08-05/06 实际发生过 31 次同类拒绝。
 *
 * 两种模式：
 *
 *   node tools/check-shipped-games.mjs
 *       静态检查，CI 用。比对 build-windows-installer.ps1 与 shipped-games.json
 *       的 gameId 集合是否严格一致。不需要网络与数据库。
 *
 *   node tools/check-shipped-games.mjs --against-production
 *       连生产库核对 admission 是否属实。需要先起 cloud-sql-proxy：
 *         cloud-sql-proxy --port 55432 hakkolite:us-central1:dataminer-db-production
 *       并需要 `pg`（apps/api 的依赖，或临时 npm i pg）。
 *
 * 退出码：0 一致；1 有漂移；2 用不了（缺文件、连不上库）。
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const INSTALLER = join(ROOT, "tools", "build-windows-installer.ps1");
const MANIFEST = join(ROOT, "config", "catalogs", "shipped-games.json");
const DEV_CATALOGS = join(ROOT, "config", "catalogs", "dev");

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(2);
}

/**
 * 从 PowerShell 脚本里抠出 $allGamesGameIds 的字面量数组。
 *
 * 刻意不引 PowerShell 解析器：这份清单是纯字面量，而在 CI 的 Linux runner 上
 * 装 pwsh 只为读一个数组不划算。清单一旦不再是字面量（比如改成拼接生成），
 * 这里会因为匹配不到而直接 exit 2，不会假装通过。
 */
function installerText() {
  let text;
  try {
    text = readFileSync(INSTALLER, "utf8");
  } catch {
    fail(`读不到 ${INSTALLER}`);
  }
  return text;
}

function literalArray(text, variableName) {
  const block = text.match(
    new RegExp(`\\$${variableName}\\s*=\\s*@\\(([\\s\\S]*?)\\n\\)`),
  );
  if (!block) {
    fail(`在 build-windows-installer.ps1 里找不到 $${variableName} 的字面量数组`);
  }
  const values = [...block[1].matchAll(/'([^']+)'/g)].map((match) => match[1]);
  if (values.length === 0) fail(`$${variableName} 解析出来是空的`);
  return values;
}

function shippedGameIdsFromInstaller(text) {
  return literalArray(text, "allGamesGameIds");
}

function shippedProcessNamesFromInstaller(text) {
  // COD 专属口味退役后进程名并入 $allGamesProcessNames 一个数组。
  return literalArray(text, "allGamesProcessNames");
}

function manifestGames() {
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(MANIFEST, "utf8"));
  } catch (error) {
    fail(`读不到或解析不了 ${MANIFEST}：${error.message}`);
  }
  if (!Array.isArray(parsed.games)) fail("shipped-games.json 缺 games 数组");
  for (const game of parsed.games) {
    if (!game.gameId) fail("shipped-games.json 里有条目缺 gameId");
    if (Object.hasOwn(game, "appId") && !/^\d+$/.test(game.appId ?? "")) {
      fail(`${game.gameId} 的可选 appId 必须是十进制字符串`);
    }
    if (game.admission !== "admitted" && game.admission !== "onboarding") {
      fail(`${game.gameId} 的 admission 只能是 admitted / onboarding，实得 ${game.admission}`);
    }
  }
  return parsed;
}

function diff(left, right) {
  return left.filter((value) => !right.includes(value));
}

function catalogProcessNames(manifest) {
  return manifest.games.flatMap((game) => {
    const path = join(DEV_CATALOGS, game.catalogDir, "game-profile.json");
    let profile;
    try {
      profile = JSON.parse(readFileSync(path, "utf8"));
    } catch (error) {
      fail(`读不到或解析不了 ${path}：${error.message}`);
    }
    if (!Array.isArray(profile.executable_names) || profile.executable_names.length === 0) {
      fail(`${path} 缺 executable_names`);
    }
    return profile.executable_names;
  });
}

function reportStatic(installerIds, installerProcessNames, manifest) {
  const manifestIds = manifest.games.map((game) => game.gameId);
  const missingFromManifest = diff(installerIds, manifestIds);
  const missingFromInstaller = diff(manifestIds, installerIds);
  const catalogNames = [...new Set(catalogProcessNames(manifest))];
  const installerNames = [...new Set(installerProcessNames)];
  const staleInstallerNames = diff(installerNames, catalogNames);
  const missingInstallerNames = diff(catalogNames, installerNames);

  if (
    missingFromManifest.length === 0 &&
    missingFromInstaller.length === 0 &&
    staleInstallerNames.length === 0 &&
    missingInstallerNames.length === 0
  ) {
    const admitted = manifest.games.filter((game) => game.admission === "admitted").length;
    const onboarding = manifest.games.length - admitted;
    console.log(
      `✓ 内置游戏清单一致：${manifest.games.length} 款（${admitted} 款已开放，${onboarding} 款接入中）`,
    );
    return true;
  }

  console.error("✗ 内置游戏清单漂移：");
  for (const id of missingFromManifest) {
    console.error(`   安装包装了但 shipped-games.json 没有：${id}`);
  }
  for (const id of missingFromInstaller) {
    console.error(`   shipped-games.json 有但安装包没装：${id}`);
  }
  for (const name of staleInstallerNames) {
    console.error(`   安装包脚本仍检查旧进程名：${name}`);
  }
  for (const name of missingInstallerNames) {
    console.error(`   catalog 声明但安装包脚本未检查：${name}`);
  }
  console.error("   两份清单必须同增同减，否则客户端无法把未开放的游戏标成不可选。");
  return false;
}

async function reportAgainstProduction(manifest) {
  let pg;
  try {
    ({ default: pg } = await import("pg"));
  } catch {
    fail("需要 pg：在 apps/api 下 pnpm install，或临时 npm i pg");
  }
  const client = new pg.Client({
    host: "127.0.0.1",
    port: Number(process.env.DATAMINER_DB_PORT ?? 55432),
    user: process.env.DATAMINER_DB_USER,
    password: process.env.DATAMINER_DB_PASSWORD,
    database: process.env.DATAMINER_DB_NAME ?? "dataminer_production",
    ssl: false,
  });
  try {
    await client.connect();
  } catch (error) {
    fail(`连不上生产库（先起 cloud-sql-proxy --port 55432）：${error.message}`);
  }
  const { rows } = await client.query("select id, allowed_for_capture from games");
  await client.end();

  const registered = new Map(rows.map((row) => [row.id, row.allowed_for_capture]));
  let drifted = false;
  for (const game of manifest.games) {
    const isRegistered = registered.has(game.gameId) && registered.get(game.gameId);
    const expected = game.admission === "admitted";
    if (isRegistered !== expected) {
      drifted = true;
      console.error(
        `✗ ${game.gameId}：清单写 ${game.admission}，生产库实际 ${
          isRegistered ? "已注册且允许采集" : "未注册或不允许采集"
        }`,
      );
    }
  }
  if (!drifted) {
    console.log(`✓ admission 与生产库一致（核对 ${manifest.games.length} 款）`);
  } else {
    console.error("   改 shipped-games.json 的 admission，并更新 verifiedAgainstProduction 日期。");
  }
  return !drifted;
}

const text = installerText();
const manifest = manifestGames();
let ok = reportStatic(
  shippedGameIdsFromInstaller(text),
  shippedProcessNamesFromInstaller(text),
  manifest,
);
if (process.argv.includes("--against-production")) {
  ok = (await reportAgainstProduction(manifest)) && ok;
}
process.exit(ok ? 0 : 1);

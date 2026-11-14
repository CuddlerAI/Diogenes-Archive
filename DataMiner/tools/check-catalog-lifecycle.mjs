/**
 * Catalog 目录生命周期门禁（issue #855 §1 的分层表）。
 *
 * `config/catalogs/dev/` 下的每个目录必须**恰好**落进四类之一，并且各类有各自的
 * 不变量。此前没有任何东西检查这件事：目录数、schema 代际、标题口径和"哪些能发布"
 * 分散在 shipped-games.json、各 bundle 的字节和人的记忆里，三者可以互相矛盾而全绿。
 *
 *   | 类别                | 判据                        | schema | 进正式 release |
 *   |---------------------|-----------------------------|--------|----------------|
 *   | shipped (30)        | 在 shipped-games.json 里    | 中性   | 是             |
 *   | synthetic (1)       | 目录名 `synthetic`          | 中性   | **否**         |
 *   | frozen snapshot (3) | 目录里有 `SUPERSEDED.md`    | legacy | **否**         |
 *   | 未分类 (0)          | 以上都不是                  | —      | 失败           |
 *
 * 分类是**派生**的，不是手抄名单：shipped 由 shipped-games.json 回答，冻结由
 * `SUPERSEDED.md` 这个 sidecar 回答（issue 自己就是这么点名那三份的）。手抄一份
 * 名单意味着删一个目录要改两处，而漏改的那一处不会有人发现。
 *
 * 两个方向都要挡：
 * - 活跃 bundle 停在 legacy schema / 标题仍自称禁止训练 → 断代没做完；
 * - 冻结快照被"顺手迁移" → 研究快照的原字节没了，而它们存在的全部意义就是原字节。
 *   门禁因此必须能区分两者，不能把冻结快照误报成「schema 未迁移」（issue §1 原话）。
 *
 * 用法：
 *   node tools/check-catalog-lifecycle.mjs            校验（CI 用）
 *   node tools/check-catalog-lifecycle.mjs --write    重算并写回 release-plan.json
 */
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const DEV_ROOT = path.join(ROOT, "config", "catalogs", "dev");
const SHIPPED_GAMES = path.join(ROOT, "config", "catalogs", "shipped-games.json");
const RELEASE_PLAN = path.join(ROOT, "config", "catalogs", "release-plan.json");

/** 中性一代（issue #855 PR 1 的 `catalog-schema.ts` / `catalog.rs` 同一张表）。 */
export const NEUTRAL_SCHEMAS = {
  "catalog.json": "dm-catalog-bundle-3",
  "device-profile.json": "dm-device-profile-3",
  "recording-profile.json": "dm-recording-profile-2",
  "game-profile.json": "dm-game-profile-3",
};

/** legacy 一代：三份冻结研究快照永远停在这里。 */
export const LEGACY_SCHEMAS = {
  "catalog.json": "dm-catalog-bundle-2",
  "device-profile.json": "dm-device-profile-2",
  "recording-profile.json": "dm-recording-profile-1",
  "game-profile.json": "dm-game-profile-2",
};

/** 与迁移 0028 的 `games_non_trainable_title_guard` 及 register-catalog.ts 同一条正则。 */
export const NON_TRAINABLE_TITLE_PATTERN = /禁止训练|non[-_ ]?trainable/i;

export const RELEASE_PLAN_SCHEMA_VERSION = "dm-catalog-release-plan-1";

const PAYLOAD_KINDS = [
  ["device_profiles", "device-profile.json", "device_profile"],
  ["recording_profiles", "recording-profile.json", "recording_profile"],
  ["game_profiles", "game-profile.json", "game_profile"],
  ["mappings", "mapping.json", "game_mapping"],
  ["depth_profiles", "depth-profile.json", "depth_profile"],
  ["camera_bindings", "camera-binding.json", "camera_binding"],
];

function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

function sha256Of(file) {
  return `sha256:${createHash("sha256").update(readFileSync(file)).digest("hex")}`;
}

/**
 * 把 `config/catalogs/dev/` 读成四类。未分类目录直接抛——沉默地归进某一类，正是让
 * 一份没人负责的 bundle 混进内嵌/发布名单的那条路。
 */
export function readCatalogLifecycleInventory({
  devRoot = DEV_ROOT,
  shippedGamesPath = SHIPPED_GAMES,
} = {}) {
  const shipped = readJson(shippedGamesPath).games;
  const shippedByDir = new Map(shipped.map((game) => [game.catalogDir, game]));
  const directories = readdirSync(devRoot)
    .filter((name) => statSync(path.join(devRoot, name)).isDirectory())
    .sort();

  const buckets = { shipped: [], synthetic: [], frozen: [], unclassified: [] };
  for (const name of directories) {
    const directory = path.join(devRoot, name);
    const manifest = readJson(path.join(directory, "catalog.json"));
    const gameProfile = readJson(path.join(directory, "game-profile.json"));
    const entry = {
      directory: name,
      path: directory,
      catalogId: manifest.catalog_id,
      catalogVersion: manifest.catalog_version,
      gameId: gameProfile.game_id,
      displayName: gameProfile.display_name,
      manifest,
      shipped: shippedByDir.get(name) ?? null,
    };
    if (existsSync(path.join(directory, "SUPERSEDED.md"))) {
      buckets.frozen.push(entry);
    } else if (shippedByDir.has(name)) {
      buckets.shipped.push(entry);
    } else if (name === "synthetic") {
      buckets.synthetic.push(entry);
    } else {
      buckets.unclassified.push(entry);
    }
  }
  return buckets;
}

/** 冻结快照与 synthetic 均不得进正式 release。 */
export function nonPublishableCatalogIds(buckets) {
  return [...buckets.frozen, ...buckets.synthetic]
    .map((entry) => entry.catalogId)
    .sort();
}

/** 一份 bundle 在 release plan 里的样子：PR 4 拿它逐款注册、组 roster、签名发布。 */
export function releaseEntryFor(entry) {
  const payloads = [];
  for (const [manifestKey, filename, kind] of PAYLOAD_KINDS) {
    const declared = entry.manifest[manifestKey];
    if (!Array.isArray(declared) || declared.length === 0) continue;
    for (const pinned of declared) {
      payloads.push({
        kind,
        catalogId: pinned.id,
        version: pinned.version,
        contentHash: pinned.content_hash,
        file: filename,
      });
    }
  }
  return {
    gameId: entry.gameId,
    catalogDir: entry.directory,
    catalogId: entry.catalogId,
    catalogVersion: entry.catalogVersion,
    displayName: entry.displayName,
    admission: entry.shipped.admission,
    payloads,
  };
}

export function buildReleasePlan(buckets) {
  return {
    $comment: [
      `issue #855 PR 2 产出的**待签名/待发布**清单：${buckets.shipped.length} 款 shipped 游戏各一份中性 schema bundle。`,
      "",
      "本 PR 只生成它，不签名、不发布、不切换 production current——那是 PR 4，前置条件是",
      "「含新 schema reader 的兼容客户端已发布并达到切换条件」。在那之前 current 仍是旧",
      "v2 release，旧客户端拉到的字节一个都没变。",
      "",
      "为什么是文件而不是 roster：真正的 publish roster 要的是 `trusted_catalog_versions`",
      "的行 id（UUID），那些 id 只有在对着目标库注册之后才存在。本文件登记的是注册**之前**",
      "就已确定、且可离线核验的那部分：每款游戏的 catalog identity 与逐份 payload 的",
      "(kind, id, version, content_hash)。PR 4 按它逐款 register:catalog，再用返回的行 id 组",
      "roster 发布。",
      "",
      "synthetic 与三份 SUPERSEDED 研究快照**不在**本清单里（issue #855 §1）：前者是回归",
      "基线不是游戏，后者冻结在 legacy schema、不得发布。",
      "",
      "由 tools/check-catalog-lifecycle.mjs 生成与校验；漂了 CI 直接红，不要手改。",
    ],
    schemaVersion: RELEASE_PLAN_SCHEMA_VERSION,
    manifestSchemaVersion: NEUTRAL_SCHEMAS["catalog.json"],
    signed: false,
    published: false,
    releases: buckets.shipped
      .map((entry) => releaseEntryFor(entry))
      .sort((a, b) => (a.gameId < b.gameId ? -1 : 1)),
  };
}

function checkBundleGeneration(entry, schemas, failures, label) {
  for (const [filename, expected] of Object.entries(schemas)) {
    const file = path.join(entry.path, filename);
    const actual = readJson(file).schema_version;
    if (actual !== expected) {
      failures.push(
        `✘ ${label} ${entry.directory}/${filename} schema_version is ${actual}, expected ${expected}`,
      );
    }
  }
}

function checkNoRetiredField(entry, failures) {
  for (const filename of Object.keys(NEUTRAL_SCHEMAS)) {
    const payload = readJson(path.join(entry.path, filename));
    if ("non_trainable" in payload) {
      failures.push(
        `✘ active ${entry.directory}/${filename} still carries non_trainable; ` +
          "the neutral catalog schema does not express training classification (issue #855 §A.4)",
      );
    }
  }
}

function checkHashes(entry, failures) {
  for (const [manifestKey, filename] of PAYLOAD_KINDS) {
    for (const pinned of entry.manifest[manifestKey] ?? []) {
      const file = path.join(entry.path, filename);
      if (!existsSync(file)) {
        failures.push(`✘ ${entry.directory} declares ${manifestKey} but ${filename} is missing`);
        continue;
      }
      const digest = sha256Of(file);
      if (pinned.content_hash !== digest) {
        failures.push(
          `✘ ${entry.directory}/${filename} bytes do not match the manifest content_hash ` +
            `(${pinned.content_hash} declared, ${digest} on disk)`,
        );
      }
    }
  }
}

export function checkLifecycle({ write = false } = {}) {
  const buckets = readCatalogLifecycleInventory();
  const failures = [];

  for (const entry of buckets.unclassified) {
    failures.push(
      `✘ ${entry.directory} is not classified: it is not in shipped-games.json, is not ` +
        "`synthetic`, and carries no SUPERSEDED.md. Every catalog directory must belong to " +
        "exactly one lifecycle class (issue #855 §1)",
    );
  }

  // ── 活跃：中性 schema、无退役字段、中性标题、三处标题一致 ──────────────────
  for (const entry of [...buckets.shipped, ...buckets.synthetic]) {
    checkBundleGeneration(entry, NEUTRAL_SCHEMAS, failures, "active");
    checkNoRetiredField(entry, failures);
    checkHashes(entry, failures);
    if (NON_TRAINABLE_TITLE_PATTERN.test(entry.displayName)) {
      failures.push(
        `✘ active ${entry.directory} display_name declares it non-trainable: ` +
          `${JSON.stringify(entry.displayName)}. Training/environment状态由 UI 独立展示，` +
          "不再塞进游戏名（issue #855 §A.3）",
      );
    }
    const mapping = readJson(path.join(entry.path, "mapping.json"));
    if (NON_TRAINABLE_TITLE_PATTERN.test(mapping.game?.name ?? "")) {
      failures.push(
        `✘ active ${entry.directory}/mapping.json game.name declares it non-trainable: ` +
          JSON.stringify(mapping.game.name),
      );
    }
    if (entry.shipped && entry.shipped.displayName !== entry.displayName) {
      failures.push(
        `✘ ${entry.directory}: shipped-games.json displayName ` +
          `${JSON.stringify(entry.shipped.displayName)} != game-profile display_name ` +
          `${JSON.stringify(entry.displayName)}. 一个 game_id 一个名字——UI 与 games.title ` +
          "读的是不同的那一份，漂了就是两个人看到两个游戏",
      );
    }
  }

  // ── 冻结：legacy schema 原样，且必须仍带 non_trainable（原字节的证据） ────────
  for (const entry of buckets.frozen) {
    checkBundleGeneration(entry, LEGACY_SCHEMAS, failures, "frozen snapshot");
    checkHashes(entry, failures);
    for (const filename of Object.keys(LEGACY_SCHEMAS)) {
      if (!("non_trainable" in readJson(path.join(entry.path, filename)))) {
        failures.push(
          `✘ frozen snapshot ${entry.directory}/${filename} lost its legacy non_trainable field; ` +
            "these three snapshots exist precisely to keep their original bytes (issue #855 §A.9)",
        );
      }
    }
    if (entry.shipped) {
      failures.push(
        `✘ frozen snapshot ${entry.directory} is listed in shipped-games.json; ` +
          "a superseded research snapshot must never ship or publish",
      );
    }
  }

  // ── 库存断言：32 shipped + 1 synthetic + 3 superseded = 36 ─────────────────
  const counts = {
    shipped: buckets.shipped.length,
    synthetic: buckets.synthetic.length,
    frozen: buckets.frozen.length,
  };
  const expected = { shipped: 32, synthetic: 1, frozen: 3 };
  for (const [key, want] of Object.entries(expected)) {
    if (counts[key] !== want) {
      failures.push(`✘ expected ${want} ${key} catalog directories, found ${counts[key]}`);
    }
  }
  const shippedGames = readJson(SHIPPED_GAMES).games;
  if (shippedGames.length !== buckets.shipped.length) {
    failures.push(
      `✘ shipped-games.json lists ${shippedGames.length} games but ${buckets.shipped.length} ` +
        "directories resolved to the shipped class",
    );
  }

  // ── 待签名 release 计划：与 shipped 清单逐条对账 ────────────────────────────
  const plan = buildReleasePlan(buckets);
  const nonPublishable = new Set(nonPublishableCatalogIds(buckets));
  for (const release of plan.releases) {
    if (nonPublishable.has(release.catalogId)) {
      failures.push(`✘ release plan contains a non-publishable catalog ${release.catalogId}`);
    }
  }
  if (write) {
    writeFileSync(RELEASE_PLAN, `${JSON.stringify(plan, null, 2)}\n`, "utf8");
  } else if (!existsSync(RELEASE_PLAN)) {
    failures.push(`✘ ${path.relative(ROOT, RELEASE_PLAN)} is missing; run with --write`);
  } else {
    const onDisk = readFileSync(RELEASE_PLAN, "utf8");
    if (onDisk !== `${JSON.stringify(plan, null, 2)}\n`) {
      failures.push(
        `✘ ${path.relative(ROOT, RELEASE_PLAN)} is stale; regenerate with ` +
          "`node tools/check-catalog-lifecycle.mjs --write`",
      );
    }
  }

  return { buckets, counts, plan, nonPublishable: [...nonPublishable], failures };
}

function main() {
  const write = process.argv.includes("--write");
  const { counts, plan, nonPublishable, failures } = checkLifecycle({ write });
  for (const failure of failures) console.error(failure);
  const total = counts.shipped + counts.synthetic + counts.frozen;
  console.log(
    `catalog lifecycle: ${total} directories — ${counts.shipped} shipped, ` +
      `${counts.synthetic} synthetic, ${counts.frozen} frozen snapshot(s); ` +
      `${plan.releases.length} pending-signature release(s); ` +
      `${nonPublishable.length} catalog id(s) refused by the publisher`,
  );
  if (failures.length) {
    console.error(`${failures.length} lifecycle violation(s)`);
    process.exit(1);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}

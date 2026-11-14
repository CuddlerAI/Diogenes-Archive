import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { describeCatalogMiss } from "./repository.js";

/**
 * docs/101 行动项 E 的回归：`catalog_version_not_found` 必须说得出**差在哪一项**。
 *
 * 旧文案是一句 `${kind} catalog version was not found`——四本册子逐本查，任何一本
 * 对不上都得到同一句话。而被拒绝的请求在数据库里不留任何行（docs/101 §3），
 * 所以 2026-08-05/06 那 31 次拒绝，服务端既答不出是哪一本，也答不出差的是
 * 版本号还是字节（§3.3）。
 */

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;

/** `sha256_digest` 域要求 `^sha256:[0-9a-f]{64}$`，助记前缀补足 64 位十六进制。 */
function digest(mnemonic: string): string {
  const hex = [...mnemonic].map((c) => c.charCodeAt(0).toString(16).padStart(2, "0")).join("");
  return `sha256:${(hex + "0".repeat(64)).slice(0, 64)}`;
}

/** `describeCatalogMiss` 只用到 `query`，给它一个结构相容的最小壳。 */
function client() {
  return {
    query: (text: string, values?: unknown[]) => db.query(text, values as never[]),
  } as never;
}

async function insertVersion(input: {
  kind: string;
  catalogId: string;
  version: string | null;
  contentHash: string;
  status: string;
  gameId: string | null;
}) {
  // 表上的 CHECK 要求：approved 必须有 approved_at/by，revoked 必须有 revoked_at，
  // 且 device_profile 的 game_id 必须为 NULL、其余 kind 必须非 NULL。照实满足，
  // 不放宽约束——放宽等于用一个真实不可能出现的状态去测诊断文案。
  await db.query(
    `INSERT INTO trusted_catalog_versions
       (id, kind, catalog_id, version, content_hash, status, game_id, snapshot,
        approved_at, approved_by, revoked_at)
     VALUES (gen_random_uuid(), $1, $2, $3, $4, $5::catalog_status, $6, '{}'::jsonb,
        CASE WHEN $5::text = 'approved' THEN now() END,
        CASE WHEN $5::text = 'approved' THEN 'test' END,
        CASE WHEN $5::text = 'revoked' THEN now() END)`,
    [input.kind, input.catalogId, input.version, input.contentHash, input.status, input.gameId],
  );
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind)
     VALUES ('game-a', 'A', 'game', 'first_person', 'synthetic'),
            ('game-b', 'B', 'game', 'first_person', 'synthetic')`,
  );
});

afterAll(async () => {
  await db.close();
});

describe("目录未命中的诊断文案（docs/101 行动项 E）", () => {
  it("这个 id 根本不存在时，直说不存在", async () => {
    const message = await describeCatalogMiss(
      client(),
      "game_profile",
      "never-registered",
      digest("aaa"),
      "1",
      "game-a",
    );
    expect(message).toContain("no game_profile catalog with that id at all");
    // 请求方声明了什么必须回显，否则日志里对不上是哪一次。
    expect(message).toContain("never-registered@1");
    expect(message).toContain(digest("aaa"));
  });

  it("字节对得上但版本被吊销时，指名是吊销", async () => {
    await insertVersion({
      kind: "game_profile",
      catalogId: "revoked-one",
      version: "3",
      contentHash: digest("rev"),
      status: "revoked",
      gameId: "game-a",
    });
    const message = await describeCatalogMiss(
      client(),
      "game_profile",
      "revoked-one",
      digest("rev"),
      "3",
      "game-a",
    );
    expect(message).toContain("those exact bytes exist but the version is revoked");
  });

  it("字节对得上但版本号声明错了时，报出真实版本号", async () => {
    await insertVersion({
      kind: "game_profile",
      catalogId: "version-drift",
      version: "4",
      contentHash: digest("v4"),
      status: "approved",
      gameId: "game-a",
    });
    const message = await describeCatalogMiss(
      client(),
      "game_profile",
      "version-drift",
      digest("v4"),
      "2",
      "game-a",
    );
    expect(message).toContain("registered as version 4, not 2");
  });

  it("字节对得上但挂在别的游戏下时，报出真实归属", async () => {
    await insertVersion({
      kind: "recording_profile",
      catalogId: "cross-game",
      version: "1",
      contentHash: digest("x"),
      status: "approved",
      gameId: "game-b",
    });
    const message = await describeCatalogMiss(
      client(),
      "recording_profile",
      "cross-game",
      digest("x"),
      "1",
      "game-a",
    );
    expect(message).toContain("registered against game game-b, not game-a");
  });

  /**
   * 这一条是 wangharp 那 31 次最可能的形态：目录在、版本也在，但客户端手上的
   * 字节不是服务端信任的那份。旧文案在这里什么都说不出来。
   */
  it("字节对不上时，把服务端信任的版本与哈希列出来", async () => {
    await insertVersion({
      kind: "game_profile",
      catalogId: "hash-drift",
      version: "4",
      contentHash: digest("trusted"),
      status: "approved",
      gameId: "game-a",
    });
    const message = await describeCatalogMiss(
      client(),
      "game_profile",
      "hash-drift",
      digest("whatever-the-client-has"),
      "4",
      "game-a",
    );
    expect(message).toContain("content hash does not match any trusted version");
    expect(message).toContain(`this deployment trusts 4=${digest("trusted")}`);
  });

  it("该 id 的版本全部被吊销时，不谎称还信任着什么", async () => {
    await insertVersion({
      kind: "game_mapping",
      catalogId: "all-revoked",
      version: "1",
      contentHash: digest("old"),
      status: "revoked",
      gameId: "game-a",
    });
    const message = await describeCatalogMiss(
      client(),
      "game_mapping",
      "all-revoked",
      digest("client-has-this"),
      "1",
      "game-a",
    );
    expect(message).toContain("all known versions are revoked");
    expect(message).not.toContain("this deployment trusts ");
  });

  it("device_profile 的 game_id 为 null 也能正确描述", async () => {
    await insertVersion({
      kind: "device_profile",
      catalogId: "dev-profile",
      version: "2",
      contentHash: digest("dp"),
      status: "approved",
      gameId: null,
    });
    const message = await describeCatalogMiss(
      client(),
      "device_profile",
      "dev-profile",
      digest("not-dp"),
      "2",
      null,
    );
    expect(message).toContain("device_profile dev-profile@2");
    expect(message).toContain(`this deployment trusts 2=${digest("dp")}`);
  });
});

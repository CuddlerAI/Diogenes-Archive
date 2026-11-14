import { createHash, randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
let trustedIds: Record<string, string>;
const CORE_KINDS = ["device_profile", "recording_profile", "game_profile", "game_mapping"];
const DEPTH_KINDS = ["depth_profile", "camera_binding"];

function hash(bytes: Uint8Array): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

async function createRelease(version: string): Promise<string> {
  const id = randomUUID();
  const manifest = Buffer.from(`{"catalog_version":"${version}"}`, "utf8");
  const envelope = Buffer.from(`{"signed":"${version}"}`, "utf8");
  await db.query(
    `INSERT INTO catalog_releases(
       id, channel, catalog_id, catalog_version,
       manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id, created_by
     ) VALUES (
       $1, 'dev', 'dataminer-dev-release-test', $2,
       $3::bytea, $4::bytea, $5, 'dm-dev-test-key', 'release-test'
     )`,
    [id, version, manifest, envelope, hash(envelope)],
  );
  return id;
}

async function addEntries(releaseId: string, kinds = CORE_KINDS): Promise<void> {
  for (const kind of kinds) {
    await db.query(
      `INSERT INTO catalog_release_entries(
         release_id, trusted_catalog_version_id, kind, ordinal
       ) VALUES ($1, $2, $3::catalog_kind, 0)`,
      [releaseId, trustedIds[kind], kind],
    );
  }
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
     VALUES ('release-test-game', 'Release Test Game', 'game', 'first_person', 'rendered')`,
  );
  trustedIds = {};
  // 四本册子（docs/57 §3.1）。断代前这里是五个，capture 与 settings 各一。
  for (const kind of [...CORE_KINDS, ...DEPTH_KINDS]) {
    const id = randomUUID();
    const bytes = Buffer.from(`{"kind":"${kind}"}`, "utf8");
    const contentHash = hash(bytes);
    await db.query(
      `WITH trusted AS (
         INSERT INTO trusted_catalog_versions(
           id, kind, catalog_id, version, game_id, content_hash, snapshot
         ) VALUES (
           $1, $2::catalog_kind, $3, '1',
           CASE WHEN $2 = 'device_profile' THEN NULL ELSE 'release-test-game' END,
           $4, $5::jsonb
         )
         RETURNING id
       )
       INSERT INTO catalog_payloads(
         trusted_catalog_version_id, exact_bytes, byte_size, sha256
       )
       SELECT id, $6::bytea, $7, $4 FROM trusted`,
      [id, kind, `${kind}-release-test`, contentHash, bytes.toString("utf8"), bytes, bytes.byteLength],
    );
    trustedIds[kind] = id;
  }
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("immutable catalog release model", () => {
  // 断代前这里断言的是「catalog_id 与 catalog_mode 必须与渠道配对」——那条 CHECK
  // 把 id 钉死成 dataminer-dev-fleet / dataminer-certified-fleet 两个舰队常量。
  // 每游戏一份 bundle（docs/57 §3.3）之后 id 是自由的，那条 CHECK 随
  // catalog_mode 一起走了。仍然成立、也仍然值得钉住的是渠道本身的封闭词汇表。
  it("still pins the channel to a closed vocabulary", async () => {
    const envelope = Buffer.from('{"signed":"wrong-channel"}');
    await expect(
      db.query(
        `INSERT INTO catalog_releases(
           id, channel, catalog_id, catalog_version,
           manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id, created_by
         ) VALUES (
           $1, 'first_party', 'dataminer-dev-release-test', '2026-07-28.0',
           $2::bytea, $3::bytea, $4, 'test-key', 'release-test'
         )`,
        [randomUUID(), Buffer.from("{}"), envelope, hash(envelope)],
      ),
    ).rejects.toThrow();
  });

  it("atomically switches the one published current release per channel", async () => {
    const first = await createRelease("2026-07-28.1");
    await addEntries(first);
    await db.query(`SELECT publish_catalog_release($1, 'ops-a@example.com')`, [first]);

    const second = await createRelease("2026-07-28.2");
    await addEntries(second);
    await db.query(`SELECT publish_catalog_release($1, 'ops-b@example.com')`, [second]);

    const rows = await db.query<{
      id: string;
      status: string;
      published_by: string;
      superseded_at: Date | null;
    }>(
      `SELECT id, status, published_by, superseded_at
         FROM catalog_releases
        WHERE channel = 'dev'
        ORDER BY catalog_version`,
    );
    expect(rows.rows).toMatchObject([
      {
        id: first,
        status: "superseded",
        published_by: "ops-a@example.com",
      },
      {
        id: second,
        status: "published",
        published_by: "ops-b@example.com",
        superseded_at: null,
      },
    ]);
    expect(rows.rows[0]!.superseded_at).toBeInstanceOf(Date);

    const audit = await db.query(
      `SELECT action, entity_id, details
         FROM audit_events
        WHERE action = 'catalog.release_published'
        ORDER BY id`,
    );
    expect(audit.rows).toHaveLength(2);
    expect(audit.rows[1]).toMatchObject({
      entity_id: second,
      details: { previous_release_id: first, entry_count: 4 },
    });
  });

  it("rolls back a failed switch and leaves the previous current untouched", async () => {
    const current = await db.query<{ id: string }>(
      `SELECT id FROM catalog_releases WHERE channel = 'dev' AND status = 'published'`,
    );
    const incomplete = await createRelease("2026-07-28.3");
    await addEntries(
      incomplete,
      Object.keys(trustedIds).filter((kind) => kind !== "game_profile"),
    );

    await expect(
      db.query(`SELECT publish_catalog_release($1, 'ops@example.com')`, [incomplete]),
    ).rejects.toThrow(/all four profile kinds/);

    const after = await db.query<{ id: string }>(
      `SELECT id FROM catalog_releases WHERE channel = 'dev' AND status = 'published'`,
    );
    expect(after.rows).toEqual(current.rows);
    const failed = await db.query<{ status: string }>(
      `SELECT status FROM catalog_releases WHERE id = $1`,
      [incomplete],
    );
    expect(failed.rows[0]!.status).toBe("draft");
  });

  it("publishes the optional depth pair alongside all four core kinds", async () => {
    const release = await createRelease("2026-07-28.4");
    await addEntries(release, [...CORE_KINDS, ...DEPTH_KINDS]);

    await expect(
      db.query(`SELECT publish_catalog_release($1, 'depth-ops@example.com')`, [release]),
    ).resolves.toBeDefined();

    const rows = await db.query<{ kind: string }>(
      `SELECT kind::text AS kind
         FROM catalog_release_entries
        WHERE release_id = $1
        ORDER BY kind::text`,
      [release],
    );
    expect(rows.rows.map((row) => row.kind)).toEqual(
      [...CORE_KINDS, ...DEPTH_KINDS].sort(),
    );
  });

  it("rejects signed-byte changes and release roster mutation", async () => {
    const current = await db.query<{ id: string }>(
      `SELECT id FROM catalog_releases WHERE channel = 'dev' AND status = 'published'`,
    );
    const currentId = current.rows[0]!.id;

    await expect(
      db.query(`UPDATE catalog_releases SET envelope_bytes = $2::bytea WHERE id = $1`, [
        currentId,
        Buffer.from("changed"),
      ]),
    ).rejects.toThrow(/signed bytes are immutable/);
    await expect(
      db.query(
        `DELETE FROM catalog_release_entries
          WHERE release_id = $1 AND kind = 'game_profile'`,
        [currentId],
      ),
    ).rejects.toThrow(/entries are immutable/);
    await expect(
      db.query(`DELETE FROM catalog_releases WHERE id = $1`, [currentId]),
    ).rejects.toThrow(/releases are immutable/);
  });
});
